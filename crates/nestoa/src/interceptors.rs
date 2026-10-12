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
use std::path::Path;

use serde_json::{Value, json};
use swc_ecma_ast::{
    Class, Decorator, Expr, TsArrayType, TsEntityName, TsKeywordTypeKind, TsType,
    TsUnionOrIntersectionType,
};

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
        if let Some(cfg) = config.interceptors.get(name)
            && let Some(RequestBodyTransform::SingleOrArray) = cfg.request_body
        {
            let single = body.schema.clone();
            let array = json!({ "type": "array", "items": single.clone() });
            body.schema = json!({ "anyOf": [single, array] });
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
        components,
        type_env: Some(env),
        current_owner: None,
        instantiating: Vec::new(),
        passthrough_types: &config.passthrough_types,
    };
    let schema = lower_inline(&mut ctx, &current_file, &current_ty);
    if schema.is_null() || schema == json!({}) {
        None
    } else {
        Some(schema)
    }
}

/// Lower the (possibly generic) `Out` type. When it is a bare, *non-generic*
/// reference to a project type, inline that declaration's body under the active
/// substitution env rather than emitting a `$ref` to a shared named component.
/// A *generic* reference (e.g. `PaginatedData<T>` or `MultiStatusResponse<Omit<
/// T, ...>>`) is instead routed through the normal lowering, which — with the
/// env active — instantiates it: it binds the callee's own parameters to the
/// arguments and inlines per route, so distinct element types never collapse
/// onto a single component. Everything else (arrays, type literals, built-ins
/// such as `Omit`, unions, …) also uses the normal lowering.
fn lower_inline(ctx: &mut SchemaCtx, file: &Path, ty: &TsType) -> Value {
    if let TsType::TsTypeRef(r) = ty
        && r.type_params.is_none()
        && let Some(name) = ref_ident(&r.type_name)
        && let Some((decl_file, sym)) = ctx.project.resolve(file, &name)
    {
        return ctx.symbol_to_schema(&decl_file, &sym);
    }
    ctx.ts_type_to_schema(file, ty)
}

/// The `(In, Out)` type arguments of a class's `implements NestInterceptor<In, Out>`.
fn nest_interceptor_args(class: &Class) -> Option<(TsType, TsType)> {
    for imp in &class.implements {
        if let Expr::Ident(id) = &*imp.expr
            && &*id.sym == "NestInterceptor"
        {
            let args = imp.type_args.as_ref()?;
            let in_ty = args.params.first()?;
            let out_ty = args.params.get(1)?;
            return Some(((**in_ty).clone(), (**out_ty).clone()));
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

/// Peel wrappers off a handler return type until the payload type is exposed:
/// `Promise<...>` / `Observable<...>`, and nullish unions (`X | undefined` /
/// `X | null` / `X | void`, as commonly written on handlers that may 404).
/// A nullish union collapses to its sole non-nullish member; a genuine union of
/// several payload types is left intact for `unify` to reject. This mirrors the
/// way the schema lowerer treats `X | undefined` as just `X`, and is what lets
/// an interceptor whose handler returns `Promise<DataToPaginate<T> | undefined>`
/// still bind `T` and produce the transformed `PaginatedData<T>` response.
fn unwrap_async(ty: &TsType) -> &TsType {
    if let TsType::TsTypeRef(r) = ty
        && let Some(name) = ref_ident(&r.type_name)
        && (name == "Promise" || name == "Observable")
        && let Some(args) = &r.type_params
        && let Some(first) = args.params.first()
    {
        return unwrap_async(first);
    }
    if let TsType::TsUnionOrIntersectionType(TsUnionOrIntersectionType::TsUnionType(u)) = ty {
        let mut non_nullish = u.types.iter().filter(|t| !is_nullish(t));
        if let (Some(only), None) = (non_nullish.next(), non_nullish.next()) {
            return unwrap_async(only);
        }
    }
    ty
}

/// Whether a type is `undefined`, `null`, or `void` — union members that carry
/// no wire shape and are dropped when peeling a handler return type.
fn is_nullish(ty: &TsType) -> bool {
    matches!(
        ty,
        TsType::TsKeywordType(k) if matches!(
            k.kind,
            TsKeywordTypeKind::TsUndefinedKeyword
                | TsKeywordTypeKind::TsNullKeyword
                | TsKeywordTypeKind::TsVoidKeyword
        )
    )
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
                resolved: None,
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
    use std::path::PathBuf;
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
        let mut components = ComponentSchemas::default();
        let routes = {
            let mut ex = Extractor {
                config,
                project,
                entry_files: entry,
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

    const TWO_ROUTE_CONTROLLER: &str = r#"
        import { PaginationBodyInterceptor, DataToPaginate } from './pagination';
        import { Project } from './project.dto';
        import { Widget } from './widget.dto';

        @Controller('things')
        export class ThingsController {
            @Get('projects')
            @UseInterceptors(new PaginationBodyInterceptor({ perPageName: 'limit' }))
            public async projects(): Promise<DataToPaginate<Project>> {
                return null as any;
            }

            @Get('widgets')
            @UseInterceptors(new PaginationBodyInterceptor({ perPageName: 'limit' }))
            public async widgets(): Promise<DataToPaginate<Widget>> {
                return null as any;
            }
        }
    "#;

    const SAME_TYPE_TWO_ROUTE_CONTROLLER: &str = r#"
        import { PaginationBodyInterceptor, DataToPaginate } from './pagination';
        import { Project } from './project.dto';

        @Controller('things')
        export class ThingsController {
            @Get('a')
            @UseInterceptors(new PaginationBodyInterceptor({ perPageName: 'limit' }))
            public async a(): Promise<DataToPaginate<Project>> {
                return null as any;
            }

            @Get('b')
            @UseInterceptors(new PaginationBodyInterceptor({ perPageName: 'limit' }))
            public async b(): Promise<DataToPaginate<Project>> {
                return null as any;
            }
        }
    "#;

    /// Mirrors `schema::MAX_INSTANTIATION_DEPTH`.
    const MAX_NEST_LEVELS: usize = 4;

    const PAIR_DTS: &str = r#"
        export interface DataToPaginate<T> {
            resources: T[];
        }
        export interface Pair<A> {
            a: A;
            other: Pair<string>;
        }
        interface PaginatedData<T> {
            resources: Pair<T>[];
        }
        export declare class PaginationBodyInterceptor<T>
            implements NestInterceptor<DataToPaginate<T>, PaginatedData<T>> {
            intercept(): unknown;
        }
    "#;

    const POLY_NEST_DTS: &str = r#"
        export interface DataToPaginate<T> {
            resources: T[];
        }
        export interface Nest<T> {
            child: Nest<Nest<T>>;
        }
        interface PaginatedData<T> {
            resources: Nest<T>[];
        }
        export declare class PaginationBodyInterceptor<T>
            implements NestInterceptor<DataToPaginate<T>, PaginatedData<T>> {
            intercept(): unknown;
        }
    "#;

    const NESTED_BOX_DTS: &str = r#"
        export interface DataToPaginate<T> {
            resources: T[];
        }
        interface Box<U> {
            value: U;
        }
        export declare class PaginationBodyInterceptor<T>
            implements NestInterceptor<DataToPaginate<T>, Box<Box<T>>> {
            intercept(): unknown;
        }
    "#;

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

    const TREE_DTS: &str = r#"
        export interface TreeNode<T> {
            value: T;
            children: TreeNode<T>[];
        }
        interface PaginatedData<T> {
            resources: TreeNode<T>[];
        }
        export interface DataToPaginate<T> {
            resources: T[];
        }
        export declare class PaginationBodyInterceptor<T>
            implements NestInterceptor<DataToPaginate<T>, PaginatedData<T>> {
            intercept(): unknown;
        }
    "#;

    /// A generic type that reaches itself (`TreeNode<T>` holding `TreeNode<T>[]`)
    /// cannot be inlined to a finite schema. Reached through an interceptor's
    /// `Out`, instantiation used to re-enter it without a guard and recurse
    /// until the stack was exhausted (SIGABRT). It now becomes one component
    /// per argument binding, self-referential on the wire.
    /// A polymorphically recursive type — `Nest<T>` holding `Nest<Nest<T>>` —
    /// produces a *fresh* argument binding at every level, so it never repeats
    /// an instantiation and cannot terminate on its own. Each level is lowered
    /// faithfully up to `MAX_INSTANTIATION_DEPTH`, then folded back onto the
    /// outermost instance so the schema closes.
    #[test]
    fn polymorphically_recursive_generic_terminates() {
        let (project, _dir, paths) = build(
            "polynest",
            &[
                ("pagination.d.ts", POLY_NEST_DTS),
                ("project.dto.ts", PROJECT_DTO),
                (
                    "projects.controller.ts",
                    &paginated_controller("Promise<DataToPaginate<Project>>"),
                ),
            ],
        );
        let config = Config::default();
        let (routes, components) = extract(&config, &project, &[paths[2].clone()]);

        let entry = routes
            .iter()
            .find(|r| r.path == "/projects")
            .and_then(|r| r.response.as_ref())
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.get("resources"))
            .and_then(|r| r.get("items"))
            .and_then(|i| i.get("$ref"))
            .and_then(|v| v.as_str())
            .expect("element $ref");

        // Walk `child` from the entry point: the chain must close onto a
        // component already seen rather than run off, and do so within the
        // depth bound.
        let mut seen: Vec<String> = Vec::new();
        let mut cursor = entry.rsplit('/').next().unwrap().to_string();
        loop {
            if seen.contains(&cursor) {
                break;
            }
            seen.push(cursor.clone());
            assert!(seen.len() <= 8, "chain did not close: {seen:?}");
            let node = components
                .schemas
                .get(&cursor)
                .unwrap_or_else(|| panic!("missing component {cursor}"));
            cursor = node
                .get("properties")
                .and_then(|p| p.get("child"))
                .and_then(|c| c.get("$ref"))
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("{cursor}.child is not a $ref"))
                .rsplit('/')
                .next()
                .unwrap()
                .to_string();
        }
        assert_eq!(seen.len(), MAX_NEST_LEVELS, "expanded to the depth bound");
    }

    /// A declaration referring to itself with *different* arguments is not
    /// recursion: `Pair<A>` holding `other: Pair<string>` must lower that field
    /// as its own instantiation, keeping `string`. Guarding on the declaration
    /// alone collapsed it onto the enclosing `Pair<Project>`, so `other.a` came
    /// out as `Project`.
    #[test]
    fn same_declaration_with_other_arguments_is_not_recursion() {
        let (project, _dir, paths) = build(
            "pairargs",
            &[
                ("pagination.d.ts", PAIR_DTS),
                ("project.dto.ts", PROJECT_DTO),
                (
                    "projects.controller.ts",
                    &paginated_controller("Promise<DataToPaginate<Project>>"),
                ),
            ],
        );
        let config = Config::default();
        let (routes, components) = extract(&config, &project, &[paths[2].clone()]);

        let outer = routes
            .iter()
            .find(|r| r.path == "/projects")
            .and_then(|r| r.response.as_ref())
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.get("resources"))
            .and_then(|r| r.get("items"))
            .expect("element schema");

        // The outer binding keeps its own argument...
        assert_eq!(
            outer
                .get("properties")
                .and_then(|p| p.get("a"))
                .and_then(|a| a.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/Project"),
        );
        // ...and `other` resolves to a *separate* instance bound to `string`.
        let other = outer
            .get("properties")
            .and_then(|p| p.get("other"))
            .and_then(|o| o.get("$ref"))
            .and_then(|v| v.as_str())
            .expect("other $ref")
            .rsplit('/')
            .next()
            .unwrap();
        let inner = components.schemas.get(other).expect("inner instance");
        assert_eq!(
            inner
                .get("properties")
                .and_then(|p| p.get("a"))
                .and_then(|a| a.get("type"))
                .and_then(|v| v.as_str()),
            Some("string"),
            "the inner instantiation keeps its own `string` argument",
        );
    }

    /// Two routes binding the same recursive generic to the *same* element type
    /// must share one component. `instantiating` is per-`SchemaCtx` and a
    /// context is built per interceptor rewrite, so without the completed-
    /// instantiation map on `ComponentSchemas` the second route lowered a
    /// duplicate under a fresh name (`TreeNodeProject2`).
    #[test]
    fn identical_recursive_instantiations_share_one_component() {
        let (project, _dir, paths) = build(
            "rectree3",
            &[
                ("pagination.d.ts", TREE_DTS),
                ("project.dto.ts", PROJECT_DTO),
                ("things.controller.ts", SAME_TYPE_TWO_ROUTE_CONTROLLER),
            ],
        );
        let config = Config::default();
        let (routes, components) = extract(&config, &project, &[paths[2].clone()]);

        let element = |path: &str| -> String {
            routes
                .iter()
                .find(|r| r.path == path)
                .and_then(|r| r.response.as_ref())
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.get("resources"))
                .and_then(|r| r.get("items"))
                .and_then(|i| i.get("$ref"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(element("/things/a"), "#/components/schemas/TreeNodeProject");
        assert_eq!(
            element("/things/a"),
            element("/things/b"),
            "both routes reuse one component",
        );
        let dupes: Vec<&String> = components
            .schemas
            .keys()
            .filter(|k| k.starts_with("TreeNode"))
            .collect();
        assert_eq!(
            dupes,
            vec!["TreeNodeProject"],
            "no near-duplicate component"
        );
    }

    #[test]
    fn recursive_generic_under_interceptor_terminates() {
        let (project, _dir, paths) = build(
            "rectree",
            &[
                ("pagination.d.ts", TREE_DTS),
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
        assert_eq!(
            resp.get("properties")
                .and_then(|p| p.get("resources"))
                .and_then(|r| r.get("items"))
                .and_then(|i| i.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/TreeNodeProject"),
            "the recursive element is a component named after its argument",
        );

        let node = components
            .schemas
            .get("TreeNodeProject")
            .expect("recursive component registered");
        // The argument is bound concretely...
        assert_eq!(
            node.get("properties")
                .and_then(|p| p.get("value"))
                .and_then(|v| v.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/Project"),
        );
        // ...and the self reference points back at the same component, which is
        // the finite form of a recursive schema.
        assert_eq!(
            node.get("properties")
                .and_then(|p| p.get("children"))
                .and_then(|c| c.get("items"))
                .and_then(|i| i.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/TreeNodeProject"),
        );
    }

    /// Two routes instantiating the same recursive generic with different
    /// element types must not share one component: keying the in-progress
    /// instantiation on the declaration alone made the second route's children
    /// reference the first route's tree.
    #[test]
    fn recursive_generic_keeps_each_element_type_apart() {
        let (project, _dir, paths) = build(
            "rectree2",
            &[
                ("pagination.d.ts", TREE_DTS),
                ("project.dto.ts", PROJECT_DTO),
                ("widget.dto.ts", "export class Widget { sku: string; }"),
                ("things.controller.ts", TWO_ROUTE_CONTROLLER),
            ],
        );
        let config = Config::default();
        let (routes, components) = extract(&config, &project, &[paths[3].clone()]);

        let element = |path: &str| -> String {
            routes
                .iter()
                .find(|r| r.path == path)
                .and_then(|r| r.response.as_ref())
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.get("resources"))
                .and_then(|r| r.get("items"))
                .and_then(|i| i.get("$ref"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let projects = element("/things/projects");
        let widgets = element("/things/widgets");
        assert_ne!(
            projects, widgets,
            "each element type gets its own component"
        );
        assert_eq!(projects, "#/components/schemas/TreeNodeProject");
        assert_eq!(widgets, "#/components/schemas/TreeNodeWidget");

        // Each component binds its own element type, and recurses into itself.
        for (component, element) in [("TreeNodeProject", "Project"), ("TreeNodeWidget", "Widget")] {
            let node = components.schemas.get(component).expect(component);
            assert_eq!(
                node.get("properties")
                    .and_then(|p| p.get("value"))
                    .and_then(|v| v.get("$ref"))
                    .and_then(|v| v.as_str()),
                Some(format!("#/components/schemas/{element}").as_str()),
            );
            assert_eq!(
                node.get("properties")
                    .and_then(|p| p.get("children"))
                    .and_then(|c| c.get("items"))
                    .and_then(|i| i.get("$ref"))
                    .and_then(|v| v.as_str()),
                Some(format!("#/components/schemas/{component}").as_str()),
            );
        }
    }

    /// `Box<Box<T>>` is finite nesting, not recursion. Marking the declaration
    /// active before lowering its arguments made the inner reference look
    /// re-entrant, stranding the callee's own parameter unbound.
    #[test]
    fn repeated_generic_in_arguments_is_not_recursion() {
        let (project, _dir, paths) = build(
            "boxbox",
            &[
                ("pagination.d.ts", NESTED_BOX_DTS),
                ("project.dto.ts", PROJECT_DTO),
                (
                    "projects.controller.ts",
                    &paginated_controller("Promise<DataToPaginate<Project>>"),
                ),
            ],
        );
        let config = Config::default();
        let (routes, _components) = extract(&config, &project, &[paths[2].clone()]);

        let resp = routes
            .iter()
            .find(|r| r.path == "/projects")
            .and_then(|r| r.response.as_ref())
            .expect("response schema");
        // Both levels inline, and the innermost value is the bound element.
        assert_eq!(
            resp.get("properties")
                .and_then(|p| p.get("value"))
                .and_then(|v| v.get("properties"))
                .and_then(|p| p.get("value"))
                .and_then(|v| v.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/Project"),
        );
        let flat = serde_json::to_string(resp).unwrap();
        assert!(
            !flat.contains("x-unresolved"),
            "no parameter left unbound: {flat}"
        );
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
    fn pagination_interceptor_rewrites_nullable_response() {
        // A handler that may 404 writes `Promise<DataToPaginate<T> | undefined>`.
        // The `| undefined` must be peeled so the interceptor still binds `T` and
        // emits the transformed PaginatedData shape, not the raw generic wrapper
        // with an unbound `T` (which surfaced as `x-unresolved: T`).
        let (project, _dir, paths) = build(
            "pag_null",
            &[
                ("pagination.d.ts", PAGINATION_DTS),
                ("project.dto.ts", PROJECT_DTO),
                (
                    "projects.controller.ts",
                    &paginated_controller("Promise<DataToPaginate<Project> | undefined>"),
                ),
            ],
        );
        let config = Config::default();
        let (routes, components) = extract(&config, &project, &[paths[2].clone()]);

        let route = routes.iter().find(|r| r.path == "/projects").unwrap();
        let resp = route.response.as_ref().expect("response schema");
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

    // A generic interceptor whose `Out` nests a generic element type and reshapes
    // it with `Omit` — the multi-status pattern. `NestInterceptor<T[],
    // Envelope<Omit<T, 'validity'>>>` must instantiate `Envelope`/`Element` with
    // the bound element type per route (not collapse onto a shared component) and
    // apply the `Omit`.
    const MULTISTATUS_DTS: &str = r#"
        export interface Base { validity: boolean; }
        export interface Element<T> {
            status: number;
            resource: T;
        }
        export interface Envelope<T> {
            elements: Element<T>[];
            total: number;
        }
        export declare class MultiStatusInterceptor<T extends Base>
            implements NestInterceptor<T[], Envelope<Omit<T, 'validity'>>> {
            intercept(): unknown;
        }
    "#;

    const WIDGET_DTO: &str = "export class Widget { id: string; validity: boolean; }";

    fn multistatus_controller() -> String {
        r#"
            import { MultiStatusInterceptor } from './multistatus';
            import { Widget } from './widget.dto';

            @Controller('widgets')
            export class WidgetsController {
                @Post()
                @UseInterceptors(MultiStatusInterceptor)
                public async post(): Promise<Widget[]> {
                    return null as any;
                }
            }
        "#
        .to_string()
    }

    #[test]
    fn generic_interceptor_instantiates_nested_element_type() {
        let (project, _dir, paths) = build(
            "multistatus",
            &[
                ("multistatus.d.ts", MULTISTATUS_DTS),
                ("widget.dto.ts", WIDGET_DTO),
                ("widgets.controller.ts", &multistatus_controller()),
            ],
        );
        let config = Config::default();
        let (routes, components) = extract(&config, &project, &[paths[2].clone()]);

        let route = routes.iter().find(|r| r.path == "/widgets").unwrap();
        let resp = route.response.as_ref().expect("response schema");

        // The envelope shape is present: elements[].resource is the bound element
        // type with the interceptor's `Omit<T, 'validity'>` applied.
        let resource = &resp["properties"]["elements"]["items"]["properties"]["resource"];
        assert!(
            resource["properties"].get("id").is_some(),
            "element resource keeps `id`"
        );
        assert!(
            resource["properties"].get("validity").is_none(),
            "Omit strips `validity` from the element resource"
        );

        // Generic wrappers are inlined per route, never emitted as shared
        // components (which would collide across controllers with a different T).
        assert!(!components.schemas.contains_key("Envelope"));
        assert!(!components.schemas.contains_key("Element"));
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

    #[test]
    fn single_or_array_body_is_an_any_of() {
        // An unresolved or unconstrained element also matches the array branch,
        // which `oneOf` would reject.
        let mut config = Config::default();
        config.interceptors.insert(
            "BulkInterceptor".into(),
            crate::config::InterceptorConfig {
                request_body: Some(RequestBodyTransform::SingleOrArray),
                ..Default::default()
            },
        );
        let item = json!({ "x-unresolved": "Item" });
        let mut body = RequestBody {
            schema: item.clone(),
            required: true,
            description: None,
        };
        request_body_transform(&config, &["BulkInterceptor".into()], &mut body);
        assert_eq!(
            body.schema,
            json!({ "anyOf": [item.clone(), { "type": "array", "items": item }] })
        );
    }
}
