use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use swc_common::BytePos;
use swc_ecma_ast::{
    Class, ClassMember, Expr, ExprOrSpread, Lit, MethodKind, Param, Pat, PropName, TsEntityName,
    TsType, TsTypeRef,
};

use crate::config::{Config, DecoratorAlias};
use crate::interceptors::{
    interceptor_class_names, request_body_transform, response_override, rewrite_response,
};
use crate::jsdoc::{JsDoc, jsdoc_before};
use crate::schema::{ComponentSchemas, SchemaCtx, decorator_name};
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
    /// The originally-discovered source files. Controllers are only looked for
    /// here, so package declarations pulled into the symbol table for type
    /// resolution cannot introduce phantom routes.
    pub entry_files: &'a [PathBuf],
    pub sources: &'a HashMap<PathBuf, String>,
    pub file_starts: &'a HashMap<PathBuf, BytePos>,
    pub components: &'a mut ComponentSchemas,
}

impl<'a> Extractor<'a> {
    pub fn extract_all(&mut self) -> Vec<Route> {
        let mut routes = Vec::new();
        let files: Vec<PathBuf> = self.entry_files.to_vec();
        for file in &files {
            if !self.project.files.contains_key(file) {
                continue;
            }
            let parsed = &self.project.files[file];
            for item in &parsed.module.body {
                let class_decl = match item {
                    swc_ecma_ast::ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportDecl(
                        e,
                    )) => match &e.decl {
                        swc_ecma_ast::Decl::Class(c) => Some(c),
                        _ => None,
                    },
                    swc_ecma_ast::ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(
                        swc_ecma_ast::Decl::Class(c),
                    )) => Some(c),
                    _ => None,
                };
                if let Some(cd) = class_decl
                    && let Some(base_paths) = controller_base_paths(&cd.class)
                {
                    let class_tag = strip_suffix_ci(&cd.ident.sym, "Controller").to_string();
                    self.extract_class(file, &cd.class, &base_paths, &class_tag, &mut routes);
                }
            }
        }
        routes
    }

    fn extract_class(
        &mut self,
        file: &Path,
        class: &Class,
        base_paths: &[String],
        tag: &str,
        out: &mut Vec<Route>,
    ) {
        // `@UseInterceptors(...)` applied at the controller level — these run
        // before any method-level ones, so they lead the combined list.
        let class_interceptors = collect_interceptors(&class.decorators);
        for member in &class.body {
            let ClassMember::Method(m) = member else {
                continue;
            };
            if !matches!(m.kind, MethodKind::Method) {
                continue;
            }
            // Find HTTP verb decorator.
            let mut http: Option<(HttpMethod, Vec<String>)> = None;
            let mut deprecated = false;
            let mut http_code: Option<u16> = None;
            for d in &m.function.decorators {
                let name = decorator_name(d);
                if let Some(method) = HttpMethod::from_decorator(&name) {
                    let paths = path_args(d);
                    http = Some((method, paths));
                    continue;
                }
                if name == "HttpCode"
                    && let Some(p) = numeric_arg(d)
                {
                    http_code = Some(p);
                }
            }
            let Some((method, sub_paths)) = http else {
                continue;
            };
            let mut interceptor_names = class_interceptors.clone();
            interceptor_names.extend(collect_interceptors(&m.function.decorators));
            // A controller and/or a route may declare multiple paths; the
            // effective routes are the cross product. A route with no path
            // argument contributes a single `None` sub-path.
            let sub_opts: Vec<Option<&str>> = if sub_paths.is_empty() {
                vec![None]
            } else {
                sub_paths.iter().map(|s| Some(s.as_str())).collect()
            };
            let mut full_paths: Vec<String> = Vec::with_capacity(base_paths.len() * sub_opts.len());
            for base in base_paths {
                for sub in &sub_opts {
                    full_paths.push(join_paths(self.config.base_path.as_deref(), base, *sub));
                }
            }

            // JSDoc on the method
            let mut method_doc = JsDoc::default();
            if let (Some(src), Some(start)) = (self.sources.get(file), self.file_starts.get(file))
                && let Some(doc) = jsdoc_before(src, *start, m.span)
            {
                method_doc = doc;
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
            // Interceptors may accept a request body in a different shape than
            // the handler's `@Body` parameter (e.g. single-object-or-array).
            if let Some(body) = request_body.as_mut() {
                request_body_transform(self.config, &interceptor_names, body);
            }

            // Response type. An interceptor may reshape the wire response (e.g.
            // pagination); when it does, the rewritten schema replaces the
            // declared return type — and we skip lowering the declared type at
            // all so its now-unreferenced components don't leak into the doc.
            let mut response_value: Option<Value> = None;
            if let Some(ret) = m.function.return_type.as_ref() {
                let rewritten = if interceptor_names.is_empty() {
                    None
                } else {
                    rewrite_response(
                        self.project,
                        self.config,
                        file,
                        &ret.type_ann,
                        &interceptor_names,
                        self.components,
                        self.sources,
                        self.file_starts,
                    )
                };
                if rewritten.is_some() {
                    response_value = rewritten;
                } else {
                    let mut ctx = SchemaCtx {
                        project: self.project,
                        sources: self.sources,
                        file_starts: self.file_starts,
                        components: self.components,
                        type_env: None,
                    };
                    let schema = ctx.ts_type_to_schema(file, &ret.type_ann);
                    if !schema.is_null() && schema != json!({}) {
                        response_value = Some(schema);
                    }
                }
            }

            let status_code = http_code.unwrap_or(match method {
                HttpMethod::Post => 201,
                HttpMethod::Delete => 204,
                _ => 200,
            });

            // Per-path response is filled in below: an interceptor may override
            // it for only some of the route's sibling paths (e.g. a signing
            // interceptor that rewrites the body solely on the `/jws` variant).
            let template = Route {
                method,
                path: String::new(),
                summary: method_doc.summary.clone(),
                description: method_doc.description.clone(),
                deprecated,
                parameters: params,
                request_body,
                response: None,
                status_code,
                tags: vec![tag.to_string()],
            };
            for full_path in full_paths {
                let response = response_override(self.config, &interceptor_names, &full_path)
                    .or_else(|| response_value.clone());
                out.push(Route {
                    path: full_path,
                    response,
                    ..template.clone()
                });
            }
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
            type_env: None,
        };
        let schema = pat_info
            .ts_type
            .as_ref()
            .map(|t| ctx.ts_type_to_schema(file, t))
            .unwrap_or_else(|| json!({}));
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

/// Collect interceptor class names from any `@UseInterceptors(...)` decorators
/// in the given list, in source order.
fn collect_interceptors(decorators: &[swc_ecma_ast::Decorator]) -> Vec<String> {
    let mut names = Vec::new();
    for d in decorators {
        if decorator_name(d) == "UseInterceptors" {
            names.extend(interceptor_class_names(d));
        }
    }
    names
}

/// Returns the controller's base path(s), or `None` if the class is not a
/// `@Controller`. A bare `@Controller()` yields a single empty path. NestJS
/// allows an array of paths (`@Controller(['a', 'b'])`), each of which becomes
/// a distinct mount point.
fn controller_base_paths(class: &Class) -> Option<Vec<String>> {
    for d in &class.decorators {
        if decorator_name(d) == "Controller" {
            let paths = path_args(d);
            return Some(if paths.is_empty() {
                vec![String::new()]
            } else {
                paths
            });
        }
    }
    None
}

/// Extracts the path(s) from a decorator's first argument. Handles a string
/// literal, an array of string literals (NestJS multi-path form), and the
/// `@Controller({ path: ... })` object form (where `path` may itself be a
/// string or an array). Non-literal entries are skipped.
fn path_args(d: &swc_ecma_ast::Decorator) -> Vec<String> {
    let call = match &*d.expr {
        Expr::Call(c) => c,
        _ => return Vec::new(),
    };
    match call.args.first() {
        Some(ExprOrSpread { expr, .. }) => expr_to_paths(expr.as_ref()),
        None => Vec::new(),
    }
}

fn expr_to_paths(expr: &Expr) -> Vec<String> {
    match expr {
        Expr::Lit(Lit::Str(s)) => vec![s.value.to_string_lossy().into_owned()],
        Expr::Array(arr) => arr
            .elems
            .iter()
            .flatten()
            .filter(|el| el.spread.is_none())
            .filter_map(|el| match el.expr.as_ref() {
                Expr::Lit(Lit::Str(s)) => Some(s.value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect(),
        Expr::Object(o) => {
            // Support `@Controller({ path: 'foo' })` (path may be an array too).
            for prop in &o.props {
                if let swc_ecma_ast::PropOrSpread::Prop(p) = prop
                    && let swc_ecma_ast::Prop::KeyValue(kv) = p.as_ref()
                {
                    let k = match &kv.key {
                        PropName::Ident(i) => i.sym.to_string(),
                        PropName::Str(s) => s.value.to_string_lossy().into_owned(),
                        _ => continue,
                    };
                    if k == "path" {
                        return expr_to_paths(kv.value.as_ref());
                    }
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
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
    if s.len() >= suffix.len() && s[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix) {
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

    // Sort paths, and the HTTP methods within each path, alphabetically. Routes
    // are collected in source/file order, so without this the output would drift
    // when the codebase is refactored (files moved, controllers reordered) even
    // though the API itself is unchanged. Sorting keeps the document idempotent.
    let mut sorted_paths: Vec<(String, Map<String, Value>)> = paths.into_iter().collect();
    sorted_paths.sort_by(|a, b| a.0.cmp(&b.0));

    let mut paths_json: Map<String, Value> = Map::new();
    for (p, ops) in sorted_paths {
        let mut sorted_ops: Vec<(String, Value)> = ops.into_iter().collect();
        sorted_ops.sort_by(|a, b| a.0.cmp(&b.0));
        paths_json.insert(p, Value::Object(sorted_ops.into_iter().collect()));
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
        // Schemas are collected in type-encounter order, which also follows file
        // order; sort them for the same refactor-stability reason as paths.
        let mut schema_pairs: Vec<(String, Value)> = components.schemas.into_iter().collect();
        schema_pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let schemas: Map<String, Value> = schema_pairs.into_iter().collect();
        let mut comp = Map::new();
        comp.insert("schemas".into(), Value::Object(schemas));
        root.insert("components".into(), Value::Object(comp));
    }
    Value::Object(root)
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
        assert_eq!(
            strip_suffix_ci("AccountController", "Controller"),
            "Account"
        );
        assert_eq!(strip_suffix_ci("Account", "Controller"), "Account");
    }

    /// Build a `Project` from one in-memory controller file and extract routes.
    fn extract_paths(name: &str, body: &str) -> Vec<(HttpMethod, String)> {
        use crate::config::Config;
        use crate::parse::ParsingContext;
        use crate::schema::ComponentSchemas;

        let dir =
            std::env::temp_dir().join(format!("nestoa-routes-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("ctrl.ts");
        std::fs::write(&path, body).unwrap();

        let parser = ParsingContext::default();
        let mut project = Project::default();
        project.add(parser.parse_file(&path).unwrap());

        let config = Config::default();
        let entry = vec![path];
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let mut components = ComponentSchemas::default();
        let routes = {
            let mut ex = Extractor {
                config: &config,
                project: &project,
                entry_files: &entry,
                sources: &sources,
                file_starts: &file_starts,
                components: &mut components,
            };
            ex.extract_all()
        };
        routes.into_iter().map(|r| (r.method, r.path)).collect()
    }

    #[test]
    fn expands_multi_path_controller_and_route_cross_product() {
        let routes = extract_paths(
            "multi",
            r#"
            @Controller([
                'organizations/:organizationId/service-accounts',
                'organizations/:organizationId/projects/:projectId/service-accounts',
            ])
            export class ServiceAccountsController {
                @Get([':analysisId/incomes', ':analysisId/expenses'])
                findAll() {}
            }
            "#,
        );
        let paths: Vec<String> = routes.iter().map(|(_, p)| p.clone()).collect();
        assert_eq!(paths.len(), 4, "2 base paths x 2 sub paths");
        assert!(paths.contains(
            &"/organizations/{organizationId}/service-accounts/{analysisId}/incomes".to_string()
        ));
        assert!(paths.contains(
            &"/organizations/{organizationId}/service-accounts/{analysisId}/expenses".to_string()
        ));
        assert!(paths.contains(&"/organizations/{organizationId}/projects/{projectId}/service-accounts/{analysisId}/incomes".to_string()));
        assert!(paths.contains(&"/organizations/{organizationId}/projects/{projectId}/service-accounts/{analysisId}/expenses".to_string()));
    }

    #[test]
    fn single_path_controller_still_yields_one_route() {
        let routes = extract_paths(
            "single",
            r#"
            @Controller('users')
            export class UsersController {
                @Get(':id')
                findOne() {}
            }
            "#,
        );
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].1, "/users/{id}");
    }

    /// Minimal route with a 200 status and no body/params, for output-ordering tests.
    fn route(method: HttpMethod, path: &str) -> Route {
        Route {
            method,
            path: path.to_string(),
            summary: None,
            description: None,
            deprecated: false,
            parameters: vec![],
            request_body: None,
            response: None,
            status_code: 200,
            tags: vec![],
        }
    }

    #[test]
    fn to_openapi_sorts_paths_methods_and_schemas() {
        use crate::schema::ComponentSchemas;
        use serde_json::json;

        // Feed routes, methods, and schemas in deliberately non-alphabetical order.
        let routes = vec![
            route(HttpMethod::Post, "/users"),
            route(HttpMethod::Get, "/users"),
            route(HttpMethod::Get, "/accounts"),
        ];
        let mut components = ComponentSchemas::default();
        components.insert("Zebra", json!({"type": "object"}));
        components.insert("Apple", json!({"type": "object"}));
        components.insert("Mango", json!({"type": "object"}));

        let doc = to_openapi(&Config::default(), &routes, components);

        // Paths are alphabetical.
        let path_keys: Vec<&String> = doc["paths"].as_object().unwrap().keys().collect();
        assert_eq!(path_keys, vec!["/accounts", "/users"]);

        // Methods within a path are alphabetical (get before post).
        let method_keys: Vec<&String> = doc["paths"]["/users"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(method_keys, vec!["get", "post"]);

        // Component schemas are alphabetical.
        let schema_keys: Vec<&String> = doc["components"]["schemas"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(schema_keys, vec!["Apple", "Mango", "Zebra"]);
    }

    #[test]
    fn to_openapi_is_idempotent_under_input_reordering() {
        use crate::schema::ComponentSchemas;
        use serde_json::json;

        // Build a document from routes and schemas supplied in arbitrary order.
        let build = |routes: &[Route], schema_order: &[&str]| {
            let mut components = ComponentSchemas::default();
            for name in schema_order {
                components.insert(name, json!({"type": "object"}));
            }
            to_openapi(&Config::default(), routes, components)
        };

        let forward = vec![
            route(HttpMethod::Get, "/a"),
            route(HttpMethod::Post, "/a"),
            route(HttpMethod::Get, "/b"),
            route(HttpMethod::Delete, "/b"),
        ];
        let mut shuffled = forward.clone();
        shuffled.reverse();

        // Same API surface, different route AND schema declaration order ->
        // byte-identical output. This is the refactor-stability guarantee.
        let a = serde_json::to_string(&build(&forward, &["Beta", "Alpha", "Gamma"])).unwrap();
        let b = serde_json::to_string(&build(&shuffled, &["Gamma", "Beta", "Alpha"])).unwrap();
        assert_eq!(a, b);
    }
}
