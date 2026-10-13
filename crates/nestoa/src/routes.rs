use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use swc_ecma_ast::{
    Callee, Class, ClassMember, Decorator, Expr, ExprOrSpread, Function, Lit, MethodKind, Param,
    Pat, PropName, TsEntityName, TsType, TsTypeRef,
};
use swc_ecma_visit::{Visit, VisitWith};

use crate::config::{AuthScope, Config, DecoratorAlias, OpenApiVersion};
use crate::dialect;
use crate::errors::{self, ERROR_SCHEMA_NAME};
use crate::interceptors::{
    interceptor_class_names, request_body_transform, response_override, rewrite_response,
};
use crate::jsdoc::JsDoc;
use crate::schema::{ComponentSchemas, SchemaCtx, decorator_name, describe};
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
    /// Reachable only through `@All`: NestJS has no `@Trace()` decorator.
    Trace,
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
            HttpMethod::Trace => "trace",
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
            _ => return None,
        })
    }
}

/// The operations an `@All` handler answers: the router's `all` route matches
/// every method, TRACE included.
const ALL_METHODS: [HttpMethod; 8] = [
    HttpMethod::Get,
    HttpMethod::Post,
    HttpMethod::Put,
    HttpMethod::Patch,
    HttpMethod::Delete,
    HttpMethod::Head,
    HttpMethod::Options,
    HttpMethod::Trace,
];

/// A handler's HTTP-verb decorator: a single method, or `@All`.
#[derive(Clone, Copy)]
enum VerbDecorator {
    One(HttpMethod),
    All,
}

impl VerbDecorator {
    fn from_name(name: &str) -> Option<Self> {
        if name == "All" {
            return Some(Self::All);
        }
        HttpMethod::from_decorator(name).map(Self::One)
    }

    fn methods(&self) -> &[HttpMethod] {
        match self {
            Self::One(m) => std::slice::from_ref(m),
            Self::All => &ALL_METHODS,
        }
    }

    /// NestJS's status when no `@HttpCode` is set: 201 for `@Post`, 200 for
    /// everything else — `@All` included, even when it answers a POST.
    fn default_status(self) -> u16 {
        match self {
            Self::One(HttpMethod::Post) => 201,
            _ => 200,
        }
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
    /// Error responses (`status -> description`), reconstructed from guards,
    /// pipes, validation, and declared throws. Every one references the shared
    /// `Error` component. Sorted by status for deterministic output.
    pub errors: BTreeMap<u16, String>,
    /// Registered by `@All` rather than an explicit verb decorator. Between
    /// the two, whichever was registered first serves the method.
    pub catch_all: bool,
    /// `Controller.method`, for diagnostics.
    pub handler: String,
    /// The file declaring the handler, which tells apart two controllers of
    /// the same name.
    pub file: PathBuf,
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
                use swc_ecma_ast::{Decl, DefaultDecl, ModuleDecl, ModuleItem, Stmt};
                let (ident, class) = match item {
                    ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => match &e.decl {
                        Decl::Class(c) => (&c.ident, &*c.class),
                        _ => continue,
                    },
                    ModuleItem::Stmt(Stmt::Decl(Decl::Class(c))) => (&c.ident, &*c.class),
                    ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(e)) => match &e.decl {
                        DefaultDecl::Class(c) => match &c.ident {
                            Some(ident) => (ident, &*c.class),
                            None => continue,
                        },
                        _ => continue,
                    },
                    _ => continue,
                };
                if let Some(base_paths) = controller_base_paths(class) {
                    let class_tag = strip_suffix_ci(&ident.sym, "Controller").to_string();
                    self.extract_class(
                        file,
                        &ident.sym,
                        class,
                        &base_paths,
                        &class_tag,
                        &mut routes,
                    );
                }
            }
        }
        routes
    }

    fn extract_class(
        &mut self,
        file: &Path,
        class_name: &str,
        class: &Class,
        base_paths: &[String],
        tag: &str,
        out: &mut Vec<Route>,
    ) {
        // `@UseInterceptors(...)` applied at the controller level — these run
        // before any method-level ones, so they lead the combined list.
        let class_interceptors = collect_interceptors(&class.decorators);
        // Controller-level error signals: a decorator on the class applies to
        // every method (auth opt-out, roles, and — in per-controller auth mode —
        // an `@UseGuards(AuthGuard)`). Computed to plain bools so no borrow of
        // `self.config` is held across the `&mut self` calls in the loop below.
        let er_cfg = &self.config.error_responses;
        let class_opted_out = has_any_decorator(&class.decorators, &er_cfg.auth_opt_out);
        let class_has_roles = has_any_decorator(&class.decorators, &er_cfg.role_decorators);
        let class_guarded = has_use_guards(&class.decorators, &er_cfg.auth_guards);
        for member in &class.body {
            let ClassMember::Method(m) = member else {
                continue;
            };
            if !matches!(m.kind, MethodKind::Method) {
                continue;
            }
            // Decorators apply bottom-up, so with several verb decorators the
            // topmost one is applied last and is the one NestJS registers.
            let mut http: Option<(VerbDecorator, Vec<String>)> = None;
            let mut deprecated = false;
            let mut http_code: Option<u16> = None;
            for d in &m.function.decorators {
                let name = decorator_name(d);
                if let Some(verb) = VerbDecorator::from_name(&name) {
                    if http.is_none() {
                        http = Some((verb, path_args(d)));
                    }
                    continue;
                }
                if name == "HttpCode" {
                    // `@HttpCode(207)` — a literal — or `@HttpCode(MULTI_STATUS)`,
                    // where the argument names a numeric `const` (possibly
                    // imported from another module, e.g. `@algoan/multi-status`).
                    http_code = numeric_arg(d)
                        .or_else(|| {
                            ident_arg(d)
                                .and_then(|c| self.project.resolve_const_number(file, &c))
                                .map(|v| v as u16)
                        })
                        .or_else(|| {
                            member_arg(d)
                                .and_then(|(obj, prop)| self.member_status(file, &obj, &prop))
                        });
                }
            }
            let Some((verb, sub_paths)) = http else {
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
            if let Some(doc) = self
                .project
                .files
                .get(file)
                .and_then(|f| f.docs.jsdoc(m.span.lo))
            {
                method_doc = doc;
            }
            if method_doc.deprecated {
                deprecated = true;
            }

            // Parameters
            let mut params: Vec<RouteParam> = Vec::new();
            let mut request_body: Option<RequestBody> = None;
            let mut body_fields: Vec<BodyField> = Vec::new();
            for p in &m.function.params {
                if let Some(parsed) = self.classify_param(file, p, &method_doc) {
                    match parsed {
                        ParsedParam::Routed(rp) => params.push(rp),
                        ParsedParam::Body(b) => request_body = Some(b),
                        ParsedParam::BodyField(f) => body_fields.push(f),
                        ParsedParam::Ignored => {}
                    }
                }
            }
            // `@Body('field')` reads one property of the body; a whole-body
            // `@Body()` alongside already describes all of them.
            if request_body.is_none() && !body_fields.is_empty() {
                request_body = Some(body_from_fields(body_fields));
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
                    )
                };
                if rewritten.is_some() {
                    response_value = rewritten;
                } else {
                    let mut ctx = SchemaCtx {
                        project: self.project,
                        components: self.components,
                        type_env: None,
                        current_owner: None,
                        instantiating: Vec::new(),
                        passthrough_types: &self.config.passthrough_types,
                    };
                    let schema = ctx.ts_type_to_schema(file, &ret.type_ann);
                    if !schema.is_null() && schema != json!({}) {
                        response_value = Some(schema);
                    }
                }
            }

            // `@route-mapping` lets one method serving several paths return a
            // distinct schema per path, keyed by a trailing path suffix. Resolve
            // each mapped type once (resolution registers components), then match
            // it against each concrete path below.
            let route_mapping: Vec<(String, Value)> = if method_doc.route_mapping.is_empty() {
                Vec::new()
            } else {
                let mut ctx = SchemaCtx {
                    project: self.project,
                    components: self.components,
                    type_env: None,
                    current_owner: None,
                    instantiating: Vec::new(),
                    passthrough_types: &self.config.passthrough_types,
                };
                method_doc
                    .route_mapping
                    .iter()
                    .map(|(suffix, type_name)| {
                        (suffix.clone(), ctx.named_type_to_schema(file, type_name))
                    })
                    .collect()
            };

            let status_code = http_code.unwrap_or(verb.default_status());

            // Error-response signals for this method (path-independent parts).
            // The auth opt-out and role decorators apply if present on either the
            // method or its controller; `auth_active` follows the configured
            // scope. Pipe names come from every parameter's decorators.
            let er = &self.config.error_responses;
            let opted_out =
                class_opted_out || has_any_decorator(&m.function.decorators, &er.auth_opt_out);
            let has_roles =
                class_has_roles || has_any_decorator(&m.function.decorators, &er.role_decorators);
            let auth_active = match er.auth {
                AuthScope::Global => true,
                AuthScope::PerController => {
                    class_guarded || has_use_guards(&m.function.decorators, &er.auth_guards)
                }
                AuthScope::None => false,
            };
            let pipe_names: Vec<String> = m
                .function
                .params
                .iter()
                .flat_map(param_pipe_names)
                .collect();
            let scanned_throws: Vec<u16> = if er.scan_throws {
                scanned_throw_statuses(&m.function)
            } else {
                Vec::new()
            };

            // Per-path response is filled in below: an interceptor may override
            // it for only some of the route's sibling paths (e.g. a signing
            // interceptor that rewrites the body solely on the `/jws` variant).
            let template = Route {
                method: HttpMethod::Get,
                path: String::new(),
                summary: method_doc.summary.clone(),
                description: method_doc.description.clone(),
                deprecated,
                parameters: params,
                request_body,
                response: None,
                status_code,
                tags: vec![tag.to_string()],
                errors: BTreeMap::new(),
                catch_all: matches!(verb, VerbDecorator::All),
                handler: format!("{class_name}.{}", prop_name_text(&m.key)),
                file: file.to_path_buf(),
            };
            let has_body = template.request_body.is_some();
            for (&method, full_path) in verb
                .methods()
                .iter()
                .flat_map(|m| full_paths.iter().map(move |p| (m, p.clone())))
            {
                // A `@route-mapping` match is the most specific author intent, so
                // it wins over an interceptor override or the declared return type.
                let response = route_mapping
                    .iter()
                    .find(|(suffix, _)| path_matches_suffix(&full_path, suffix))
                    .map(|(_, schema)| schema.clone())
                    .or_else(|| response_override(self.config, &interceptor_names, &full_path))
                    .or_else(|| response_value.clone());
                let signals = errors::RouteSignals {
                    auth_active,
                    auth_opted_out: opted_out,
                    has_roles,
                    has_body,
                    pipes: &pipe_names,
                    throws: &method_doc.throws,
                    scanned_throws: &scanned_throws,
                    method: method.as_lower(),
                    path: &full_path,
                };
                let route_errors = errors::compute(&signals, er);
                out.push(Route {
                    method,
                    parameters: params_for_path(&template.parameters, &full_path),
                    path: full_path,
                    response,
                    errors: route_errors,
                    ..template.clone()
                });
            }
        }
    }

    /// The status named by `@HttpCode(<Enum>.<MEMBER>)`: a member of an enum
    /// in the symbol table, or of NestJS's `HttpStatus`, whose declarations are
    /// not parsed unless `@nestjs/common` is in `resolvePackages`.
    fn member_status(&self, file: &Path, enum_name: &str, member: &str) -> Option<u16> {
        if let Some(v) = self
            .project
            .resolve_enum_member_number(file, enum_name, member)
        {
            return Some(v as u16);
        }
        let binding = self.project.symbols.get(file)?.imports.get(enum_name)?;
        if binding.specifier == "@nestjs/common"
            && binding.imported.as_deref() == Some("HttpStatus")
        {
            errors::nest_http_status(member)
        } else {
            None
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
        let body_field = decorator_arg.clone();
        let name = decorator_arg.unwrap_or_else(|| pat_info.name.clone());
        let mut ctx = SchemaCtx {
            project: self.project,
            components: self.components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &self.config.passthrough_types,
        };
        let schema = pat_info
            .ts_type
            .as_ref()
            .map(|t| ctx.ts_type_to_schema(file, t))
            .unwrap_or_else(|| json!({}));
        let description = method_doc.params.get(&pat_info.name).cloned();
        match location {
            DecoratorKind::Body if body_field.is_some() => {
                Some(ParsedParam::BodyField(BodyField {
                    name,
                    required: !pat_info.optional,
                    schema,
                    description,
                }))
            }
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
    BodyField(BodyField),
    Ignored,
}

/// A `@Body('field')` parameter: one property of the request body.
struct BodyField {
    name: String,
    required: bool,
    schema: Value,
    description: Option<String>,
}

/// The object body assembled from a handler's `@Body('field')` parameters.
fn body_from_fields(fields: Vec<BodyField>) -> RequestBody {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for f in fields {
        let mut schema = f.schema;
        if let Some(d) = &f.description {
            describe(&mut schema, d);
        }
        let name = Value::String(f.name.clone());
        if f.required && !required.contains(&name) {
            required.push(name);
        }
        match properties.get_mut(&f.name) {
            Some(existing) if *existing != schema => {
                *existing = json!({ "allOf": [existing.take(), schema] });
            }
            Some(_) => {}
            None => {
                properties.insert(f.name, schema);
            }
        }
    }
    let any_required = !required.is_empty();
    let mut schema = json!({ "type": "object", "properties": properties });
    if any_required {
        schema["required"] = Value::Array(required);
    }
    RequestBody {
        schema,
        required: any_required,
        description: None,
    }
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
                    && prop_name_text(&kv.key) == "path"
                {
                    return expr_to_paths(kv.value.as_ref());
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

/// The identifier name of a decorator's first argument, e.g. `MULTI_STATUS` in
/// `@HttpCode(MULTI_STATUS)`. `None` when the argument is not a bare identifier.
fn ident_arg(d: &swc_ecma_ast::Decorator) -> Option<String> {
    let call = match &*d.expr {
        Expr::Call(c) => c,
        _ => return None,
    };
    match call.args.first()?.expr.as_ref() {
        Expr::Ident(i) => Some(i.sym.to_string()),
        _ => None,
    }
}

/// The `(object, property)` names of a decorator's first argument when it is a
/// plain member access, e.g. `("HttpStatus", "NO_CONTENT")`.
fn member_arg(d: &swc_ecma_ast::Decorator) -> Option<(String, String)> {
    let Expr::Call(call) = &*d.expr else {
        return None;
    };
    let Expr::Member(m) = call.args.first()?.expr.as_ref() else {
        return None;
    };
    match (m.obj.as_ref(), &m.prop) {
        (Expr::Ident(obj), swc_ecma_ast::MemberProp::Ident(prop)) => {
            Some((obj.sym.to_string(), prop.sym.to_string()))
        }
        _ => None,
    }
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

/// Whether any decorator in `decorators` has one of the given names.
fn has_any_decorator(decorators: &[Decorator], names: &[String]) -> bool {
    decorators
        .iter()
        .any(|d| names.iter().any(|n| decorator_name(d) == *n))
}

/// Whether a `@UseGuards(...)` decorator lists one of the given guard class
/// names (e.g. `AuthGuard`). Guards may be written as a bare identifier, a
/// `new Guard()`, or a `Guard(...)` factory call.
fn has_use_guards(decorators: &[Decorator], guards: &[String]) -> bool {
    for d in decorators {
        if decorator_name(d) == "UseGuards"
            && let Expr::Call(call) = &*d.expr
        {
            let mut names = Vec::new();
            for arg in &call.args {
                collect_class_idents(&arg.expr, &mut names);
            }
            if names.iter().any(|n| guards.contains(n)) {
                return true;
            }
        }
    }
    false
}

/// Pipe class names applied to a parameter's decorators (`@Param('id', new
/// ParseObjectIdPipe())` → `["ParseObjectIdPipe"]`). The field-name selector
/// (a string/number literal) contributes nothing.
fn param_pipe_names(p: &Param) -> Vec<String> {
    let mut out = Vec::new();
    for d in &p.decorators {
        if let Expr::Call(call) = &*d.expr {
            for arg in &call.args {
                collect_class_idents(&arg.expr, &mut out);
            }
        }
    }
    out
}

/// HTTP status codes thrown directly in a handler body via `throw new
/// <HttpException>(...)`, mapped through `errors::exception_status`. The whole
/// function body is scanned (including nested blocks and closures), so a throw
/// guarded by an `if` is still surfaced; only the handler's own body is read —
/// calls into services are not followed. Sorted and de-duplicated.
fn scanned_throw_statuses(func: &Function) -> Vec<u16> {
    let mut finder = ThrowFinder { codes: Vec::new() };
    func.visit_with(&mut finder);
    finder.codes.sort_unstable();
    finder.codes.dedup();
    finder.codes
}

struct ThrowFinder {
    codes: Vec<u16>,
}

impl Visit for ThrowFinder {
    fn visit_throw_stmt(&mut self, n: &swc_ecma_ast::ThrowStmt) {
        if let Expr::New(new) = n.arg.as_ref()
            && let Expr::Ident(i) = new.callee.as_ref()
            && let Some(code) = errors::exception_status(i.sym.as_ref())
        {
            self.codes.push(code);
        }
        n.visit_children_with(self);
    }
}

/// Push the class-name identifier(s) referenced by `expr` — a bare `Ident`, a
/// `new X()`, or an `X(...)` call — onto `out`. Literals and other expressions
/// contribute nothing.
fn collect_class_idents(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Ident(i) => out.push(i.sym.to_string()),
        Expr::New(n) => {
            if let Expr::Ident(i) = &*n.callee {
                out.push(i.sym.to_string());
            }
        }
        Expr::Call(c) => {
            if let Callee::Expr(e) = &c.callee
                && let Expr::Ident(i) = &**e
            {
                out.push(i.sym.to_string());
            }
        }
        _ => {}
    }
}

/// The handler's parameters as they apply to one concrete `path`. A handler
/// serving several paths (`@Get([':id', 'all'])`) declares the union of their
/// parameters, so a path parameter absent from this path is dropped; a
/// templated segment the handler never binds (e.g. read through a whole-object
/// `@Param()`) is declared as a plain string, since OpenAPI requires every
/// template to have one.
fn params_for_path(params: &[RouteParam], path: &str) -> Vec<RouteParam> {
    let templated: Vec<&str> = path
        .split('{')
        .skip(1)
        .filter_map(|rest| rest.split_once('}').map(|(name, _)| name))
        .collect();
    let mut out: Vec<RouteParam> = params
        .iter()
        .filter(|p| p.location != ParamLocation::Path || templated.contains(&p.name.as_str()))
        .cloned()
        .collect();
    for name in templated {
        if !out
            .iter()
            .any(|p| p.location == ParamLocation::Path && p.name == name)
        {
            out.push(RouteParam {
                name: name.to_string(),
                location: ParamLocation::Path,
                required: true,
                schema: json!({ "type": "string" }),
                description: None,
            });
        }
    }
    out
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
    openapi_params(&out)
}

/// Convert Express-style `:param` placeholders to OpenAPI `{param}`, wherever
/// they appear in a segment (`/files/:name.:ext` → `/files/{name}.{ext}`).
pub(crate) fn openapi_params(path: &str) -> String {
    let mut formatted = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
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

/// Whether an OpenAPI path matches a `@route-mapping` key. The key names a
/// trailing portion of the path (usually the last segment, e.g. `incomes`, but
/// possibly several, e.g. `incomes/start`); it matches when the path equals it
/// or ends with `/<suffix>`. Leading/trailing slashes on the key are ignored.
fn path_matches_suffix(path: &str, suffix: &str) -> bool {
    let suffix = suffix.trim_matches('/');
    if suffix.is_empty() {
        return false;
    }
    let path = path.trim_end_matches('/');
    path == suffix || path.ends_with(&format!("/{suffix}"))
}

/// Warn about paths that differ only in their parameter names
/// (`/users/{id}` and `/users/{slug}`): OpenAPI forbids them, as the router
/// can't tell them apart either.
fn warn_param_name_conflicts<'a>(paths: impl Iterator<Item = &'a String>) {
    let mut by_template: IndexMap<String, Vec<&str>> = IndexMap::new();
    for path in paths {
        let mut template = String::with_capacity(path.len());
        let mut in_param = false;
        for c in path.chars() {
            match c {
                '{' => in_param = true,
                '}' => {
                    in_param = false;
                    template.push_str("{}");
                }
                _ if !in_param => template.push(c),
                _ => {}
            }
        }
        by_template.entry(template).or_default().push(path);
    }
    for mut group in by_template.into_values().filter(|g| g.len() > 1) {
        group.sort_unstable();
        crate::diag::warn(format!(
            "paths {} differ only in parameter names, which OpenAPI forbids",
            group.join(" and ")
        ));
    }
}

fn prop_name_text(key: &PropName) -> String {
    match key {
        PropName::Ident(i) => i.sym.to_string(),
        PropName::Str(s) => s.value.to_string_lossy().into_owned(),
        _ => String::new(),
    }
}

fn normalize_segment(s: &str) -> String {
    let s = s.trim().trim_matches('/');
    if s.is_empty() {
        "".to_string()
    } else {
        format!("/{}", s)
    }
}

/// Convert routes + components into an OpenAPI document, in the version
/// `config.openapi_version` selects (3.1 natively; 3.0.3 via `dialect`).
pub fn to_openapi(config: &Config, routes: &[Route], mut components: ComponentSchemas) -> Value {
    components.resolve_deferred();
    // Register the shared error-body schema once, if any route emits an error
    // response. Tree-shaking below drops it if nothing ends up referencing it.
    if routes.iter().any(|r| !r.errors.is_empty()) {
        components
            .schemas
            .entry(ERROR_SCHEMA_NAME.to_string())
            .or_insert_with(errors::error_schema);
    }
    let mut paths: IndexMap<String, Map<String, Value>> = IndexMap::new();
    // NestJS registers handlers in declaration order and the router serves the
    // first match, so an `@All` and an explicit verb on the same method and path
    // resolve to whichever came first. Two explicit handlers keep last-wins, with
    // a warning naming both.
    let mut catch_all_slots: std::collections::HashSet<(&str, HttpMethod)> =
        std::collections::HashSet::new();
    let mut handlers: std::collections::HashMap<(&str, HttpMethod), &Route> =
        std::collections::HashMap::new();
    for r in routes {
        let slot = (r.path.as_str(), r.method);
        let taken = paths
            .get(&r.path)
            .is_some_and(|ops| ops.contains_key(r.method.as_lower()));
        if catch_all_slots.contains(&slot) || (r.catch_all && taken) {
            continue;
        }
        if r.catch_all {
            catch_all_slots.insert(slot);
        }
        if let Some(previous) = handlers.insert(slot, r)
            && (previous.file != r.file || previous.handler != r.handler)
        {
            let (overriding, overridden) = if previous.handler == r.handler {
                (
                    format!("{} ({})", r.handler, r.file.display()),
                    format!("{} ({})", previous.handler, previous.file.display()),
                )
            } else {
                (r.handler.clone(), previous.handler.clone())
            };
            crate::diag::warn(format!(
                "{} {}: {overriding} overrides {overridden}",
                r.method.as_lower().to_uppercase(),
                r.path,
            ));
        }
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
        // Error responses: each references the shared `Error` component so the
        // body shape is documented once. A code that somehow coincides with the
        // success status never overwrites it (the success entry was inserted
        // first and `entry` is a no-op then).
        for (code, description) in &r.errors {
            if *code == r.status_code {
                continue;
            }
            let mut status = Map::new();
            status.insert("description".into(), Value::String(description.clone()));
            let mut content = Map::new();
            let mut media = Map::new();
            media.insert(
                "schema".into(),
                json!({ "$ref": format!("#/components/schemas/{ERROR_SCHEMA_NAME}") }),
            );
            content.insert("application/json".into(), Value::Object(media));
            status.insert("content".into(), Value::Object(content));
            responses.insert(code.to_string(), Value::Object(status));
        }
        op.insert("responses".into(), Value::Object(responses));
        entry.insert(r.method.as_lower().to_string(), Value::Object(op));
    }

    warn_param_name_conflicts(paths.keys());

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

    // Tree-shake the component schemas: keep only those reachable from the
    // paths via `$ref`, following references transitively through other
    // schemas. Types are collected eagerly while lowering (a DTO may pull in
    // helper types that no route ultimately returns, or an interceptor may
    // replace a declared return type), so without this the document accumulates
    // orphaned schemas that bloat the output and confuse consumers.
    let reachable = reachable_schemas(&paths_json, &components.schemas);

    let mut root = Map::new();
    root.insert(
        "openapi".into(),
        Value::String(config.openapi_version.as_str().into()),
    );
    root.insert("info".into(), Value::Object(info));
    root.insert("paths".into(), Value::Object(paths_json));
    // Schemas are collected in type-encounter order, which also follows file
    // order; sort them for the same refactor-stability reason as paths.
    let mut schema_pairs: Vec<(String, Value)> = components
        .schemas
        .into_iter()
        .filter(|(name, _)| reachable.contains(name))
        .collect();
    if !schema_pairs.is_empty() {
        schema_pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let schemas: Map<String, Value> = schema_pairs.into_iter().collect();
        let mut comp = Map::new();
        comp.insert("schemas".into(), Value::Object(schemas));
        root.insert("components".into(), Value::Object(comp));
    }
    let mut doc = Value::Object(root);
    if config.openapi_version == OpenApiVersion::V30 {
        dialect::downgrade_to_30(&mut doc);
    }
    doc
}

/// Component-schema name referenced by a `$ref` of the form
/// `#/components/schemas/<name>`, if it matches that shape.
fn ref_target(value: &Value) -> Option<&str> {
    value
        .as_str()
        .and_then(|s| s.strip_prefix("#/components/schemas/"))
}

/// Collect every component-schema name referenced by a `$ref` anywhere within
/// `value` (recursing through objects and arrays).
fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                if key == "$ref" {
                    if let Some(name) = ref_target(v) {
                        out.push(name.to_string());
                    }
                } else {
                    collect_refs(v, out);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_refs(v, out);
            }
        }
        _ => {}
    }
}

/// Compute the set of component schemas reachable from the paths: start with
/// every `$ref` used by an operation, then transitively follow `$ref`s found
/// inside the referenced schemas. A missing target (dangling `$ref`) is simply
/// skipped — tree-shaking never invents schemas.
fn reachable_schemas(
    paths: &Map<String, Value>,
    schemas: &IndexMap<String, Value>,
) -> std::collections::HashSet<String> {
    let mut reachable = std::collections::HashSet::new();
    let mut queue: Vec<String> = Vec::new();
    for item in paths.values() {
        collect_refs(item, &mut queue);
    }

    while let Some(name) = queue.pop() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        if let Some(schema) = schemas.get(&name) {
            collect_refs(schema, &mut queue);
        }
    }
    reachable
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
    fn matches_route_suffix() {
        assert!(path_matches_suffix("/pets/{id}/cats", "cats"));
        assert!(path_matches_suffix("/pets/{id}/cats", "/cats/"));
        assert!(path_matches_suffix("/pets/{id}/cats/latest", "cats/latest"));
        assert!(path_matches_suffix("/cats", "cats"));
        // A bare substring that isn't a trailing segment must not match.
        assert!(!path_matches_suffix("/pets/{id}/cats", "ats"));
        assert!(!path_matches_suffix("/pets/{id}/dogs", "cats"));
        assert!(!path_matches_suffix("/pets/{id}/cats", ""));
    }

    /// Build a `Project` from a set of in-memory files, then extract routes +
    /// components.
    fn extract_from(
        name: &str,
        files: &[(&str, &str)],
        entry_rel: &str,
    ) -> (Vec<Route>, crate::schema::ComponentSchemas) {
        use crate::config::Config;
        use crate::parse::ParsingContext;
        use crate::schema::ComponentSchemas;

        let dir = std::env::temp_dir().join(format!("nestoa-rmap-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();

        let parser = ParsingContext::default();
        let mut project = Project::default();
        let mut entry = Vec::new();
        for (rel, body) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            project.add(parser.parse_file(&path).unwrap());
            if *rel == entry_rel {
                entry.push(path);
            }
        }

        let config = Config::default();
        let mut components = ComponentSchemas::default();
        let routes = {
            let mut ex = Extractor {
                config: &config,
                project: &project,
                entry_files: &entry,
                components: &mut components,
            };
            ex.extract_all()
        };
        (routes, components)
    }

    #[test]
    fn http_code_resolves_imported_numeric_const() {
        // `@HttpCode(MULTI_STATUS)` where the constant is declared in one module
        // and re-exported through a barrel (`export *`) — mirrors folder-manager
        // importing `MULTI_STATUS` from `@algoan/multi-status`.
        let controller = r#"
            import { MULTI_STATUS } from './lib';

            @Controller('batch')
            export class BatchController {
                @Post()
                @HttpCode(MULTI_STATUS)
                public async create(): Promise<string> { return ''; }
            }
        "#;
        let (routes, _components) = extract_from(
            "httpcode-const",
            &[
                ("batch.controller.ts", controller),
                ("lib.ts", "export * from './constants';"),
                ("constants.ts", "export const MULTI_STATUS = 207;"),
            ],
            "batch.controller.ts",
        );
        let post = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post)
            .unwrap();
        assert_eq!(post.status_code, 207);
    }

    #[test]
    fn error_responses_are_emitted_per_route_and_share_error_component() {
        let controller = r#"
            import { CreateDto } from './create.dto';

            @Controller('items')
            export class ItemsController {
                @Get(':id')
                @Roles({ roles: ['reader'] })
                public async findOne(@Param('id', new ParseObjectIdPipe()) id: string): Promise<string> {}

                @Post()
                @Unprotected()
                public async create(@Body() dto: CreateDto): Promise<string> {}

                @Get(':id/scores')
                @Roles({ roles: ['reader'] })
                public async scores(@Param('id', new ParseObjectIdPipe()) id: string): Promise<string> {
                    if (disabled) {
                        throw new UnprocessableEntityException('product disabled');
                    }
                    return '';
                }
            }
        "#;
        let dto = "export interface CreateDto { name: string; }";
        let (routes, components) = extract_from(
            "errors-e2e",
            &[("items.controller.ts", controller), ("create.dto.ts", dto)],
            "items.controller.ts",
        );

        let get = routes.iter().find(|r| r.method == HttpMethod::Get).unwrap();
        // Global auth (401) + @Roles (403) + ParseObjectIdPipe (404). No body → no 400.
        assert_eq!(
            get.errors.keys().copied().collect::<Vec<_>>(),
            vec![401, 403, 404]
        );

        let post = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post)
            .unwrap();
        // @Unprotected suppresses 401; no @Roles; a @Body DTO yields 400.
        assert_eq!(post.errors.keys().copied().collect::<Vec<_>>(), vec![400]);

        // The scores route throws UnprocessableEntityException in its body → 422
        // is detected on top of auth (401), role (403), and pipe (404).
        let scores = routes.iter().find(|r| r.path.ends_with("/scores")).unwrap();
        assert_eq!(
            scores.errors.keys().copied().collect::<Vec<_>>(),
            vec![401, 403, 404, 422]
        );

        // The shared Error component is registered and referenced.
        let doc = to_openapi(&crate::config::Config::default(), &routes, components);
        assert_eq!(
            doc["components"]["schemas"]["Error"]["properties"]["code"]["type"],
            "string"
        );
        let get_404 = &doc["paths"]["/items/{id}"]["get"]["responses"]["404"]["content"]["application/json"]
            ["schema"]["$ref"];
        assert_eq!(get_404, "#/components/schemas/Error");
    }

    #[test]
    fn route_mapping_gives_each_sibling_path_its_own_response() {
        let controller = r#"
            import { Cat } from './cat.dto';
            import { Dog } from './dog.dto';

            @Controller('pets')
            export class PetsController {
                /**
                 * Find a pet detail by kind
                 * @route-mapping{
                 * 'cats': Cat
                 * 'dogs': Dog
                 * }
                 */
                @Get([':id/cats', ':id/dogs'])
                public async findDetail(): Promise<Cat | Dog> {
                    return null as any;
                }
            }
        "#;
        let (routes, components) = extract_from(
            "per-path",
            &[
                ("cat.dto.ts", "export class Cat { meow: boolean; }"),
                ("dog.dto.ts", "export class Dog { bark: boolean; }"),
                ("pets.controller.ts", controller),
            ],
            "pets.controller.ts",
        );

        let cats = routes.iter().find(|r| r.path.ends_with("/cats")).unwrap();
        let dogs = routes.iter().find(|r| r.path.ends_with("/dogs")).unwrap();

        // Each path carries its mapped type as a $ref, not the declared union.
        assert_eq!(
            cats.response
                .as_ref()
                .and_then(|r| r.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/Cat")
        );
        assert_eq!(
            dogs.response
                .as_ref()
                .and_then(|r| r.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/Dog")
        );
        assert!(components.schemas.contains_key("Cat"));
        assert!(components.schemas.contains_key("Dog"));
    }

    #[test]
    fn route_mapping_unmatched_path_falls_back_to_declared_type() {
        // The mapping only covers `/cats`; the `/dogs` sibling has no entry and
        // must keep the handler's declared return type.
        let controller = r#"
            import { Cat } from './cat.dto';

            @Controller('pets')
            export class PetsController {
                /**
                 * @route-mapping{
                 * 'cats': Cat
                 * }
                 */
                @Get([':id/cats', ':id/dogs'])
                public async findDetail(): Promise<Cat> {
                    return null as any;
                }
            }
        "#;
        let (routes, _components) = extract_from(
            "fallback",
            &[
                ("cat.dto.ts", "export class Cat { meow: boolean; }"),
                ("pets.controller.ts", controller),
            ],
            "pets.controller.ts",
        );
        let cats = routes.iter().find(|r| r.path.ends_with("/cats")).unwrap();
        let dogs = routes.iter().find(|r| r.path.ends_with("/dogs")).unwrap();
        // Both resolve to Cat here (declared type is Cat), but via different
        // routes: cats via the mapping, dogs via the declared return type.
        assert_eq!(
            cats.response
                .as_ref()
                .and_then(|r| r.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/Cat")
        );
        assert_eq!(
            dogs.response
                .as_ref()
                .and_then(|r| r.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/Cat")
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
        let mut components = ComponentSchemas::default();
        let routes = {
            let mut ex = Extractor {
                config: &config,
                project: &project,
                entry_files: &entry,
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

    /// Extract the routes of a single in-memory controller file.
    fn extract_routes(name: &str, body: &str) -> Vec<Route> {
        extract_from(name, &[("ctrl.ts", body)], "ctrl.ts").0
    }

    fn find<'r>(routes: &'r [Route], method: HttpMethod, path: &str) -> &'r Route {
        routes
            .iter()
            .find(|r| r.method == method && r.path == path)
            .unwrap_or_else(|| panic!("no {method:?} {path}"))
    }

    fn path_param_names(route: &Route) -> Vec<&str> {
        route
            .parameters
            .iter()
            .filter(|p| p.location == ParamLocation::Path)
            .map(|p| p.name.as_str())
            .collect()
    }

    #[test]
    fn default_exported_controllers_are_extracted() {
        let routes = extract_routes(
            "default-export",
            r#"
            @Controller('items')
            export default class ItemsController {
                @Get()
                list() {}
            }
            "#,
        );
        assert_eq!(find(&routes, HttpMethod::Get, "/items").tags, ["Items"]);
    }

    #[test]
    fn the_topmost_verb_decorator_wins() {
        let routes = extract_routes(
            "stacked",
            r#"
            @Controller('items')
            export class ItemsController {
                @Get('a')
                @Post('b')
                stacked() {}
            }
            "#,
        );
        assert_eq!(routes.len(), 1);
        find(&routes, HttpMethod::Get, "/items/a");
    }

    #[test]
    fn default_status_is_201_for_post_and_200_otherwise() {
        let routes = extract_routes(
            "default-status",
            r#"
            @Controller('items')
            export class ItemsController {
                @Post()
                create() {}
                @Delete(':id')
                remove() {}
                @Put(':id')
                replace() {}
            }
            "#,
        );
        assert_eq!(find(&routes, HttpMethod::Post, "/items").status_code, 201);
        assert_eq!(
            find(&routes, HttpMethod::Delete, "/items/{id}").status_code,
            200
        );
        assert_eq!(
            find(&routes, HttpMethod::Put, "/items/{id}").status_code,
            200
        );
    }

    #[test]
    fn http_code_resolves_enum_members() {
        let controller = r#"
            import { HttpStatus, HttpStatus as Status } from '@nestjs/common';
            import { Codes } from './codes';
            const settings = { code: 299 };

            @Controller('items')
            export class ItemsController {
                @Delete(':id')
                @HttpCode(HttpStatus.NO_CONTENT)
                remove() {}
                @Post()
                @HttpCode(Status.OK)
                create() {}
                @Put(':id')
                @HttpCode(Codes.Later)
                replace() {}
                @Patch(':id')
                @HttpCode(settings.code)
                update() {}
                @Get(':id')
                @HttpCode(Codes.Next)
                read() {}
            }
        "#;
        let (routes, _) = extract_from(
            "httpcode-enum",
            &[
                ("ctrl.ts", controller),
                (
                    "codes.ts",
                    "export enum Codes { Label = 'x', Accepted = 202, Later, Sum = 1 + 1, Next }",
                ),
            ],
            "ctrl.ts",
        );
        let status = |m, p| find(&routes, m, p).status_code;
        assert_eq!(status(HttpMethod::Delete, "/items/{id}"), 204);
        assert_eq!(status(HttpMethod::Post, "/items"), 200);
        assert_eq!(status(HttpMethod::Put, "/items/{id}"), 203);
        // Not an enum, or a member whose implicit value follows an unsupported
        // initialiser: falls back to the method default.
        assert_eq!(status(HttpMethod::Patch, "/items/{id}"), 200);
        assert_eq!(status(HttpMethod::Get, "/items/{id}"), 200);
    }

    #[test]
    fn all_expands_to_every_method_with_status_200() {
        let routes = extract_routes(
            "all",
            r#"
            @Controller('proxy')
            export class ProxyController {
                @All()
                forward() {}
            }
            "#,
        );
        assert_eq!(routes.len(), ALL_METHODS.len());
        for method in ALL_METHODS {
            assert_eq!(find(&routes, method, "/proxy").status_code, 200);
        }
        let doc = to_openapi(&Config::default(), &routes, ComponentSchemas::default());
        let ops: Vec<&String> = doc["paths"]["/proxy"].as_object().unwrap().keys().collect();
        assert_eq!(
            ops,
            [
                "delete", "get", "head", "options", "patch", "post", "put", "trace"
            ]
        );
    }

    #[test]
    fn first_of_all_and_explicit_verb_serves_the_method() {
        let summaries = |name, body| {
            let routes = extract_routes(name, body);
            let doc = to_openapi(&Config::default(), &routes, ComponentSchemas::default());
            let ops = &doc["paths"]["/proxy"];
            (
                ops["get"]["summary"].clone(),
                ops["post"]["summary"].clone(),
            )
        };
        let explicit_first = summaries(
            "explicit-first",
            r#"
            @Controller('proxy')
            export class ProxyController {
                /** Explicit read */
                @Get()
                read() {}
                /** Catch-all */
                @All()
                forward() {}
            }
            "#,
        );
        assert_eq!(explicit_first, (json!("Explicit read"), json!("Catch-all")));
        let all_first = summaries(
            "all-first",
            r#"
            @Controller('proxy')
            export class ProxyController {
                /** Catch-all */
                @All()
                forward() {}
                /** Explicit read */
                @Get()
                read() {}
            }
            "#,
        );
        assert_eq!(all_first, (json!("Catch-all"), json!("Catch-all")));
        assert_eq!(crate::diag::take_warnings(), Vec::<String>::new());
    }

    #[test]
    fn two_explicit_handlers_on_one_route_warn() {
        crate::diag::take_warnings();
        let routes = extract_routes(
            "duplicate",
            r#"
            @Controller('items')
            export class ItemsController {
                /** First */
                @Get()
                list() {}
                /** Second */
                @Get()
                listAgain() {}
            }
            "#,
        );
        let doc = to_openapi(&Config::default(), &routes, ComponentSchemas::default());
        assert_eq!(doc["paths"]["/items"]["get"]["summary"], json!("Second"));
        assert_eq!(
            crate::diag::take_warnings(),
            ["GET /items: ItemsController.listAgain overrides ItemsController.list"]
        );
    }

    #[test]
    fn paths_differing_only_in_param_names_warn() {
        crate::diag::take_warnings();
        let routes = [
            route(HttpMethod::Get, "/users/{id}"),
            route(HttpMethod::Delete, "/users/{slug}"),
            route(HttpMethod::Get, "/users/{id}/posts"),
        ];
        to_openapi(&Config::default(), &routes, ComponentSchemas::default());
        assert_eq!(
            crate::diag::take_warnings(),
            [
                "paths /users/{id} and /users/{slug} differ only in parameter names, which OpenAPI forbids"
            ]
        );
    }

    #[test]
    fn same_named_handlers_from_two_files_warn() {
        crate::diag::take_warnings();
        let first = route(HttpMethod::Get, "/ping");
        let second = Route {
            file: PathBuf::from("other.ts"),
            ..first.clone()
        };
        to_openapi(
            &Config::default(),
            &[first, second],
            ComponentSchemas::default(),
        );
        assert_eq!(
            crate::diag::take_warnings(),
            ["GET /ping: Test.get (other.ts) overrides Test.get (test.ts)"]
        );
    }

    #[test]
    fn path_params_follow_each_concrete_path() {
        let routes = extract_routes(
            "sibling-params",
            r#"
            @Controller('orgs/:orgId/items')
            export class ItemsController {
                @Get([':id', 'all'])
                get(@Param('id') id: string, @Query('q') q?: string) {}
            }
            "#,
        );
        let one = find(&routes, HttpMethod::Get, "/orgs/{orgId}/items/{id}");
        assert_eq!(path_param_names(one), ["id", "orgId"]);
        let all = find(&routes, HttpMethod::Get, "/orgs/{orgId}/items/all");
        assert_eq!(path_param_names(all), ["orgId"]);
        // Non-path parameters are untouched.
        assert!(all.parameters.iter().any(|p| p.name == "q"));
        let synthesized = all.parameters.iter().find(|p| p.name == "orgId").unwrap();
        assert!(synthesized.required);
        assert_eq!(synthesized.schema, json!({ "type": "string" }));
    }

    #[test]
    fn body_fields_form_one_object_body() {
        let routes = extract_routes(
            "body-fields",
            r#"
            export class Dto { name: string; }
            @Controller('items')
            export class ItemsController {
                /**
                 * Rename
                 * @param name the new name
                 */
                @Put('fields')
                fields(@Body('name') name: string, @Body('age') age?: number) {}

                @Post('whole')
                whole(@Body() dto: Dto, @Body('name') name: string) {}

                @Patch('twice')
                twice(@Body('name') a: string, @Body('name') b: 'x') {}
            }
            "#,
        );
        let body = find(&routes, HttpMethod::Put, "/items/fields")
            .request_body
            .as_ref()
            .unwrap();
        assert!(body.required);
        assert_eq!(
            body.schema,
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "the new name" },
                    "age": { "type": "number" },
                },
                "required": ["name"],
            })
        );
        let whole = find(&routes, HttpMethod::Post, "/items/whole")
            .request_body
            .as_ref()
            .unwrap();
        assert_eq!(whole.schema, json!({ "$ref": "#/components/schemas/Dto" }));
        let twice = find(&routes, HttpMethod::Patch, "/items/twice")
            .request_body
            .as_ref()
            .unwrap();
        assert_eq!(twice.schema["required"], json!(["name"]));
        assert_eq!(
            twice.schema["properties"]["name"],
            json!({ "allOf": [{ "type": "string" }, { "type": "string", "enum": ["x"] }] })
        );
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
            errors: BTreeMap::new(),
            catch_all: false,
            handler: format!("Test.{}", method.as_lower()),
            file: PathBuf::from("test.ts"),
        }
    }

    /// Like `route`, but its response body `$ref`s the named component schema,
    /// so the schema survives tree-shaking.
    fn route_ref(method: HttpMethod, path: &str, schema_ref: &str) -> Route {
        use serde_json::json;
        let mut r = route(method, path);
        r.response = Some(json!({"$ref": format!("#/components/schemas/{schema_ref}")}));
        r
    }

    #[test]
    fn to_openapi_emits_3_1_by_default_and_downgrades_on_request() {
        use crate::config::OpenApiVersion;
        use crate::schema::ComponentSchemas;
        use serde_json::json;

        let build = |version: OpenApiVersion| {
            let mut components = ComponentSchemas::default();
            components.insert(
                "Account",
                json!({
                    "type": "object",
                    "properties": {
                        "label": {"type": ["string", "null"]},
                        "owner": {"anyOf": [{"$ref": "#/components/schemas/Owner"}, {"type": "null"}]},
                        "range": {"type": "array", "prefixItems": [{"type": "number"}, {"type": "number"}], "items": false}
                    }
                }),
            );
            components.insert("Owner", json!({"type": "object"}));
            let config = Config {
                openapi_version: version,
                ..Config::default()
            };
            to_openapi(
                &config,
                &[route_ref(HttpMethod::Get, "/accounts", "Account")],
                components,
            )
        };

        let v31 = build(OpenApiVersion::V31);
        assert_eq!(v31["openapi"], "3.1.0");
        let props = &v31["components"]["schemas"]["Account"]["properties"];
        assert_eq!(props["label"]["type"], json!(["string", "null"]));
        assert!(props["owner"]["anyOf"].is_array());
        assert!(props["range"]["prefixItems"].is_array());

        let v30 = build(OpenApiVersion::V30);
        assert_eq!(v30["openapi"], "3.0.3");
        let props = &v30["components"]["schemas"]["Account"]["properties"];
        assert_eq!(props["label"], json!({"type": "string", "nullable": true}));
        // The nullable wrapper is gone; the reference survives and still keeps
        // `Owner` reachable for tree-shaking.
        assert_eq!(
            props["owner"],
            json!({"$ref": "#/components/schemas/Owner"})
        );
        assert!(v30["components"]["schemas"].get("Owner").is_some());
        assert_eq!(
            props["range"],
            json!({"type": "array", "items": {"type": "number"}, "maxItems": 2})
        );
    }

    #[test]
    fn to_openapi_sorts_paths_methods_and_schemas() {
        use crate::schema::ComponentSchemas;
        use serde_json::json;

        // Feed routes, methods, and schemas in deliberately non-alphabetical order.
        // Each route references a schema so all three survive tree-shaking.
        let routes = vec![
            route_ref(HttpMethod::Post, "/users", "Zebra"),
            route_ref(HttpMethod::Get, "/users", "Apple"),
            route_ref(HttpMethod::Get, "/accounts", "Mango"),
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
        let method_keys: Vec<&String> =
            doc["paths"]["/users"].as_object().unwrap().keys().collect();
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
            route_ref(HttpMethod::Get, "/a", "Beta"),
            route_ref(HttpMethod::Post, "/a", "Alpha"),
            route_ref(HttpMethod::Get, "/b", "Gamma"),
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

    #[test]
    fn to_openapi_prunes_unused_schemas() {
        use crate::schema::ComponentSchemas;
        use serde_json::json;

        // `Used` is referenced by a route response; `Nested` is referenced only
        // by `Used` (so it must survive transitively); `Orphan` is referenced by
        // nobody and must be dropped.
        let routes = vec![route_ref(HttpMethod::Get, "/things", "Used")];
        let mut components = ComponentSchemas::default();
        components.insert(
            "Used",
            json!({
                "type": "object",
                "properties": { "child": { "$ref": "#/components/schemas/Nested" } }
            }),
        );
        components.insert("Nested", json!({"type": "object"}));
        components.insert("Orphan", json!({"type": "object"}));

        let doc = to_openapi(&Config::default(), &routes, components);

        let schema_keys: Vec<&String> = doc["components"]["schemas"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(schema_keys, vec!["Nested", "Used"]);
    }

    #[test]
    fn to_openapi_omits_components_when_all_schemas_unused() {
        use crate::schema::ComponentSchemas;
        use serde_json::json;

        // A schema referenced by no route leaves the document with no
        // `components` section at all, rather than an empty one.
        let routes = vec![route(HttpMethod::Get, "/things")];
        let mut components = ComponentSchemas::default();
        components.insert("Orphan", json!({"type": "object"}));

        let doc = to_openapi(&Config::default(), &routes, components);
        assert!(doc.as_object().unwrap().get("components").is_none());
    }
}
