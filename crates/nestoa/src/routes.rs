use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use swc_common::BytePos;
use swc_ecma_ast::{
    Class, ClassMember, Expr, ExprOrSpread, Lit, MethodKind, Param, Pat, PropName, TsEntityName,
    TsType, TsTypeRef,
};

use crate::config::{Config, DecoratorAlias};
use crate::jsdoc::{jsdoc_before, JsDoc};
use crate::schema::{decorator_name, ComponentSchemas, SchemaCtx};
use crate::symbols::Project;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    All,
}

impl HttpMethod {
    pub fn as_lower(self) -> &'static str {
        match self {
            HttpMethod::Get => "get",
            HttpMethod::Post => "post",
            HttpMethod::Put => "put",
            HttpMethod::Patch => "patch",
            HttpMethod::Delete => "delete",
            HttpMethod::Head => "head",
            HttpMethod::Options => "options",
            HttpMethod::All => "all",
        }
    }
    fn from_decorator(name: &str) -> Option<HttpMethod> {
        Some(match name {
            "Get" => HttpMethod::Get,
            "Post" => HttpMethod::Post,
            "Put" => HttpMethod::Put,
            "Patch" => HttpMethod::Patch,
            "Delete" => HttpMethod::Delete,
            "Head" => HttpMethod::Head,
            "Options" => HttpMethod::Options,
            "All" => HttpMethod::All,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Route {
    pub method: HttpMethod,
    pub path: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub deprecated: bool,
    pub parameters: Vec<RouteParam>,
    pub request_body: Option<RequestBody>,
    pub response: Option<Value>,
    pub status_code: u16,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RouteParam {
    pub name: String,
    pub location: ParamLocation,
    pub required: bool,
    pub schema: Value,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamLocation {
    Path,
    Query,
    Header,
}

#[derive(Debug, Clone)]
pub struct RequestBody {
    pub schema: Value,
    pub required: bool,
    pub description: Option<String>,
}

pub struct Extractor<'a> {
    pub config: &'a Config,
    pub project: &'a Project,
    pub sources: &'a HashMap<PathBuf, String>,
    pub file_starts: &'a HashMap<PathBuf, BytePos>,
    pub components: &'a mut ComponentSchemas,
}

impl<'a> Extractor<'a> {
    pub fn extract_all(&mut self) -> Vec<Route> {
        let mut routes = Vec::new();
        let files: Vec<PathBuf> = self.project.files.keys().cloned().collect();
        for file in &files {
            let parsed = &self.project.files[file];
            for item in &parsed.module.body {
                let class_decl = match item {
                    swc_ecma_ast::ModuleItem::ModuleDecl(
                        swc_ecma_ast::ModuleDecl::ExportDecl(e),
                    ) => match &e.decl {
                        swc_ecma_ast::Decl::Class(c) => Some(c),
                        _ => None,
                    },
                    swc_ecma_ast::ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(
                        swc_ecma_ast::Decl::Class(c),
                    )) => Some(c),
                    _ => None,
                };
                if let Some(cd) = class_decl {
                    if let Some(base_path) = controller_base_path(&cd.class) {
                        let class_tag = strip_suffix_ci(&cd.ident.sym, "Controller").to_string();
                        self.extract_class(file, &cd.class, &base_path, &class_tag, &mut routes);
                    }
                }
            }
        }
        routes
    }

    fn extract_class(
        &mut self,
        file: &Path,
        class: &Class,
        base_path: &str,
        tag: &str,
        out: &mut Vec<Route>,
    ) {
        for member in &class.body {
            let ClassMember::Method(m) = member else { continue };
            if !matches!(m.kind, MethodKind::Method) {
                continue;
            }
            // Find HTTP verb decorator.
            let mut http: Option<(HttpMethod, Option<String>)> = None;
            let mut deprecated = false;
            let mut http_code: Option<u16> = None;
            for d in &m.function.decorators {
                let name = decorator_name(d);
                if let Some(method) = HttpMethod::from_decorator(&name) {
                    let path = path_arg(d);
                    http = Some((method, path));
                    continue;
                }
                if name == "HttpCode" {
                    if let Some(p) = numeric_arg(d) {
                        http_code = Some(p);
                    }
                }
            }
            let Some((method, sub_path)) = http else {
                continue;
            };
            let full_path = join_paths(self.config.base_path.as_deref(), base_path, sub_path.as_deref());

            // JSDoc on the method
            let mut method_doc = JsDoc::default();
            if let (Some(src), Some(start)) =
                (self.sources.get(file), self.file_starts.get(file))
            {
                if let Some(doc) = jsdoc_before(src, *start, m.span) {
                    method_doc = doc;
                }
            }
            if method_doc.deprecated {
                deprecated = true;
            }

            // Parameters
            let mut params: Vec<RouteParam> = Vec::new();
            let mut request_body: Option<RequestBody> = None;
            for p in &m.function.params {
                if let Some(parsed) = self.classify_param(file, p, &method_doc) {
                    match parsed {
                        ParsedParam::Routed(rp) => params.push(rp),
                        ParsedParam::Body(b) => request_body = Some(b),
                        ParsedParam::Ignored => {}
                    }
                }
            }

            // Response type
            let mut response_value: Option<Value> = None;
            if let Some(ret) = m.function.return_type.as_ref() {
                let mut ctx = SchemaCtx {
                    project: self.project,
                    sources: self.sources,
                    file_starts: self.file_starts,
                    components: self.components,
                };
                let schema = ctx.ts_type_to_schema(file, &ret.type_ann);
                if !schema.is_null() && schema != json!({}) {
                    response_value = Some(schema);
                }
            }

            let status_code = http_code.unwrap_or(match method {
                HttpMethod::Post => 201,
                HttpMethod::Delete => 204,
                _ => 200,
            });

            out.push(Route {
                method,
                path: full_path,
                summary: method_doc.summary.clone(),
                description: method_doc.description.clone(),
                deprecated,
                parameters: params,
                request_body,
                response: response_value,
                status_code,
                tags: vec![tag.to_string()],
            });
        }
    }

    fn classify_param(
        &mut self,
        file: &Path,
        p: &Param,
        method_doc: &JsDoc,
    ) -> Option<ParsedParam> {
        // Find the first known decorator on the parameter.
        let mut location: Option<DecoratorKind> = None;
        let mut decorator_arg: Option<String> = None;
        for d in &p.decorators {
            let name = decorator_name(d);
            let kind = match name.as_str() {
                "Param" => DecoratorKind::Param,
                "Query" => DecoratorKind::Query,
                "Body" => DecoratorKind::Body,
                "Headers" => DecoratorKind::Header,
                other => match self.config.decorator_aliases.get(other) {
                    Some(DecoratorAlias::Param) => DecoratorKind::Param,
                    Some(DecoratorAlias::Query) => DecoratorKind::Query,
                    Some(DecoratorAlias::Body) => DecoratorKind::Body,
                    Some(DecoratorAlias::Header) => DecoratorKind::Header,
                    Some(DecoratorAlias::User) | Some(DecoratorAlias::Ignore) => {
                        return Some(ParsedParam::Ignored);
                    }
                    None => continue,
                },
            };
            location = Some(kind);
            decorator_arg = string_arg(d);
            break;
        }
        let location = location?;

        let pat_info = extract_pat(&p.pat);
        let name = decorator_arg.unwrap_or_else(|| pat_info.name.clone());
        let mut ctx = SchemaCtx {
            project: self.project,
            sources: self.sources,
            file_starts: self.file_starts,
            components: self.components,
        };
        let schema = pat_info
            .ts_type
            .as_ref()
            .map(|t| ctx.ts_type_to_schema(file, t))
            .unwrap_or_else(|| json!({}));
        drop(ctx);
        let description = method_doc.params.get(&pat_info.name).cloned();
        match location {
            DecoratorKind::Body => Some(ParsedParam::Body(RequestBody {
                schema,
                required: !pat_info.optional,
                description,
            })),
            DecoratorKind::Param => Some(ParsedParam::Routed(RouteParam {
                name,
                location: ParamLocation::Path,
                required: true, // path params are always required
                schema,
                description,
            })),
            DecoratorKind::Query => Some(ParsedParam::Routed(RouteParam {
                name,
                location: ParamLocation::Query,
                required: !pat_info.optional,
                schema,
                description,
            })),
            DecoratorKind::Header => Some(ParsedParam::Routed(RouteParam {
                name,
                location: ParamLocation::Header,
                required: !pat_info.optional,
                schema,
                description,
            })),
        }
    }
}

enum ParsedParam {
    Routed(RouteParam),
    Body(RequestBody),
    Ignored,
}

#[derive(Clone, Copy)]
enum DecoratorKind {
    Param,
    Query,
    Body,
    Header,
}

struct PatInfo {
    name: String,
    optional: bool,
    ts_type: Option<TsType>,
}

fn extract_pat(pat: &Pat) -> PatInfo {
    match pat {
        Pat::Ident(b) => PatInfo {
            name: b.id.sym.to_string(),
            optional: b.id.optional,
            ts_type: b.type_ann.as_ref().map(|t| (*t.type_ann).clone()),
        },
        Pat::Assign(a) => {
            // `foo = default` form
            let mut inner = extract_pat(&a.left);
            inner.optional = true;
            inner
        }
        _ => PatInfo {
            name: "arg".to_string(),
            optional: false,
            ts_type: None,
        },
    }
}

fn controller_base_path(class: &Class) -> Option<String> {
    for d in &class.decorators {
        if decorator_name(d) == "Controller" {
            return Some(path_arg(d).unwrap_or_default());
        }
    }
    None
}

fn path_arg(d: &swc_ecma_ast::Decorator) -> Option<String> {
    let call = match &*d.expr {
        Expr::Call(c) => c,
        _ => return None,
    };
    let first = call.args.first()?;
    match first {
        ExprOrSpread { expr, .. } => match expr.as_ref() {
            Expr::Lit(Lit::Str(s)) => Some(s.value.to_string_lossy().into_owned()),
            Expr::Object(o) => {
                // Support `@Controller({ path: 'foo' })`
                for prop in &o.props {
                    if let swc_ecma_ast::PropOrSpread::Prop(p) = prop {
                        if let swc_ecma_ast::Prop::KeyValue(kv) = p.as_ref() {
                            let k = match &kv.key {
                                PropName::Ident(i) => i.sym.to_string(),
                                PropName::Str(s) => s.value.to_string_lossy().into_owned(),
                                _ => continue,
                            };
                            if k == "path" {
                                if let Expr::Lit(Lit::Str(s)) = kv.value.as_ref() {
                                    return Some(s.value.to_string_lossy().into_owned());
                                }
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        },
    }
}

fn numeric_arg(d: &swc_ecma_ast::Decorator) -> Option<u16> {
    let call = match &*d.expr {
        Expr::Call(c) => c,
        _ => return None,
    };
    let first = call.args.first()?;
    if let Expr::Lit(Lit::Num(n)) = first.expr.as_ref() {
        return Some(n.value as u16);
    }
    None
}

fn string_arg(d: &swc_ecma_ast::Decorator) -> Option<String> {
    let call = match &*d.expr {
        Expr::Call(c) => c,
        _ => return None,
    };
    let first = call.args.first()?;
    if let Expr::Lit(Lit::Str(s)) = first.expr.as_ref() {
        return Some(s.value.to_string_lossy().into_owned());
    }
    None
}

fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> &'a str {
    if s.len() >= suffix.len()
        && s[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    {
        &s[..s.len() - suffix.len()]
    } else {
        s
    }
}

fn join_paths(prefix: Option<&str>, base: &str, sub: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(p) = prefix {
        out.push_str(&normalize_segment(p));
    }
    out.push_str(&normalize_segment(base));
    if let Some(s) = sub {
        out.push_str(&normalize_segment(s));
    }
    if out.is_empty() {
        return "/".to_string();
    }
    // Convert :param to {param} for OpenAPI.
    let mut formatted = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' {
            let mut name = String::new();
            while let Some(&next) = chars.peek() {
                if next.is_ascii_alphanumeric() || next == '_' {
                    name.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            formatted.push('{');
            formatted.push_str(&name);
            formatted.push('}');
        } else {
            formatted.push(c);
        }
    }
    formatted
}

fn normalize_segment(s: &str) -> String {
    let s = s.trim().trim_matches('/');
    if s.is_empty() {
        "".to_string()
    } else {
        format!("/{}", s)
    }
}

/// Convert routes + components into an OpenAPI 3.1 document.
pub fn to_openapi(config: &Config, routes: &[Route], components: ComponentSchemas) -> Value {
    let mut paths: IndexMap<String, Map<String, Value>> = IndexMap::new();
    for r in routes {
        let entry = paths.entry(r.path.clone()).or_default();
        let mut op = Map::new();
        if let Some(s) = &r.summary {
            op.insert("summary".into(), Value::String(s.clone()));
        }
        if let Some(d) = &r.description {
            op.insert("description".into(), Value::String(d.clone()));
        }
        if r.deprecated {
            op.insert("deprecated".into(), Value::Bool(true));
        }
        if !r.tags.is_empty() {
            op.insert(
                "tags".into(),
                Value::Array(r.tags.iter().cloned().map(Value::String).collect()),
            );
        }
        // Parameters
        if !r.parameters.is_empty() {
            let arr: Vec<Value> = r
                .parameters
                .iter()
                .map(|p| {
                    let mut m = Map::new();
                    m.insert("name".into(), Value::String(p.name.clone()));
                    m.insert(
                        "in".into(),
                        Value::String(
                            match p.location {
                                ParamLocation::Path => "path",
                                ParamLocation::Query => "query",
                                ParamLocation::Header => "header",
                            }
                            .into(),
                        ),
                    );
                    m.insert("required".into(), Value::Bool(p.required));
                    if let Some(d) = &p.description {
                        m.insert("description".into(), Value::String(d.clone()));
                    }
                    m.insert("schema".into(), p.schema.clone());
                    Value::Object(m)
                })
                .collect();
            op.insert("parameters".into(), Value::Array(arr));
        }
        // Request body
        if let Some(body) = &r.request_body {
            let mut content = Map::new();
            let mut media = Map::new();
            media.insert("schema".into(), body.schema.clone());
            content.insert("application/json".into(), Value::Object(media));
            let mut rb = Map::new();
            rb.insert("required".into(), Value::Bool(body.required));
            if let Some(d) = &body.description {
                rb.insert("description".into(), Value::String(d.clone()));
            }
            rb.insert("content".into(), Value::Object(content));
            op.insert("requestBody".into(), Value::Object(rb));
        }
        // Responses
        let mut responses = Map::new();
        let mut status = Map::new();
        status.insert(
            "description".into(),
            Value::String(default_status_text(r.status_code).to_string()),
        );
        if let Some(schema) = &r.response {
            let mut content = Map::new();
            let mut media = Map::new();
            media.insert("schema".into(), schema.clone());
            content.insert("application/json".into(), Value::Object(media));
            status.insert("content".into(), Value::Object(content));
        }
        responses.insert(r.status_code.to_string(), Value::Object(status));
        op.insert("responses".into(), Value::Object(responses));
        entry.insert(r.method.as_lower().to_string(), Value::Object(op));
    }

    let mut paths_json: Map<String, Value> = Map::new();
    for (p, ops) in paths {
        paths_json.insert(p, Value::Object(ops));
    }

    let mut info = Map::new();
    info.insert("title".into(), Value::String(config.info.title.clone()));
    info.insert("version".into(), Value::String(config.info.version.clone()));
    if let Some(d) = &config.info.description {
        info.insert("description".into(), Value::String(d.clone()));
    }

    let mut root = Map::new();
    root.insert("openapi".into(), Value::String("3.1.0".into()));
    root.insert("info".into(), Value::Object(info));
    root.insert("paths".into(), Value::Object(paths_json));
    if !components.schemas.is_empty() {
        let schemas: Map<String, Value> = components.schemas.into_iter().collect();
        let mut comp = Map::new();
        comp.insert("schemas".into(), Value::Object(schemas));
        root.insert("components".into(), Value::Object(comp));
    }
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_paths_and_converts_params() {
        assert_eq!(join_paths(None, "users", Some(":id")), "/users/{id}");
        assert_eq!(
            join_paths(Some("/api"), "v1/users", Some("")),
            "/api/v1/users"
        );
        assert_eq!(join_paths(None, "", None), "/");
        assert_eq!(
            join_paths(None, "files", Some(":fileId/raw")),
            "/files/{fileId}/raw"
        );
    }

    #[test]
    fn strips_controller_suffix() {
        assert_eq!(strip_suffix_ci("AccountController", "Controller"), "Account");
        assert_eq!(strip_suffix_ci("Account", "Controller"), "Account");
    }
}

fn default_status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "Response",
    }
}

// Avoid dead-code warning on TsTypeRef helper used elsewhere.
#[allow(dead_code)]
fn _silence_unused(_: &TsEntityName, _: &TsTypeRef) {}
