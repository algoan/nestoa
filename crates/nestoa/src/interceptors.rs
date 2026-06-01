//! NestJS interceptor handling.
//!
//! Interceptors can reshape the payload between the route handler and the wire.
//! NestJS types this with `NestInterceptor<In, Out>`, where `In` is the type the
//! handler returns and `Out` is what the client receives — so the *response*
//! transform is recoverable from the interceptor's `implements` clause. We
//! resolve the interceptor class, read those two type arguments, bind the
//! interceptor's generic parameters by unifying the handler's (Promise/Observable
//! unwrapped) return type against `In`, then lower `Out` as the response schema.
//!
//! Request-body reshaping (e.g. accepting a single object the handler sees as an
//! array) is done imperatively and is *not* in the type signature, so it is
//! driven by the `interceptors` config map instead.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use swc_common::BytePos;
use swc_ecma_ast::{Class, Decorator, Expr, TsArrayType, TsEntityName, TsType};

use crate::config::{Config, RequestBodyTransform};
use crate::routes::RequestBody;
use crate::schema::{ComponentSchemas, SchemaCtx, TypeBinding, TypeEnv};
use crate::symbols::{Project, Symbol};

/// Collect the interceptor class names referenced by a single `@UseInterceptors(...)`
/// decorator, in declared order. Handles `new Foo(...)`, bare `Foo`, and arrays.
pub fn interceptor_class_names(d: &Decorator) -> Vec<String> {
    let mut names = Vec::new();
    if let Expr::Call(call) = &*d.expr {
        for arg in &call.args {
            collect_arg_names(&arg.expr, &mut names);
        }
    }
    names
}

fn collect_arg_names(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::New(n) => {
            if let Expr::Ident(i) = &*n.callee {
                out.push(i.sym.to_string());
            }
        }
        Expr::Ident(i) => out.push(i.sym.to_string()),
        Expr::Array(arr) => {
            for elem in arr.elems.iter().flatten() {
                collect_arg_names(&elem.expr, out);
            }
        }
        _ => {}
    }
}

/// Apply any configured request-body reshaping for the route's interceptors.
pub fn request_body_transform(
    config: &Config,
    interceptor_names: &[String],
    body: &mut RequestBody,
) {
    for name in interceptor_names {
        if let Some(cfg) = config.interceptors.get(name) {
            if let Some(RequestBodyTransform::SingleOrArray) = cfg.request_body {
                let single = body.schema.clone();
                let array = json!({ "type": "array", "items": single.clone() });
                body.schema = json!({ "oneOf": [single, array] });
            }
        }
    }
}

/// A configured response-schema override for the given route path, if any of the
/// route's interceptors declares one whose `whenPathEndsWith` matches the path
/// (or has no suffix condition). This covers interceptors whose transform is
/// runtime control flow the type signature can't express — e.g. a signing
/// interceptor that only rewrites the body when the URL ends in `/jws`, leaving
/// the sibling route (same interceptor, different array path) untouched.
///
/// Later interceptors in the list win, mirroring `rewrite_response`'s ordering.
/// Returned schema is cloned from config and inlined verbatim.
pub fn response_override(
    config: &Config,
    interceptor_names: &[String],
    path: &str,
) -> Option<Value> {
    let mut found = None;
    for name in interceptor_names {
        let Some(cfg) = config.interceptors.get(name) else {
            continue;
        };
        let Some(ov) = &cfg.response else { continue };
        let matches = match &ov.when_path_ends_with {
            Some(suffix) => path.ends_with(suffix.as_str()),
            None => true,
        };
        if matches {
            found = Some(ov.schema.clone());
        }
    }
    found
}

/// Compute the response schema after applying the route's interceptors, or
/// `None` to keep the handler's declared return type unchanged.
///
/// Interceptors are applied in declared order (class-level before method-level);
/// each matching interceptor's `Out` becomes the input type for the next.
#[allow(clippy::too_many_arguments)]
pub fn rewrite_response(
    project: &Project,
    config: &Config,
    controller_file: &Path,
    declared_return: &TsType,
    interceptor_names: &[String],
    components: &mut ComponentSchemas,
    sources: &HashMap<PathBuf, String>,
    file_starts: &HashMap<PathBuf, BytePos>,
) -> Option<Value> {
    let mut current_ty = unwrap_async(declared_return).clone();
    let mut current_file = controller_file.to_path_buf();
    let mut env: TypeEnv = HashMap::new();
    let mut matched = false;

    for name in interceptor_names {
        // Side-effect interceptors the user has told us to ignore.
        if config
            .interceptors
            .get(name)
            .map(|c| c.ignore)
            .unwrap_or(false)
        {
            continue;
        }
        // Resolve the interceptor class (may live in node_modules / a workspace lib).
        let Some((decl_file, Symbol::Class(class_decl))) = project.resolve(controller_file, name)
        else {
            continue;
        };
        let class = &class_decl.class;
        let Some((in_ty, out_ty)) = nest_interceptor_args(class) else {
            continue;
        };
        let params = type_param_names(class);

        // Bind the interceptor's generics from the current (incoming) type.
        let mut local_env: TypeEnv = HashMap::new();
        if !unify(&params, &in_ty, &current_ty, &current_file, &mut local_env) {
            // `In` doesn't describe the incoming type — leave it untransformed.
            continue;
        }
        // Newer bindings win, but keep prior ones so chained substitutions resolve.
        env.extend(local_env);
        current_ty = out_ty;
        current_file = decl_file;
        matched = true;
    }

    if !matched {
        return None;
    }

    let mut ctx = SchemaCtx {
        project,
        sources,
        file_starts,
        components,
        type_env: Some(&env),
    };
    let schema = lower_inline(&mut ctx, &current_file, &current_ty);
    if schema.is_null() || schema == json!({}) {
        None
    } else {
        Some(schema)
    }
}

/// Lower the (possibly generic) `Out` type. When it is a bare reference to a
/// project type, inline that declaration's body under the active substitution
/// env rather than emitting a `$ref` to a shared named component — a generic
/// wrapper like `PaginatedData<T>` must not collapse every element type onto a
/// single component. Falls back to the normal lowering for everything else
/// (arrays, type literals, built-ins such as `Omit`, unions, …).
fn lower_inline(ctx: &mut SchemaCtx, file: &Path, ty: &TsType) -> Value {
    if let TsType::TsTypeRef(r) = ty {
        if let Some(name) = ref_ident(&r.type_name) {
            if let Some((decl_file, sym)) = ctx.project.resolve(file, &name) {
                return ctx.symbol_to_schema(&decl_file, &sym);
            }
        }
    }
    ctx.ts_type_to_schema(file, ty)
}

/// The `(In, Out)` type arguments of a class's `implements NestInterceptor<In, Out>`.
fn nest_interceptor_args(class: &Class) -> Option<(TsType, TsType)> {
    for imp in &class.implements {
        if let Expr::Ident(id) = &*imp.expr {
            if &*id.sym == "NestInterceptor" {
                let args = imp.type_args.as_ref()?;
                let in_ty = args.params.first()?;
                let out_ty = args.params.get(1)?;
                return Some(((**in_ty).clone(), (**out_ty).clone()));
            }
        }
    }
    None
}

/// The names of a class's generic type parameters, e.g. `{T}` for `class X<T>`.
fn type_param_names(class: &Class) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(tp) = &class.type_params {
        for p in &tp.params {
            set.insert(p.name.sym.to_string());
        }
    }
    set
}

/// Peel `Promise<...>` / `Observable<...>` wrappers off a handler return type.
fn unwrap_async(ty: &TsType) -> &TsType {
    if let TsType::TsTypeRef(r) = ty {
        if let Some(name) = ref_ident(&r.type_name) {
            if name == "Promise" || name == "Observable" {
                if let Some(args) = &r.type_params {
                    if let Some(first) = args.params.first() {
                        return unwrap_async(first);
                    }
                }
            }
        }
    }
    ty
}

/// Structurally unify an interceptor's `In` pattern against the concrete
/// incoming type, recording type-parameter bindings in `env`. Returns whether
/// the shapes are compatible. Handles bare param refs, arrays (`T[]`/`Array<T>`),
/// and same-head generic refs (`Wrapper<T>` ↔ `Wrapper<Concrete>`).
fn unify(
    params: &HashSet<String>,
    pattern: &TsType,
    concrete: &TsType,
    concrete_file: &Path,
    env: &mut TypeEnv,
) -> bool {
    // Arrays on both sides → recurse on the element type.
    if let (Some(pe), Some(ce)) = (array_elem(pattern), array_elem(concrete)) {
        return unify(params, pe, ce, concrete_file, env);
    }

    let TsType::TsTypeRef(pr) = pattern else {
        return false;
    };
    let Some(pname) = ref_ident(&pr.type_name) else {
        return false;
    };

    // Bare reference to a generic parameter → bind it to the concrete type.
    if pr.type_params.is_none() && params.contains(&pname) {
        env.insert(
            pname,
            TypeBinding {
                origin_file: concrete_file.to_path_buf(),
                ty: concrete.clone(),
            },
        );
        return true;
    }

    // Same-head generic reference → recurse positionally over type arguments.
    let TsType::TsTypeRef(cr) = concrete else {
        return false;
    };
    let Some(cname) = ref_ident(&cr.type_name) else {
        return false;
    };
    if cname != pname {
        return false;
    }
    match (&pr.type_params, &cr.type_params) {
        (Some(pa), Some(ca)) if pa.params.len() == ca.params.len() => pa
            .params
            .iter()
            .zip(ca.params.iter())
            .all(|(p, c)| unify(params, p, c, concrete_file, env)),
        (None, None) => true,
        _ => false,
    }
}

/// The element type of an array type, accepting both `T[]` and `Array<T>` forms.
fn array_elem(ty: &TsType) -> Option<&TsType> {
    match ty {
        TsType::TsArrayType(TsArrayType { elem_type, .. }) => Some(elem_type.as_ref()),
        TsType::TsTypeRef(r) => {
            let name = ref_ident(&r.type_name)?;
            if name == "Array" || name == "ReadonlyArray" {
                r.type_params.as_ref()?.params.first().map(|b| b.as_ref())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The identifier of a simple (non-qualified) type-entity name.
fn ref_ident(name: &TsEntityName) -> Option<String> {
    match name {
        TsEntityName::Ident(i) => Some(i.sym.to_string()),
        TsEntityName::TsQualifiedName(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::parse::ParsingContext;
    use crate::routes::{Extractor, Route};
    use crate::schema::ComponentSchemas;
    use swc_ecma_ast::{Decl, ModuleItem, Stmt};

    /// Write `files` into a fresh temp dir and build a `Project` from them.
    /// Returns the project, the dir, and the absolute paths in order.
    fn build(name: &str, files: &[(&str, &str)]) -> (Project, PathBuf, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!("nestoa-intc-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let parser = ParsingContext::default();
        let mut project = Project::default();
        let mut paths = Vec::new();
        for (rel, body) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            project.add(parser.parse_file(&path).unwrap());
            paths.push(path);
        }
        (project, dir, paths)
    }

    /// Run the route extractor and return the routes plus collected components.
    fn extract(
        config: &Config,
        project: &Project,
        entry: &[PathBuf],
    ) -> (Vec<Route>, ComponentSchemas) {
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let mut components = ComponentSchemas::default();
        let routes = {
            let mut ex = Extractor {
                config,
                project,
                entry_files: entry,
                sources: &sources,
                file_starts: &file_starts,
                components: &mut components,
            };
            ex.extract_all()
        };
        (routes, components)
    }

    /// Parse a single TS type fragment by wrapping it in a type alias.
    fn parse_ty(dir: &Path, parser: &ParsingContext, name: &str, frag: &str) -> TsType {
        let path = dir.join(format!("{name}.ts"));
        std::fs::write(&path, format!("type _Q = {frag};")).unwrap();
        let pf = parser.parse_file(&path).unwrap();
        for item in &pf.module.body {
            if let ModuleItem::Stmt(Stmt::Decl(Decl::TsTypeAlias(a))) = item {
                return (*a.type_ann).clone();
            }
        }
        panic!("no type alias parsed from `{frag}`");
    }

    const PAGINATION_DTS: &str = r#"
        export interface DataToPaginate<T> {
            resources: T[];
            totalResources: number;
        }
        interface PaginatedData<T> {
            resources: T[];
            pagination: {
                next: string | null;
                totalResources: number | null;
            };
        }
        export declare class PaginationBodyInterceptor<T>
            implements NestInterceptor<DataToPaginate<T>, PaginatedData<T>> {
            intercept(): unknown;
        }
    "#;

    const PROJECT_DTO: &str = "export class Project { id: string; name?: string; }";

    fn paginated_controller(ret: &str) -> String {
        format!(
            r#"
            import {{ PaginationBodyInterceptor, DataToPaginate }} from './pagination';
            import {{ Project }} from './project.dto';

            @Controller('projects')
            export class ProjectsController {{
                @Get()
                @UseInterceptors(new PaginationBodyInterceptor({{ perPageName: 'limit' }}))
                public async findAll(): {ret} {{
                    return null as any;
                }}
            }}
        "#
        )
    }

    #[test]
    fn unify_binds_bare_param() {
        let (_p, dir, _) = build("unify1", &[]);
        let parser = ParsingContext::default();
        let params: HashSet<String> = ["T".to_string()].into_iter().collect();
        let pattern = parse_ty(&dir, &parser, "p", "T");
        let concrete = parse_ty(&dir, &parser, "c", "Project");
        let mut env = TypeEnv::new();
        assert!(unify(&params, &pattern, &concrete, &dir, &mut env));
        assert!(env.contains_key("T"));
    }

    #[test]
    fn unify_binds_through_array_and_generic() {
        let (_p, dir, _) = build("unify2", &[]);
        let parser = ParsingContext::default();
        let params: HashSet<String> = ["T".to_string()].into_iter().collect();

        let mut env = TypeEnv::new();
        assert!(unify(
            &params,
            &parse_ty(&dir, &parser, "pa", "T[]"),
            &parse_ty(&dir, &parser, "ca", "Project[]"),
            &dir,
            &mut env,
        ));
        assert!(env.contains_key("T"));

        let mut env = TypeEnv::new();
        assert!(unify(
            &params,
            &parse_ty(&dir, &parser, "pg", "DataToPaginate<T>"),
            &parse_ty(&dir, &parser, "cg", "DataToPaginate<Project>"),
            &dir,
            &mut env,
        ));
        assert!(env.contains_key("T"));
    }

    #[test]
    fn unify_rejects_mismatched_head_and_arity() {
        let (_p, dir, _) = build("unify3", &[]);
        let parser = ParsingContext::default();
        let params: HashSet<String> = ["T".to_string()].into_iter().collect();

        let mut env = TypeEnv::new();
        assert!(!unify(
            &params,
            &parse_ty(&dir, &parser, "ph", "DataToPaginate<T>"),
            &parse_ty(&dir, &parser, "ch", "Other<Project>"),
            &dir,
            &mut env,
        ));

        let mut env = TypeEnv::new();
        assert!(!unify(
            &params,
            &parse_ty(&dir, &parser, "pn", "Foo<T>"),
            &parse_ty(&dir, &parser, "cn", "Foo"),
            &dir,
            &mut env,
        ));
    }

    #[test]
    fn non_exported_local_interface_resolves_in_its_own_file() {
        let (project, _dir, paths) = build("nonexp", &[("pagination.d.ts", PAGINATION_DTS)]);
        let resolved = project.resolve(&paths[0], "PaginatedData");
        assert!(matches!(resolved, Some((_, Symbol::Interface(_)))));
    }

    #[test]
    fn pagination_interceptor_rewrites_response() {
        let (project, _dir, paths) = build(
            "pag",
            &[
                ("pagination.d.ts", PAGINATION_DTS),
                ("project.dto.ts", PROJECT_DTO),
                (
                    "projects.controller.ts",
                    &paginated_controller("Promise<DataToPaginate<Project>>"),
                ),
            ],
        );
        let config = Config::default();
        let (routes, components) = extract(&config, &project, &[paths[2].clone()]);

        let route = routes.iter().find(|r| r.path == "/projects").unwrap();
        let resp = route.response.as_ref().expect("response schema");

        // Shape is the interceptor's PaginatedData, not the handler's DataToPaginate.
        let props = resp.get("properties").expect("object with properties");
        assert!(
            props.get("pagination").is_some(),
            "pagination block present"
        );
        let items = props
            .get("resources")
            .and_then(|r| r.get("items"))
            .and_then(|i| i.get("$ref"))
            .and_then(|v| v.as_str())
            .expect("resources items $ref");
        assert_eq!(items, "#/components/schemas/Project");

        // The element DTO is registered; the generic wrappers are inlined, not refs.
        assert!(components.schemas.contains_key("Project"));
        assert!(!components.schemas.contains_key("PaginatedData"));
        assert!(!components.schemas.contains_key("DataToPaginate"));
    }

    #[test]
    fn ignore_config_keeps_declared_response() {
        let (project, _dir, paths) = build(
            "ignore",
            &[
                ("pagination.d.ts", PAGINATION_DTS),
                ("project.dto.ts", PROJECT_DTO),
                (
                    "projects.controller.ts",
                    &paginated_controller("Promise<DataToPaginate<Project>>"),
                ),
            ],
        );
        let mut config = Config::default();
        config.interceptors.insert(
            "PaginationBodyInterceptor".to_string(),
            crate::config::InterceptorConfig {
                ignore: true,
                request_body: None,
                response: None,
            },
        );
        let (routes, _components) = extract(&config, &project, &[paths[2].clone()]);
        let route = routes.iter().find(|r| r.path == "/projects").unwrap();
        let resp = route.response.as_ref().expect("response schema");
        // Untransformed: the declared DataToPaginate<Project> is emitted as a $ref.
        let refstr = resp.get("$ref").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(refstr, "#/components/schemas/DataToPaginate");
    }

    fn jws_controller() -> String {
        r#"
            import { Project } from './project.dto';

            @Controller('analyses')
            export class AnalysisController {
                @Get([':id', ':id/jws'])
                @UseInterceptors(new SignResponseInterceptor('role'))
                public async findById(): Promise<Project> {
                    return null as any;
                }
            }
        "#
        .to_string()
    }

    #[test]
    fn response_override_applies_only_to_matching_path() {
        let (project, _dir, paths) = build(
            "jws",
            &[
                ("project.dto.ts", PROJECT_DTO),
                ("analysis.controller.ts", &jws_controller()),
            ],
        );
        let mut config = Config::default();
        config.interceptors.insert(
            "SignResponseInterceptor".to_string(),
            crate::config::InterceptorConfig {
                ignore: false,
                request_body: None,
                response: Some(crate::config::ResponseOverride {
                    when_path_ends_with: Some("/jws".to_string()),
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "protected": { "type": "string" },
                            "payload": { "type": "string" },
                            "signature": { "type": "string" }
                        },
                        "required": ["protected", "payload", "signature"]
                    }),
                }),
            },
        );
        let (routes, _components) = extract(&config, &project, &[paths[1].clone()]);

        let jws = routes
            .iter()
            .find(|r| r.path.ends_with("/jws"))
            .expect("jws route present");
        let plain = routes
            .iter()
            .find(|r| !r.path.ends_with("/jws"))
            .expect("plain sibling present");

        // The /jws sibling carries the configured JWS envelope.
        let jws_resp = jws.response.as_ref().expect("jws response schema");
        let props = jws_resp
            .get("properties")
            .expect("jws object with properties");
        assert!(props.get("protected").is_some());
        assert!(props.get("payload").is_some());
        assert!(props.get("signature").is_some());

        // The plain sibling keeps the handler's declared Project shape untouched.
        let plain_resp = plain.response.as_ref().expect("plain response schema");
        let refstr = plain_resp
            .get("$ref")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(refstr, "#/components/schemas/Project");
    }

    #[test]
    fn response_override_without_suffix_applies_to_all_paths() {
        let (project, _dir, paths) = build(
            "jws_all",
            &[
                ("project.dto.ts", PROJECT_DTO),
                ("analysis.controller.ts", &jws_controller()),
            ],
        );
        let mut config = Config::default();
        config.interceptors.insert(
            "SignResponseInterceptor".to_string(),
            crate::config::InterceptorConfig {
                ignore: false,
                request_body: None,
                response: Some(crate::config::ResponseOverride {
                    when_path_ends_with: None,
                    schema: json!({ "type": "string" }),
                }),
            },
        );
        let (routes, _components) = extract(&config, &project, &[paths[1].clone()]);
        assert_eq!(routes.len(), 2);
        for route in &routes {
            let resp = route.response.as_ref().expect("response schema");
            assert_eq!(resp.get("type").and_then(|v| v.as_str()), Some("string"));
        }
    }

    #[test]
    fn non_matching_return_type_is_left_untransformed() {
        let (project, _dir, paths) = build(
            "nomatch",
            &[
                ("pagination.d.ts", PAGINATION_DTS),
                ("project.dto.ts", PROJECT_DTO),
                // Handler returns a bare Project, which does not match `DataToPaginate<T>`.
                (
                    "projects.controller.ts",
                    &paginated_controller("Promise<Project>"),
                ),
            ],
        );
        let config = Config::default();
        let (routes, _components) = extract(&config, &project, &[paths[2].clone()]);
        let route = routes.iter().find(|r| r.path == "/projects").unwrap();
        let resp = route.response.as_ref().expect("response schema");
        let refstr = resp.get("$ref").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(refstr, "#/components/schemas/Project");
    }
}
