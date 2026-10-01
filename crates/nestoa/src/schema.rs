use std::collections::{HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use swc_ecma_ast::{
    Expr, Lit, TsArrayType, TsEntityName, TsKeywordTypeKind, TsLit, TsType, TsTypeElement,
    TsTypeLit, TsTypeParamInstantiation, TsTypeRef, TsUnionOrIntersectionType,
};

use crate::jsdoc::{JsDoc, jsdoc_before};
use crate::symbols::{Project, Symbol};

/// Mutable container for the components/schemas section of the OpenAPI doc.
#[derive(Default)]
pub struct ComponentSchemas {
    pub schemas: IndexMap<String, Value>,
    /// Declaration identity (origin file + type name) -> the component name it
    /// was assigned. The same declaration always maps to one component, so a
    /// type referenced from many places is emitted once; a self-reference
    /// hit during lowering finds the in-progress assignment here, which is how
    /// recursive types terminate.
    by_decl: HashMap<(std::path::PathBuf, String), String>,
    /// Base (sanitized type) name -> the component names registered under it,
    /// in registration order. Used to detect a same-name collision and to
    /// dedupe a later declaration whose lowered schema is structurally equal to
    /// an earlier sibling.
    by_base: HashMap<String, Vec<String>>,
    /// Every component name currently in use (reserved while lowering, or
    /// committed). Guards against handing out a name that is already taken.
    allocated: HashSet<String>,
    /// Recursive instantiations already lowered, keyed by declaration + the
    /// arguments bound to it. Lives here rather than on `SchemaCtx` because a
    /// context is built per interceptor rewrite: without it, a second route
    /// instantiating `TreeNode<Project>` would lower a duplicate component
    /// under a fresh name instead of reusing the first.
    instantiated: HashMap<InstantiationKey, String>,
}

impl ComponentSchemas {
    /// Seed a schema under an exact name (used by tests). Production code goes
    /// through `SchemaCtx::component_ref`, which handles naming and dedup.
    #[cfg(test)]
    pub fn insert(&mut self, name: &str, schema: Value) {
        if !self.schemas.contains_key(name) {
            self.schemas.insert(name.to_string(), schema);
        }
    }

    /// Pick an unused component name for a colliding declaration: prefer
    /// `<owner><base>` (the parent-type prefix), falling back to a numeric
    /// suffix if that is somehow taken too.
    fn fresh_name(&self, owner_prefix: &str, base: &str) -> String {
        let candidate = if owner_prefix.is_empty() {
            base.to_string()
        } else {
            format!("{owner_prefix}{base}")
        };
        if !self.allocated.contains(&candidate) {
            return candidate;
        }
        let mut n = 2;
        loop {
            let c = format!("{candidate}{n}");
            if !self.allocated.contains(&c) {
                return c;
            }
            n += 1;
        }
    }
}

/// A generic type parameter bound to a concrete type, carrying the file the
/// concrete type came from so nested references in it resolve in the right
/// scope. Populated by interceptor unification (see `interceptors.rs`).
#[derive(Clone)]
pub struct TypeBinding {
    pub origin_file: std::path::PathBuf,
    pub ty: TsType,
    /// A pre-lowered schema for this parameter. When set, a reference to the
    /// parameter emits this value verbatim instead of re-lowering `ty`. Used by
    /// generic instantiation (`instantiate_generic`), where an argument such as
    /// `Omit<T, ...>` is lowered under the *outer* env before it shadows the
    /// parameter it binds, so the resolved shape is captured eagerly.
    pub resolved: Option<Value>,
}

/// Map of type-parameter name -> bound concrete type.
pub type TypeEnv = HashMap<String, TypeBinding>;

/// How deep one declaration may be re-instantiated with *fresh* arguments
/// before the expansion is folded back onto the instance already in flight.
/// A type that refers to itself with different arguments is not recursive and
/// must be lowered on its own — `Pair<A>` holding `other: Pair<string>` — but a
/// polymorphically recursive one (`Nest<T>` holding `Nest<Nest<T>>`) produces a
/// fresh binding at every level and would never stop. Real DTOs nest a handful
/// of levels at most, so the bound only ever bites on the pathological case.
const MAX_INSTANTIATION_DEPTH: usize = 4;

/// Identity of one generic instantiation: the declaration it expands, plus the
/// lowered shapes bound to its parameters. Two references to the same generic
/// with different arguments are different instantiations.
pub type InstantiationKey = (std::path::PathBuf, String, String);

/// Build an `InstantiationKey`, signing the bindings by their lowered schemas so
/// the identity follows the concrete shape rather than how it was spelled.
fn instantiation_key(
    decl_file: &Path,
    type_name: &str,
    params: &[String],
    env: &TypeEnv,
) -> InstantiationKey {
    let sig: Vec<&Value> = params
        .iter()
        .filter_map(|p| env.get(p)?.resolved.as_ref())
        .collect();
    (
        decl_file.to_path_buf(),
        type_name.to_string(),
        serde_json::to_string(&sig).unwrap_or_default(),
    )
}

/// Name a recursive instantiation after the components its arguments resolved
/// to (`TreeNode` + `Project` -> `TreeNodeProject`), so sibling instantiations
/// read apart instead of colliding on a numeric suffix.
fn instantiation_suffix(params: &[String], env: &TypeEnv) -> String {
    params
        .iter()
        .filter_map(|p| {
            let r = env.get(p)?.resolved.as_ref()?;
            let name = r.get("$ref")?.as_str()?.rsplit('/').next()?;
            Some(sanitize_name(name))
        })
        .collect()
}

pub struct SchemaCtx<'a> {
    pub project: &'a Project,
    pub sources: &'a HashMap<std::path::PathBuf, String>,
    pub file_starts: &'a HashMap<std::path::PathBuf, swc_common::BytePos>,
    pub components: &'a mut ComponentSchemas,
    /// Active generic substitutions. `None` outside interceptor lowering.
    /// Owned so it can be temporarily replaced when instantiating a nested
    /// generic type (whose own parameters shadow the outer ones).
    pub type_env: Option<TypeEnv>,
    /// User-configured transparent wrapper types (`Config::passthrough_types`),
    /// each lowered to its first type argument. Extends the built-in passthrough
    /// set (`Promise`, `Observable`, `Partial`, …).
    pub passthrough_types: &'a [String],
    /// Component name of the type whose body is currently being lowered. A
    /// same-named child encountered here is disambiguated by prefixing this
    /// owner's name (`Party` + `Identity` -> `PartyIdentity`). `None` at the
    /// top level (route bodies/responses), so the first type keeps its bare name.
    pub current_owner: Option<String>,
    /// The instantiations `instantiate_generic` is currently lowering, outermost
    /// first: the identity, the component name to build if it is ever referred
    /// back to, and that name once allocated. Allocating lazily keeps a
    /// declaration that never refers to itself inlined whole. Arguments are
    /// lowered before an entry is pushed, so a finite nesting (`Box<Box<T>>`)
    /// completes and pops before the outer level pushes, and is never mistaken
    /// for recursion.
    pub instantiating: Vec<(InstantiationKey, String, Option<String>)>,
}

impl<'a> SchemaCtx<'a> {
    pub fn ts_type_to_schema(&mut self, file: &Path, ty: &TsType) -> Value {
        match ty {
            TsType::TsKeywordType(k) => keyword_schema(k.kind),
            TsType::TsLitType(l) => match &l.lit {
                TsLit::Str(s) => {
                    json!({ "type": "string", "enum": [s.value.to_string_lossy().into_owned()] })
                }
                TsLit::Number(n) => json!({ "type": "number", "enum": [n.value] }),
                TsLit::Bool(b) => json!({ "type": "boolean", "enum": [b.value] }),
                _ => json!({}),
            },
            TsType::TsArrayType(TsArrayType { elem_type, .. }) => json!({
                "type": "array",
                "items": self.ts_type_to_schema(file, elem_type),
            }),
            TsType::TsTupleType(t) => {
                let items: Vec<Value> = t
                    .elem_types
                    .iter()
                    .map(|e| self.ts_type_to_schema(file, &e.ty))
                    .collect();
                json!({ "type": "array", "prefixItems": items, "items": false })
            }
            TsType::TsUnionOrIntersectionType(uoi) => self.uoi_schema(file, uoi),
            TsType::TsTypeRef(r) => self.type_ref_schema(file, r),
            TsType::TsTypeLit(lit) => self.type_lit_schema(file, lit),
            TsType::TsParenthesizedType(p) => self.ts_type_to_schema(file, &p.type_ann),
            TsType::TsTypeOperator(_) => json!({}),
            _ => json!({}),
        }
    }

    fn uoi_schema(&mut self, file: &Path, uoi: &TsUnionOrIntersectionType) -> Value {
        match uoi {
            TsUnionOrIntersectionType::TsUnionType(u) => {
                let parts: Vec<Value> = u
                    .types
                    .iter()
                    .map(|t| self.ts_type_to_schema(file, t))
                    .collect();
                let nullable = parts.iter().any(|p| {
                    p.get("type").and_then(|t| t.as_str()) == Some("null")
                        || matches!(
                            p.get("enum"),
                            Some(Value::Array(a)) if a.iter().any(|v| v.is_null()),
                        )
                });
                // Drop the members with no wire representation: the `null` branch
                // (folded into `nullable` above) and the JSON-null placeholder
                // that `void`/`undefined`/`never` lower to — a `T | undefined`
                // property is already expressed by leaving it out of `required`.
                let non_null: Vec<Value> = parts
                    .into_iter()
                    .filter(|p| {
                        p.is_object() && p.get("type").and_then(|t| t.as_str()) != Some("null")
                    })
                    .collect();
                let schema = if non_null.is_empty() {
                    json!({})
                } else if non_null.len() == 1 {
                    non_null.into_iter().next().unwrap()
                } else if non_null
                    .iter()
                    .all(|s| s.get("enum").is_some() && s.get("type") == non_null[0].get("type"))
                {
                    // Merge string literals into a single enum
                    let merged: Vec<Value> = non_null
                        .iter()
                        .flat_map(|s| {
                            s.get("enum")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default()
                        })
                        .collect();
                    let mut out = Map::new();
                    if let Some(t) = non_null[0].get("type") {
                        out.insert("type".into(), t.clone());
                    }
                    out.insert("enum".into(), Value::Array(merged));
                    Value::Object(out)
                } else {
                    json!({ "oneOf": non_null })
                };
                if nullable {
                    make_nullable(schema)
                } else {
                    schema
                }
            }
            TsUnionOrIntersectionType::TsIntersectionType(it) => {
                let parts: Vec<Value> = it
                    .types
                    .iter()
                    .map(|t| self.ts_type_to_schema(file, t))
                    .collect();
                json!({ "allOf": parts })
            }
        }
    }

    fn type_ref_schema(&mut self, file: &Path, r: &TsTypeRef) -> Value {
        // Qualified name like `FileState.OK` — resolve to the literal value of the enum member.
        if let TsEntityName::TsQualifiedName(q) = &r.type_name {
            let parent = entity_name(&q.left);
            let member = q.right.sym.to_string();
            if let Some((decl_file, Symbol::Enum(e))) = self.project.resolve(file, &parent) {
                let _ = decl_file;
                let mut auto: i64 = 0;
                let mut is_string;
                for m in &e.members {
                    let mname = match &m.id {
                        swc_ecma_ast::TsEnumMemberId::Ident(i) => i.sym.to_string(),
                        swc_ecma_ast::TsEnumMemberId::Str(s) => {
                            s.value.to_string_lossy().into_owned()
                        }
                    };
                    let value = match m.init.as_deref() {
                        Some(Expr::Lit(Lit::Str(s))) => {
                            is_string = true;
                            Value::String(s.value.to_string_lossy().into_owned())
                        }
                        Some(Expr::Lit(Lit::Num(n))) => {
                            is_string = false;
                            let v = Value::from(n.value);
                            auto = (n.value as i64) + 1;
                            v
                        }
                        _ => {
                            is_string = false;
                            let v = Value::from(auto);
                            auto += 1;
                            v
                        }
                    };
                    if mname == member {
                        let ty = if is_string { "string" } else { "number" };
                        return json!({ "type": ty, "enum": [value] });
                    }
                }
            }
            // Namespace-qualified type, e.g. `Enums.AccountType` where `Enums`
            // is a TS namespace (`import * as Enums` / `export * as Enums`).
            if let Some((decl_file, sym)) = self
                .project
                .resolve_namespace_member(file, &parent, &member)
            {
                return self.component_ref(&decl_file, &member, &sym);
            }
            // Unknown — fall through and return placeholder.
            return json!({ "x-unresolved": format!("{}.{}", parent, member) });
        }
        let name = entity_name(&r.type_name);
        // Generic substitution: a bare reference to a bound interceptor type
        // parameter (e.g. `T`) lowers to the bound concrete type, resolved in
        // the file that type came from. The env is cleared for the recursion
        // because bound types come from the concrete side and never reference
        // the interceptor's own parameters (avoids param->param loops).
        if r.type_params.is_none()
            && let Some(binding) = self
                .type_env
                .as_ref()
                .and_then(|env| env.get(&name))
                .cloned()
        {
            // Instantiation captured a pre-lowered shape — emit it directly.
            if let Some(resolved) = binding.resolved {
                return resolved;
            }
            let saved = self.type_env.take();
            let schema = self.ts_type_to_schema(&binding.origin_file, &binding.ty);
            self.type_env = saved;
            return schema;
        }
        // User-configured transparent wrappers: lower to the first type argument
        // (a bare reference with no args lowers as the wrapper name itself).
        if self.passthrough_types.iter().any(|t| t == &name)
            && let Some(args) = &r.type_params
            && let Some(first) = args.params.first()
        {
            return self.ts_type_to_schema(file, first);
        }
        // Built-in generic handling
        match name.as_str() {
            "Promise" | "Observable" => {
                if let Some(args) = &r.type_params
                    && let Some(first) = args.params.first()
                {
                    return self.ts_type_to_schema(file, first);
                }
                return json!({});
            }
            "Array" | "ReadonlyArray" => {
                if let Some(args) = &r.type_params
                    && let Some(first) = args.params.first()
                {
                    return json!({ "type": "array", "items": self.ts_type_to_schema(file, first) });
                }
                return json!({ "type": "array" });
            }
            "Partial" | "Required" => {
                if let Some(args) = &r.type_params
                    && let Some(first) = args.params.first()
                {
                    let base = self.ts_type_to_schema(file, first);
                    let edit = if name == "Partial" {
                        ObjectEdit::AllOptional
                    } else {
                        ObjectEdit::AllRequired
                    };
                    return self.reshape_object(base, &edit);
                }
            }
            "Readonly" | "NonNullable" => {
                if let Some(args) = &r.type_params
                    && let Some(first) = args.params.first()
                {
                    return self.ts_type_to_schema(file, first);
                }
            }
            "Omit" | "Pick" => {
                if let Some(args) = &r.type_params
                    && let Some(first) = args.params.first()
                {
                    let base = self.ts_type_to_schema(file, first);
                    let keys = args
                        .params
                        .get(1)
                        .map(|k| collect_string_keys(k))
                        .unwrap_or_default();
                    let edit = if name == "Omit" {
                        ObjectEdit::Omit(keys)
                    } else {
                        ObjectEdit::Pick(keys)
                    };
                    return self.reshape_object(base, &edit);
                }
            }
            "Record" => {
                if let Some(args) = &r.type_params
                    && let Some(value_t) = args.params.get(1)
                {
                    return json!({
                        "type": "object",
                        "additionalProperties": self.ts_type_to_schema(file, value_t),
                    });
                }
                return json!({ "type": "object" });
            }
            "Date" => return json!({ "type": "string", "format": "date-time" }),
            "ObjectId" | "Buffer" => return json!({ "type": "string" }),
            _ => {}
        }

        // Resolve to a project symbol.
        if let Some((decl_file, sym)) = self.project.resolve(file, &name) {
            // During interceptor response lowering, a named reference carrying
            // type arguments (e.g. `MultiStatusResponse<Omit<T, ...>>`) is
            // instantiated: the declaration's own generic parameters are bound
            // to the arguments and its body inlined per route, so distinct
            // element types never collapse onto one shared `$ref` component.
            if self.type_env.is_some()
                && let Some(args) = &r.type_params
                && let Some(inlined) = self.instantiate_generic(file, &decl_file, &name, &sym, args)
            {
                return inlined;
            }
            return self.component_ref(&decl_file, &name, &sym);
        }
        // Unknown — leave a permissive placeholder annotated with the name.
        json!({ "x-unresolved": name })
    }

    /// Resolve a bare type name (written as it would appear in source, looked
    /// up against `file`'s imports) into a schema, registering it as a reusable
    /// component. An unresolvable name yields an `x-unresolved` placeholder,
    /// exactly as an unresolved `TsTypeRef` would. Used by `@route-mapping`,
    /// where the type is named in JSDoc rather than in the AST.
    pub fn named_type_to_schema(&mut self, file: &Path, type_name: &str) -> Value {
        if let Some((decl_file, sym)) = self.project.resolve(file, type_name) {
            self.component_ref(&decl_file, type_name, &sym)
        } else {
            json!({ "x-unresolved": type_name })
        }
    }

    /// Resolve a reference to a named project type into a `$ref`, registering
    /// the type as a reusable component on first encounter.
    ///
    /// Naming rules:
    /// - The same declaration (same origin file + type name) is always emitted
    ///   as one component, regardless of how many places reference it.
    /// - The first declaration to claim a given base name keeps it bare.
    /// - A *different* declaration that wants an already-taken base name is
    ///   either deduped against a structurally-equal sibling, or — when its
    ///   shape differs — registered under a fresh name prefixed with the
    ///   enclosing type's name (`Party` + `Identity` -> `PartyIdentity`).
    fn component_ref(&mut self, decl_file: &Path, type_name: &str, sym: &Symbol) -> Value {
        let key = (decl_file.to_path_buf(), type_name.to_string());
        // Already assigned: a repeat reference, or an in-progress recursion
        // that must resolve to the name reserved below.
        if let Some(name) = self.components.by_decl.get(&key) {
            return ref_value(name);
        }
        let base = sanitize_name(type_name);
        // First declaration to claim this base keeps it bare; a later, distinct
        // one is disambiguated with its owner's name.
        let provisional = if self.components.allocated.contains(&base) {
            let owner = self
                .current_owner
                .clone()
                .map(|o| sanitize_name(&o))
                .unwrap_or_default();
            self.components.fresh_name(&owner, &base)
        } else {
            base.clone()
        };
        // Reserve and record the assignment *before* lowering, so a self
        // reference inside the body resolves back to this name.
        self.components.allocated.insert(provisional.clone());
        self.components
            .by_decl
            .insert(key.clone(), provisional.clone());
        // Lower with this type as the owner, so a same-named child is prefixed
        // by *this* type's name rather than an outer one.
        let saved_owner = self.current_owner.replace(provisional.clone());
        let schema = self.symbol_to_schema(decl_file, sym);
        self.current_owner = saved_owner;
        // A collision (provisional != base) may still be a duplicate: reuse an
        // earlier sibling with an identical shape instead of adding a copy.
        if provisional != base
            && let Some(siblings) = self.components.by_base.get(&base).cloned()
        {
            for existing in siblings {
                if self.components.schemas.get(&existing) == Some(&schema) {
                    self.components.allocated.remove(&provisional);
                    self.components.by_decl.insert(key, existing.clone());
                    return ref_value(&existing);
                }
            }
        }
        self.components.schemas.insert(provisional.clone(), schema);
        self.components
            .by_base
            .entry(base)
            .or_default()
            .push(provisional.clone());
        ref_value(&provisional)
    }

    /// Instantiate a generic project type reference (`Name<Arg1, ...>`) by
    /// binding the declaration's own type parameters to the supplied arguments
    /// and inlining its body. Returns `None` when the declaration has no type
    /// parameters (a non-generic reference, which keeps its shared `$ref`).
    ///
    /// Each argument is lowered eagerly under the *current* env before the new
    /// bindings take effect, so an argument that references an outer parameter
    /// (`Omit<T, ...>` where `T` is bound by an interceptor) resolves in the
    /// scope it was written in even though the callee reuses the name `T` for
    /// its own parameter. The body is then lowered under only the fresh
    /// bindings, and inlined rather than `$ref`d so distinct instantiations
    /// (different element types) do not collide on one component. The exception
    /// is a declaration that reaches itself, which cannot be inlined finitely:
    /// it becomes one component per distinct argument binding.
    fn instantiate_generic(
        &mut self,
        file: &Path,
        decl_file: &Path,
        type_name: &str,
        sym: &Symbol,
        args: &TsTypeParamInstantiation,
    ) -> Option<Value> {
        let params = generic_param_names(sym);
        if params.is_empty() {
            return None;
        }
        // Arguments are lowered in the *caller's* scope, before this
        // declaration is marked active: they belong to the reference site, not
        // to the body about to be expanded. Marking first would read a finite
        // nesting like `Box<Box<T>>` as recursion and leave the inner
        // argument's own parameter unbound.
        let mut new_env: TypeEnv = HashMap::new();
        for (param, arg) in params.iter().zip(args.params.iter()) {
            let resolved = self.ts_type_to_schema(file, arg);
            new_env.insert(
                param.clone(),
                TypeBinding {
                    origin_file: file.to_path_buf(),
                    ty: (**arg).clone(),
                    resolved: Some(resolved),
                },
            );
        }
        let key = instantiation_key(decl_file, type_name, &params, &new_env);
        // Already lowered — by an earlier route, or an earlier field of this
        // one. Recursive instantiations are components, so reuse the existing
        // one rather than lowering a duplicate under a fresh name.
        if let Some(name) = self.components.instantiated.get(&key) {
            return Some(ref_value(name));
        }
        // The same declaration bound to the same arguments, still in flight:
        // genuine recursion, with no finite inlining. Point at the instance
        // already being lowered.
        if let Some(i) = self.instantiating.iter().position(|(k, ..)| *k == key) {
            let name = self.reserve_instantiation_name(i);
            return Some(ref_value(&name));
        }
        // A *different* binding of a declaration in flight is not recursion —
        // `Pair<Project>` holding `other: Pair<string>` — so it is lowered on
        // its own and keeps its own argument. Only a declaration that keeps
        // producing fresh bindings fails to terminate that way, so past
        // `MAX_INSTANTIATION_DEPTH` the expansion folds onto the outermost
        // instance of it in flight.
        let deep = self
            .instantiating
            .iter()
            .filter(|(k, ..)| k.0 == decl_file && k.1 == type_name)
            .count()
            >= MAX_INSTANTIATION_DEPTH;
        if deep
            && let Some(i) = self
                .instantiating
                .iter()
                .position(|(k, ..)| k.0 == decl_file && k.1 == type_name)
        {
            let name = self.reserve_instantiation_name(i);
            return Some(ref_value(&name));
        }
        // The name this instance would take if something refers back to it,
        // built from its own arguments so the component is named after what it
        // actually holds.
        let hint = format!(
            "{}{}",
            sanitize_name(type_name),
            instantiation_suffix(&params, &new_env)
        );
        self.instantiating.push((key.clone(), hint, None));
        let saved = self.type_env.replace(new_env);
        let schema = self.symbol_to_schema(decl_file, sym);
        self.type_env = saved;
        // A name is present only if something referred back to this instance.
        // Registering the finished schema under it makes every level of the
        // recursion share one component, and recording the instantiation makes
        // a later route binding the same arguments reuse it.
        let (_, _, name) = self.instantiating.pop().expect("instantiation in flight");
        match name {
            Some(name) => {
                self.components.schemas.insert(name.clone(), schema);
                self.components.instantiated.insert(key, name.clone());
                Some(ref_value(&name))
            }
            None => Some(schema),
        }
    }

    /// The component name for the instantiation at `idx`, allocated on first
    /// use: an instance nothing refers back to is inlined and never named.
    fn reserve_instantiation_name(&mut self, idx: usize) -> String {
        if let Some(name) = &self.instantiating[idx].2 {
            return name.clone();
        }
        let hint = self.instantiating[idx].1.clone();
        let name = self.components.fresh_name("", &hint);
        self.components.allocated.insert(name.clone());
        self.instantiating[idx].2 = Some(name.clone());
        name
    }

    fn type_lit_schema(&mut self, file: &Path, lit: &TsTypeLit) -> Value {
        let mut props: IndexMap<String, Value> = IndexMap::new();
        let mut required: Vec<String> = Vec::new();
        for el in &lit.members {
            if let TsTypeElement::TsPropertySignature(p) = el {
                let name = match &*p.key {
                    Expr::Ident(i) => i.sym.to_string(),
                    Expr::Lit(Lit::Str(s)) => s.value.to_string_lossy().into_owned(),
                    _ => continue,
                };
                // `@internal`/`@private` members never reach the wire.
                if self
                    .sources
                    .get(file)
                    .zip(self.file_starts.get(file))
                    .and_then(|(src, file_start)| jsdoc_before(src, *file_start, p.span))
                    .is_some_and(|d| d.internal)
                {
                    continue;
                }
                let schema = p
                    .type_ann
                    .as_ref()
                    .map(|t| self.ts_type_to_schema(file, &t.type_ann))
                    .unwrap_or_else(|| json!({}));
                props.insert(name.clone(), schema);
                if !p.optional {
                    required.push(name);
                }
            }
        }
        let mut out = Map::new();
        out.insert("type".into(), Value::String("object".into()));
        out.insert(
            "properties".into(),
            Value::Object(props.into_iter().collect()),
        );
        if !required.is_empty() {
            out.insert(
                "required".into(),
                Value::Array(required.into_iter().map(Value::String).collect()),
            );
        }
        Value::Object(out)
    }

    /// The schema a class inherits from its `extends` clause: a plain class,
    /// or a `@nestjs/mapped-types` / `@nestjs/swagger` helper call, which may
    /// nest (`PartialType(OmitType(CreateDto, ['id']))`).
    fn heritage_schema(
        &mut self,
        file: &Path,
        expr: &Expr,
        type_args: Option<&TsTypeParamInstantiation>,
    ) -> Option<Value> {
        match expr {
            Expr::Ident(id) => {
                let reference = TsTypeRef {
                    span: id.span,
                    type_name: TsEntityName::Ident(id.clone()),
                    type_params: type_args.map(|a| Box::new(a.clone())),
                };
                Some(self.type_ref_schema(file, &reference))
            }
            Expr::Paren(p) => self.heritage_schema(file, &p.expr, type_args),
            Expr::Call(call) => {
                let swc_ecma_ast::Callee::Expr(callee) = &call.callee else {
                    return None;
                };
                let Expr::Ident(helper) = callee.as_ref() else {
                    return None;
                };
                let mut args = call.args.iter().map(|a| a.expr.as_ref());
                match helper.sym.as_ref() {
                    "IntersectionType" => {
                        let parts: Vec<Value> = args
                            .filter_map(|a| self.heritage_schema(file, a, None))
                            .collect();
                        Some(json!({ "allOf": parts }))
                    }
                    "PartialType" | "OmitType" | "PickType" => {
                        let base = self.heritage_schema(file, args.next()?, None)?;
                        let keys = args.next().map(string_array_keys).unwrap_or_default();
                        let edit = match helper.sym.as_ref() {
                            "PartialType" => ObjectEdit::AllOptional,
                            "OmitType" => ObjectEdit::Omit(keys),
                            _ => ObjectEdit::Pick(keys),
                        };
                        Some(self.reshape_object(base, &edit))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Apply a property-set edit (`Partial`, `Omit`, …) to an object schema,
    /// inlining it first so inherited properties are edited too. A schema that
    /// cannot be flattened (a union, or a component still being lowered) is
    /// returned unchanged.
    fn reshape_object(&self, schema: Value, edit: &ObjectEdit) -> Value {
        let Some(mut obj) = self.flatten_object(&schema, 0) else {
            return schema;
        };
        let keys: Vec<String> = match obj.get("properties") {
            Some(Value::Object(props)) => props.keys().cloned().collect(),
            _ => Vec::new(),
        };
        let mut required: Vec<String> = match obj.remove("required") {
            Some(Value::Array(r)) => r
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        match edit {
            ObjectEdit::AllOptional => required.clear(),
            ObjectEdit::AllRequired => required = keys,
            ObjectEdit::Omit(drop) | ObjectEdit::Pick(drop) => {
                let keep =
                    |k: &str| drop.iter().any(|d| d == k) == matches!(edit, ObjectEdit::Pick(_));
                if let Some(Value::Object(props)) = obj.get_mut("properties") {
                    props.retain(|k, _| keep(k));
                }
                required.retain(|k| keep(k));
            }
        }
        if !required.is_empty() {
            obj.insert(
                "required".into(),
                Value::Array(required.into_iter().map(Value::String).collect()),
            );
        }
        Value::Object(obj)
    }

    /// Inline `schema` into a single object: a `$ref` is replaced by its
    /// component and `allOf` branches are merged, their `properties` and
    /// `required` combined; a property several branches declare keeps every
    /// constraint, as an `allOf`. `None` for anything that is not object-shaped.
    fn flatten_object(&self, schema: &Value, depth: usize) -> Option<Map<String, Value>> {
        const MAX_DEPTH: usize = 32;
        let Value::Object(map) = schema else {
            return None;
        };
        if depth > MAX_DEPTH {
            return None;
        }
        let siblings = |skip: &str| -> Map<String, Value> {
            let mut m = map.clone();
            m.remove(skip);
            m
        };
        if let Some(target) = map.get("$ref").and_then(Value::as_str) {
            let name = target.strip_prefix("#/components/schemas/")?;
            let mut out = self.flatten_object(self.components.schemas.get(name)?, depth + 1)?;
            out.extend(siblings("$ref"));
            return Some(out);
        }
        if let Some(Value::Array(branches)) = map.get("allOf") {
            let mut out = Map::new();
            let mut props = Map::new();
            let mut required: Vec<Value> = Vec::new();
            for branch in branches {
                let mut b = self.flatten_object(branch, depth + 1)?;
                if let Some(Value::Object(p)) = b.remove("properties") {
                    for (k, v) in p {
                        match props.get_mut(&k) {
                            Some(existing) if *existing != v => {
                                *existing = json!({ "allOf": [existing.take(), v] });
                            }
                            Some(_) => {}
                            None => {
                                props.insert(k, v);
                            }
                        }
                    }
                }
                if let Some(Value::Array(r)) = b.remove("required") {
                    for k in r {
                        if !required.contains(&k) {
                            required.push(k);
                        }
                    }
                }
                out.extend(b);
            }
            out.extend(siblings("allOf"));
            out.insert("type".into(), Value::String("object".into()));
            out.insert("properties".into(), Value::Object(props));
            if !required.is_empty() {
                out.insert("required".into(), Value::Array(required));
            }
            return Some(out);
        }
        let is_object = map.get("type").and_then(Value::as_str) == Some("object")
            || map.contains_key("properties");
        is_object.then(|| map.clone())
    }

    pub fn symbol_to_schema(&mut self, file: &Path, sym: &Symbol) -> Value {
        match sym {
            Symbol::Interface(i) => {
                let mut props: IndexMap<String, Value> = IndexMap::new();
                let mut required: Vec<String> = Vec::new();
                let mut all_of: Vec<Value> = Vec::new();
                for parent in &i.extends {
                    if let Expr::Ident(id) = &*parent.expr {
                        let name = id.sym.to_string();
                        let dummy_ref = TsTypeRef {
                            span: parent.span,
                            type_name: TsEntityName::Ident(id.clone()),
                            type_params: parent.type_args.clone(),
                        };
                        all_of.push(self.type_ref_schema(file, &dummy_ref));
                        let _ = name;
                    }
                }
                for el in &i.body.body {
                    if let TsTypeElement::TsPropertySignature(p) = el {
                        let name = match &*p.key {
                            Expr::Ident(i) => i.sym.to_string(),
                            Expr::Lit(Lit::Str(s)) => s.value.to_string_lossy().into_owned(),
                            _ => continue,
                        };
                        let doc = self
                            .sources
                            .get(file)
                            .zip(self.file_starts.get(file))
                            .and_then(|(src, file_start)| jsdoc_before(src, *file_start, p.span));
                        // `@internal`/`@private` members never reach the wire.
                        if doc.as_ref().is_some_and(|d| d.internal) {
                            continue;
                        }
                        let mut schema = p
                            .type_ann
                            .as_ref()
                            .map(|t| self.ts_type_to_schema(file, &t.type_ann))
                            .unwrap_or_else(|| json!({}));
                        if let Some(doc) = &doc {
                            attach_doc(&mut schema, doc);
                        }
                        props.insert(name.clone(), schema);
                        if !p.optional {
                            required.push(name);
                        }
                    }
                }
                let mut object = Map::new();
                object.insert("type".into(), Value::String("object".into()));
                object.insert(
                    "properties".into(),
                    Value::Object(props.into_iter().collect()),
                );
                if !required.is_empty() {
                    object.insert(
                        "required".into(),
                        Value::Array(required.into_iter().map(Value::String).collect()),
                    );
                }
                if let (Some(src), Some(file_start)) =
                    (self.sources.get(file), self.file_starts.get(file))
                    && let Some(doc) = jsdoc_before(src, *file_start, i.span)
                {
                    attach_doc_to_map(&mut object, &doc);
                }
                let object_schema = Value::Object(object);
                if all_of.is_empty() {
                    object_schema
                } else {
                    all_of.push(object_schema);
                    json!({ "allOf": all_of })
                }
            }
            Symbol::TypeAlias(t) => self.ts_type_to_schema(file, &t.type_ann),
            Symbol::Enum(e) => {
                // Distinguish string-enums from numeric ones based on the first initialiser.
                let mut values: Vec<Value> = Vec::new();
                let mut is_string = true;
                let mut auto = 0i64;
                for m in &e.members {
                    let v = match &m.init {
                        Some(expr) => match expr.as_ref() {
                            Expr::Lit(Lit::Str(s)) => {
                                is_string = true;
                                Value::String(s.value.to_string_lossy().into_owned())
                            }
                            Expr::Lit(Lit::Num(n)) => {
                                is_string = false;
                                Value::from(n.value)
                            }
                            _ => {
                                // Fallback: use the member name.
                                Value::String(prop_name(&m.id))
                            }
                        },
                        None => {
                            is_string = false;
                            let v = Value::from(auto);
                            auto += 1;
                            v
                        }
                    };
                    values.push(v);
                }
                let mut obj = Map::new();
                obj.insert(
                    "type".into(),
                    Value::String(if is_string {
                        "string".into()
                    } else {
                        "number".into()
                    }),
                );
                obj.insert("enum".into(), Value::Array(values));
                if let (Some(src), Some(file_start)) =
                    (self.sources.get(file), self.file_starts.get(file))
                    && let Some(doc) = jsdoc_before(src, *file_start, e.span)
                {
                    attach_doc_to_map(&mut obj, &doc);
                }
                Value::Object(obj)
            }
            Symbol::Class(c) => {
                let mut props: IndexMap<String, Value> = IndexMap::new();
                let mut required: Vec<String> = Vec::new();
                // A `class X extends Y` carries Y's members too. Mirror the
                // interface-`extends` handling: emit Y as an allOf branch.
                let mut all_of: Vec<Value> = Vec::new();
                if let Some(super_expr) = &c.class.super_class
                    && let Some(parent) =
                        self.heritage_schema(file, super_expr, c.class.super_type_params.as_deref())
                {
                    all_of.push(parent);
                }
                for member in &c.class.body {
                    if let swc_ecma_ast::ClassMember::ClassProp(p) = member {
                        // A `static` member lives on the constructor, never on
                        // an instance, so it is never part of the serialized
                        // body — and it routinely holds internals (a logger, a
                        // cache) whose types would otherwise be dragged into the
                        // components. Accessibility is deliberately *not*
                        // considered: TypeScript erases `private`, so such a
                        // field is still serialized and still belongs on the
                        // wire. Use the `@internal`/`@private` JSDoc tag to drop
                        // one on purpose.
                        if p.is_static {
                            continue;
                        }
                        let name = match &p.key {
                            swc_ecma_ast::PropName::Ident(i) => i.sym.to_string(),
                            swc_ecma_ast::PropName::Str(s) => {
                                s.value.to_string_lossy().into_owned()
                            }
                            _ => continue,
                        };
                        let doc = self
                            .sources
                            .get(file)
                            .zip(self.file_starts.get(file))
                            .and_then(|(src, file_start)| jsdoc_before(src, *file_start, p.span));
                        // `@internal`/`@private` members never reach the wire.
                        if doc.as_ref().is_some_and(|d| d.internal) {
                            continue;
                        }
                        let mut schema = p
                            .type_ann
                            .as_ref()
                            .map(|t| self.ts_type_to_schema(file, &t.type_ann))
                            .unwrap_or_else(|| json!({}));
                        if let Some(doc) = &doc {
                            attach_doc(&mut schema, doc);
                        }
                        // class-validator `@IsOptional()` decorator implies optional.
                        let has_is_optional = p
                            .decorators
                            .iter()
                            .any(|d| decorator_name(d) == "IsOptional");
                        props.insert(name.clone(), schema);
                        if !p.is_optional && !has_is_optional {
                            required.push(name);
                        }
                    }
                }
                let mut obj = Map::new();
                obj.insert("type".into(), Value::String("object".into()));
                obj.insert(
                    "properties".into(),
                    Value::Object(props.into_iter().collect()),
                );
                if !required.is_empty() {
                    obj.insert(
                        "required".into(),
                        Value::Array(required.into_iter().map(Value::String).collect()),
                    );
                }
                if let (Some(src), Some(file_start)) =
                    (self.sources.get(file), self.file_starts.get(file))
                    && let Some(doc) = jsdoc_before(src, *file_start, c.class.span)
                {
                    attach_doc_to_map(&mut obj, &doc);
                }
                let object_schema = Value::Object(obj);
                if all_of.is_empty() {
                    object_schema
                } else {
                    all_of.push(object_schema);
                    json!({ "allOf": all_of })
                }
            }
        }
    }
}

fn prop_name(p: &swc_ecma_ast::TsEnumMemberId) -> String {
    match p {
        swc_ecma_ast::TsEnumMemberId::Ident(i) => i.sym.to_string(),
        swc_ecma_ast::TsEnumMemberId::Str(s) => s.value.to_string_lossy().into_owned(),
    }
}

fn entity_name(e: &TsEntityName) -> String {
    match e {
        TsEntityName::Ident(i) => i.sym.to_string(),
        TsEntityName::TsQualifiedName(q) => {
            format!("{}.{}", entity_name(&q.left), q.right.sym)
        }
    }
}

/// The names of a declaration's own generic type parameters, in order
/// (`["T"]` for `interface Foo<T>`). Empty for a non-generic declaration.
fn generic_param_names(sym: &Symbol) -> Vec<String> {
    let type_params = match sym {
        Symbol::Interface(i) => i.type_params.as_ref(),
        Symbol::TypeAlias(a) => a.type_params.as_ref(),
        Symbol::Class(c) => c.class.type_params.as_ref(),
        Symbol::Enum(_) => None,
    };
    type_params
        .map(|tp| tp.params.iter().map(|p| p.name.sym.to_string()).collect())
        .unwrap_or_default()
}

fn keyword_schema(k: TsKeywordTypeKind) -> Value {
    use TsKeywordTypeKind::*;
    match k {
        TsStringKeyword => json!({ "type": "string" }),
        TsNumberKeyword => json!({ "type": "number" }),
        TsBooleanKeyword => json!({ "type": "boolean" }),
        TsBigIntKeyword => json!({ "type": "integer", "format": "int64" }),
        TsNullKeyword => json!({ "type": "null" }),
        TsAnyKeyword | TsUnknownKeyword | TsObjectKeyword => json!({}),
        TsVoidKeyword | TsUndefinedKeyword | TsNeverKeyword => Value::Null,
        TsSymbolKeyword | TsIntrinsicKeyword => json!({}),
    }
}

/// A property-set edit, as made by `Partial`/`Required`/`Omit`/`Pick` and
/// their `@nestjs/mapped-types` counterparts.
enum ObjectEdit {
    AllOptional,
    AllRequired,
    Omit(Vec<String>),
    Pick(Vec<String>),
}

/// The string literals of an array expression such as the key list of
/// `OmitType(Dto, ['a', 'b'] as const)`.
fn string_array_keys(expr: &Expr) -> Vec<String> {
    match expr {
        Expr::TsConstAssertion(c) => string_array_keys(&c.expr),
        Expr::TsAs(a) => string_array_keys(&a.expr),
        Expr::Paren(p) => string_array_keys(&p.expr),
        Expr::Array(arr) => arr
            .elems
            .iter()
            .flatten()
            .filter_map(|el| match el.expr.as_ref() {
                Expr::Lit(Lit::Str(s)) => Some(s.value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Extract string literal keys from a TS type expression of the form
/// `'a' | 'b'` or just `'a'`.
fn collect_string_keys(ty: &TsType) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(ty: &TsType, out: &mut Vec<String>) {
        match ty {
            TsType::TsLitType(l) => {
                if let TsLit::Str(s) = &l.lit {
                    out.push(s.value.to_string_lossy().into_owned());
                }
            }
            TsType::TsUnionOrIntersectionType(TsUnionOrIntersectionType::TsUnionType(u)) => {
                for t in &u.types {
                    walk(t, out);
                }
            }
            _ => {}
        }
    }
    walk(ty, &mut out);
    out
}

/// Build a `$ref` JSON value pointing at a named component schema.
fn ref_value(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

pub fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn decorator_name(d: &swc_ecma_ast::Decorator) -> String {
    match &*d.expr {
        Expr::Call(c) => {
            if let swc_ecma_ast::Callee::Expr(e) = &c.callee {
                callee_name(e)
            } else {
                String::new()
            }
        }
        e => callee_name(e),
    }
}

fn callee_name(e: &Expr) -> String {
    match e {
        Expr::Ident(i) => i.sym.to_string(),
        Expr::Member(m) => {
            let prop = match &m.prop {
                swc_ecma_ast::MemberProp::Ident(i) => i.sym.to_string(),
                _ => String::new(),
            };
            format!("{}.{}", callee_name(&m.obj), prop)
        }
        _ => String::new(),
    }
}

/// Encode "may also be null" the OpenAPI 3.1 way.
///
/// 3.0's `nullable: true` keyword does not exist in 3.1 (whose schemas are
/// JSON Schema 2020-12): a 3.1 consumer ignores it outright, and even under 3.0
/// semantics it only means something next to a `type` keyword — beside a `$ref`
/// or a `oneOf` it was a silent no-op that linters flag (Redocly's
/// `nullable-type-sibling`). So widen the `type` keyword instead, and where
/// there is no type to widen add an explicit `{ "type": "null" }` branch.
fn make_nullable(schema: Value) -> Value {
    let Value::Object(mut m) = schema else {
        return schema;
    };
    // Unconstrained (`any`/`unknown`) already admits null; adding a branch to
    // `{}` would only narrow nothing into noise.
    if m.is_empty() {
        return Value::Object(m);
    }
    // An `enum` constrains the value list independently of `type`, so `null`
    // has to be listed there too.
    if let Some(Value::Array(values)) = m.get_mut("enum")
        && !values.iter().any(Value::is_null)
    {
        values.push(Value::Null);
    }
    match m.get("type").cloned() {
        Some(Value::String(t)) => {
            m.insert("type".into(), json!([t, "null"]));
            return Value::Object(m);
        }
        Some(Value::Array(mut types)) => {
            if !types.iter().any(|t| t.as_str() == Some("null")) {
                types.push(Value::String("null".into()));
            }
            m.insert("type".into(), Value::Array(types));
            return Value::Object(m);
        }
        _ => {}
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(branches)) = m.get_mut(key) {
            if !branches
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("null"))
            {
                branches.push(json!({ "type": "null" }));
            }
            return Value::Object(m);
        }
    }
    // `$ref`, `allOf`, `x-unresolved`: no type keyword of our own to widen.
    json!({ "oneOf": [Value::Object(m), { "type": "null" }] })
}

/// Make a schema map a legal place to hang annotation keywords.
///
/// A `$ref` with siblings is valid 3.1, but the JSON Reference semantics of
/// 3.0.x (and a lot of tooling that still follows them, Swagger Editor
/// included: "All other properties in a $ref object are ignored") discard
/// everything beside the `$ref`. Moving the reference into
/// `allOf: [{ $ref }]` keeps the same meaning in 3.1 while giving the
/// siblings a real schema object to apply to in 3.0 consumers too.
fn ref_to_allof(m: &mut Map<String, Value>) {
    let Some(r) = m.remove("$ref") else {
        return;
    };
    let mut branches = vec![json!({ "$ref": r })];
    if let Some(Value::Array(existing)) = m.remove("allOf") {
        branches.extend(existing);
    }
    m.insert("allOf".into(), Value::Array(branches));
}

/// Give `schema` a `description`, unless it already has one.
pub fn describe(schema: &mut Value, description: &str) {
    if let Value::Object(m) = schema
        && !m.contains_key("description")
    {
        ref_to_allof(m);
        m.insert("description".into(), Value::String(description.to_string()));
    }
}

fn attach_doc(schema: &mut Value, doc: &JsDoc) {
    if let Value::Object(m) = schema {
        attach_doc_to_map(m, doc);
    }
}

fn attach_doc_to_map(m: &mut Map<String, Value>, doc: &JsDoc) {
    let combined = match (&doc.summary, &doc.description) {
        (Some(s), Some(d)) => Some(format!("{s}\n\n{d}")),
        (Some(s), None) => Some(s.clone()),
        (None, Some(d)) => Some(d.clone()),
        (None, None) => None,
    };
    let description = combined.filter(|_| !m.contains_key("description"));
    if description.is_none() && !doc.deprecated && doc.example.is_none() {
        return;
    }
    // Only worth restructuring once there is actually a sibling to carry.
    ref_to_allof(m);
    if let Some(c) = description {
        m.insert("description".into(), Value::String(c));
    }
    if doc.deprecated {
        m.insert("deprecated".into(), Value::Bool(true));
    }
    if let Some(ex) = &doc.example {
        m.insert("example".into(), Value::String(ex.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParsingContext;
    use std::collections::HashMap;

    fn project_from(files: &[(&str, &str)]) -> (Project, std::path::PathBuf) {
        // Unique per call so tests running in parallel never share a dir.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nestoa-schema-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let parser = ParsingContext::default();
        let mut project = Project::default();
        for (rel, body) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            project.add(parser.parse_file(&path).unwrap());
        }
        (project, dir)
    }

    /// Lower each named declaration of `main.ts`, after writing `dto.ts` next to
    /// it, and return the schemas in order.
    fn lower_all(dto: &str, main: &str, names: &[&str]) -> Vec<Value> {
        let (project, dir) = project_from(&[("dto.ts", dto), ("main.ts", main)]);
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let main_file = dir.join("main.ts");
        names
            .iter()
            .map(|name| {
                let (decl_file, sym) = project.resolve(&main_file, name).expect(name);
                ctx.symbol_to_schema(&decl_file, &sym)
            })
            .collect()
    }

    fn prop_names(schema: &Value) -> Vec<&str> {
        schema["properties"]
            .as_object()
            .map(|p| p.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    fn required_names(schema: &Value) -> Vec<&str> {
        schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    const BASE_AND_CHILD: &str = "export class Base { id: string; secret: string; }\n\
        export class Child extends Base { extra: string; note?: string; }";

    #[test]
    fn utility_types_edit_inherited_properties() {
        let [partial, required, omit, pick] = lower_all(
            BASE_AND_CHILD,
            "import { Child } from './dto';\n\
             export type P = Partial<Child>;\n\
             export type R = Required<Child>;\n\
             export type O = Omit<Child, 'secret' | 'note'>;\n\
             export type K = Pick<Child, 'id' | 'note'>;",
            &["P", "R", "O", "K"],
        )
        .try_into()
        .unwrap();
        assert_eq!(prop_names(&partial), ["id", "secret", "extra", "note"]);
        assert!(required_names(&partial).is_empty());
        assert_eq!(required_names(&required), ["id", "secret", "extra", "note"]);
        assert_eq!(prop_names(&omit), ["id", "extra"]);
        assert_eq!(required_names(&omit), ["id", "extra"]);
        assert_eq!(prop_names(&pick), ["id", "note"]);
        assert_eq!(required_names(&pick), ["id"]);
    }

    #[test]
    fn mapped_type_helpers_lower_like_their_utility_types() {
        let schemas = lower_all(
            BASE_AND_CHILD,
            "import { PartialType, OmitType, PickType, IntersectionType } from '@nestjs/swagger';\n\
             import { Base, Child } from './dto';\n\
             class Extra { tag: string; }\n\
             export class U extends PartialType(Child) {}\n\
             export class O extends OmitType(Child, ['secret'] as const) {}\n\
             export class K extends PickType(Child, ['extra']) {}\n\
             export class N extends PartialType(OmitType(Child, ['secret'])) {}\n\
             export class I extends IntersectionType(Base, Extra) {}",
            &["U", "O", "K", "N", "I"],
        );
        let parent = |s: &Value| s["allOf"][0].clone();
        let [update, omit, pick, nested, intersection] =
            <[Value; 5]>::try_from(schemas.iter().map(parent).collect::<Vec<_>>()).unwrap();
        assert_eq!(prop_names(&update), ["id", "secret", "extra", "note"]);
        assert!(required_names(&update).is_empty());
        assert_eq!(prop_names(&omit), ["id", "extra", "note"]);
        assert_eq!(required_names(&omit), ["id", "extra"]);
        assert_eq!(prop_names(&pick), ["extra"]);
        assert_eq!(prop_names(&nested), ["id", "extra", "note"]);
        assert!(required_names(&nested).is_empty());
        let parts = intersection["allOf"]
            .as_array()
            .expect("allOf of both classes");
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn flattening_keeps_every_constraint_on_a_shared_property() {
        let [partial] = lower_all(
            "export class Narrow { value: 'x'; }\nexport class Wide { value: string; extra: number; }",
            "import { PartialType, IntersectionType } from '@nestjs/swagger';\n\
             import { Narrow, Wide } from './dto';\n\
             export class P extends PartialType(IntersectionType(Narrow, Wide)) {}",
            &["P"],
        )
        .try_into()
        .unwrap();
        assert_eq!(
            partial["allOf"][0]["properties"]["value"],
            json!({ "allOf": [
                { "type": "string", "enum": ["x"] },
                { "type": "string" },
            ] })
        );
    }

    #[test]
    fn class_extends_emits_allof_with_base_ref() {
        let (project, dir) = project_from(&[
            ("base.ts", "export class Base { a?: string; }"),
            (
                "derived.ts",
                "import { Base } from './base';\nexport class Derived extends Base { b?: number; }",
            ),
        ]);
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let derived_file = dir.join("derived.ts");
        let (decl_file, sym) = project
            .resolve(&derived_file, "Derived")
            .expect("resolve Derived");
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        let all_of = schema
            .get("allOf")
            .and_then(|v| v.as_array())
            .expect("allOf");
        assert_eq!(
            all_of[0].get("$ref").and_then(|v| v.as_str()),
            Some("#/components/schemas/Base")
        );
        let own = all_of.last().unwrap();
        assert!(own.get("properties").and_then(|p| p.get("b")).is_some());
        // The base class is collected into components for the $ref to point at.
        assert!(ctx.components.schemas.contains_key("Base"));
    }

    #[test]
    fn passthrough_type_lowers_to_first_type_arg() {
        // `PopulatedDoc` is not a project symbol, so without the passthrough
        // config it would surface as `x-unresolved`. Listed in passthrough_types,
        // `PopulatedDoc<Target>` must lower exactly as a bare `Target` would.
        let (project, dir) = project_from(&[
            ("target.ts", "export interface Target { value: string; }"),
            (
                "holder.ts",
                "import { Target } from './target';\nexport interface Holder { field?: PopulatedDoc<Target>; }",
            ),
        ]);
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let passthrough = vec!["PopulatedDoc".to_string()];
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &passthrough,
        };
        let holder_file = dir.join("holder.ts");
        let (decl_file, sym) = project
            .resolve(&holder_file, "Holder")
            .expect("resolve Holder");
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        assert_eq!(
            schema
                .get("properties")
                .and_then(|p| p.get("field"))
                .and_then(|f| f.get("$ref"))
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/Target"),
            "PopulatedDoc<Target> should lower to a $ref to Target, got {schema}"
        );
        assert!(ctx.components.schemas.contains_key("Target"));
    }

    #[test]
    fn unconfigured_wrapper_stays_unresolved() {
        // Same shape, but without the passthrough config the unknown wrapper is
        // reported as `x-unresolved` rather than silently unwrapped.
        let (project, dir) = project_from(&[
            ("target.ts", "export interface Target { value: string; }"),
            (
                "holder.ts",
                "import { Target } from './target';\nexport interface Holder { field?: PopulatedDoc<Target>; }",
            ),
        ]);
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let holder_file = dir.join("holder.ts");
        let (decl_file, sym) = project
            .resolve(&holder_file, "Holder")
            .expect("resolve Holder");
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        assert_eq!(
            schema
                .get("properties")
                .and_then(|p| p.get("field"))
                .and_then(|f| f.get("x-unresolved"))
                .and_then(|v| v.as_str()),
            Some("PopulatedDoc"),
        );
    }

    fn ref_name(schema: &Value, prop: &str) -> String {
        schema
            .get("properties")
            .and_then(|p| p.get(prop))
            .and_then(|p| p.get("$ref"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("expected $ref on property {prop}"))
            .strip_prefix("#/components/schemas/")
            .unwrap()
            .to_string()
    }

    #[test]
    fn same_name_different_shape_gets_parent_prefix() {
        // Two distinct `Identity` declarations with different shapes, each
        // reached through a different parent type.
        let (project, dir) = project_from(&[
            (
                "pd_identity.ts",
                "export interface Identity { name: string; }",
            ),
            (
                "party_identity.ts",
                "export interface Identity { siren: string; }",
            ),
            (
                "personal_details.ts",
                "import { Identity } from './pd_identity';\nexport interface PersonalDetails { identity: Identity; }",
            ),
            (
                "party.ts",
                "import { Identity } from './party_identity';\nexport interface Party { identity: Identity; }",
            ),
            (
                "root.ts",
                "import { PersonalDetails } from './personal_details';\nimport { Party } from './party';\nexport interface Root { personalDetails: PersonalDetails; party: Party; }",
            ),
        ]);
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let root_file = dir.join("root.ts");
        let (decl_file, sym) = project.resolve(&root_file, "Root").expect("resolve Root");
        ctx.component_ref(&decl_file, "Root", &sym);

        // First-seen Identity keeps the bare name; the second is prefixed with
        // its owner (Party).
        let pd = &ctx.components.schemas["PersonalDetails"];
        assert_eq!(ref_name(pd, "identity"), "Identity");
        let party = &ctx.components.schemas["Party"];
        assert_eq!(ref_name(party, "identity"), "PartyIdentity");
        // The two components keep their distinct shapes.
        assert!(
            ctx.components.schemas["Identity"]["properties"]
                .get("name")
                .is_some()
        );
        assert!(
            ctx.components.schemas["PartyIdentity"]["properties"]
                .get("siren")
                .is_some()
        );
    }

    #[test]
    fn same_name_identical_shape_is_deduped() {
        // Two distinct `Identity` declarations that happen to be structurally
        // identical collapse onto a single component.
        let (project, dir) = project_from(&[
            (
                "a_identity.ts",
                "export interface Identity { name: string; }",
            ),
            (
                "b_identity.ts",
                "export interface Identity { name: string; }",
            ),
            (
                "a.ts",
                "import { Identity } from './a_identity';\nexport interface A { identity: Identity; }",
            ),
            (
                "b.ts",
                "import { Identity } from './b_identity';\nexport interface B { identity: Identity; }",
            ),
            (
                "root.ts",
                "import { A } from './a';\nimport { B } from './b';\nexport interface Root { a: A; b: B; }",
            ),
        ]);
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let root_file = dir.join("root.ts");
        let (decl_file, sym) = project.resolve(&root_file, "Root").expect("resolve Root");
        ctx.component_ref(&decl_file, "Root", &sym);

        // No prefixed duplicate was created; both parents point at one Identity.
        assert!(!ctx.components.schemas.contains_key("BIdentity"));
        assert_eq!(
            ref_name(&ctx.components.schemas["A"], "identity"),
            "Identity"
        );
        assert_eq!(
            ref_name(&ctx.components.schemas["B"], "identity"),
            "Identity"
        );
    }

    #[test]
    fn same_declaration_referenced_twice_is_one_component() {
        // The same declaration reached from two properties is emitted once and
        // never prefixed.
        let (project, dir) = project_from(&[
            ("account.ts", "export interface Account { id: string; }"),
            (
                "root.ts",
                "import { Account } from './account';\nexport interface Root { primary: Account; secondary: Account; }",
            ),
        ]);
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let root_file = dir.join("root.ts");
        let (decl_file, sym) = project.resolve(&root_file, "Root").expect("resolve Root");
        ctx.component_ref(&decl_file, "Root", &sym);

        let root = &ctx.components.schemas["Root"];
        assert_eq!(ref_name(root, "primary"), "Account");
        assert_eq!(ref_name(root, "secondary"), "Account");
        assert_eq!(
            ctx.components
                .schemas
                .keys()
                .filter(|k| k.ends_with("Account"))
                .count(),
            1
        );
    }

    /// Build a project plus the `sources`/`file_starts` maps JSDoc lookups need,
    /// mirroring the wiring in `main.rs`.
    fn project_with_sources(
        files: &[(&str, &str)],
    ) -> (
        Project,
        std::path::PathBuf,
        HashMap<std::path::PathBuf, String>,
        HashMap<std::path::PathBuf, swc_common::BytePos>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nestoa-jsdoc-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let parser = ParsingContext::default();
        let mut project = Project::default();
        let mut sources = HashMap::new();
        let mut file_starts = HashMap::new();
        for (rel, body) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            project.add(parser.parse_file(&path).unwrap());
            sources.insert(path.clone(), body.to_string());
            if let Some(fm) = parser
                .source_map
                .get_source_file(&swc_common::FileName::Real(path.clone()))
            {
                file_starts.insert(path.clone(), fm.start_pos);
            }
        }
        (project, dir, sources, file_starts)
    }

    /// `static` members live on the constructor and are never part of an
    /// instance's JSON, so they do not belong in a body schema — they are also
    /// where internals like a logger live, whose types would otherwise be
    /// pulled into `components.schemas`. A `private` *instance* field is a
    /// different matter: TypeScript erases it, so it is still serialized and is
    /// kept.
    #[test]
    fn static_class_props_are_omitted_but_private_instance_ones_are_kept() {
        let (project, dir) = project_from(&[(
            "dto.ts",
            r#"
            import { Logger } from '@nestjs/common';
            export class Account {
                private static readonly logger: Logger = new Logger('Account');
                static VERSION: number;
                private secret: string;
                public id: string;
                label?: string;
            }
            "#,
        )]);
        let mut components = ComponentSchemas::default();
        let sources = HashMap::new();
        let file_starts = HashMap::new();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let dto_file = dir.join("dto.ts");
        let (decl_file, sym) = project.resolve(&dto_file, "Account").unwrap();
        let schema = ctx.symbol_to_schema(&decl_file, &sym);

        let props = schema.get("properties").unwrap();
        assert!(props.get("id").is_some(), "public instance prop kept");
        assert!(props.get("label").is_some(), "optional public prop kept");
        assert!(
            props.get("secret").is_some(),
            "a private instance field is erased at runtime and still serialized",
        );
        assert!(props.get("logger").is_none(), "private static prop dropped");
        assert!(props.get("VERSION").is_none(), "static prop dropped");
        // The dropped members must not drag their types into the components.
        assert!(!components.schemas.contains_key("Logger"));
    }

    #[test]
    fn internal_jsdoc_omits_property_from_schema() {
        // `@internal` and `@private` mark implementation-only members that are
        // stripped at runtime (toJSON / formatLean) and must not appear on the wire.
        let (project, dir, sources, file_starts) = project_with_sources(&[(
            "dto.ts",
            "export class Dto {\n  /** Public field */\n  name?: string;\n  /**\n   * Raw data from the aggregator\n   * @internal\n   */\n  rawData?: string;\n  /** @private */\n  secret?: string;\n}",
        )]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let dto_file = dir.join("dto.ts");
        let (decl_file, sym) = project.resolve(&dto_file, "Dto").expect("resolve Dto");
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("properties");
        assert!(props.contains_key("name"), "public field kept");
        assert!(!props.contains_key("rawData"), "@internal field dropped");
        assert!(!props.contains_key("secret"), "@private field dropped");
    }

    #[test]
    fn internal_property_is_not_marked_required() {
        // A required-by-syntax member that is `@internal` should neither appear
        // in `properties` nor leak into `required`.
        let (project, dir, sources, file_starts) = project_with_sources(&[(
            "iface.ts",
            "export interface Iface {\n  keep: string;\n  /** @internal */\n  drop: string;\n}",
        )]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let iface_file = dir.join("iface.ts");
        let (decl_file, sym) = project
            .resolve(&iface_file, "Iface")
            .expect("resolve Iface");
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("keep"));
        assert!(!props.contains_key("drop"));
        let required: Vec<&str> = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(required, vec!["keep"]);
    }

    #[test]
    fn documented_ref_property_hoists_ref_into_allof() {
        // A `$ref` with a `description` sibling: 3.0 tooling drops the sibling
        // (Swagger Editor's "All other properties in a $ref object are ignored"),
        // so the reference has to move into an `allOf` branch.
        let (project, dir, sources, file_starts) = project_with_sources(&[
            ("kind.ts", "export interface Kind { label: string; }"),
            (
                "dto.ts",
                "import { Kind } from './kind';\nexport class Dto {\n  /** Account's type */\n  kind?: Kind;\n  /** Plain field */\n  name?: string;\n}",
            ),
        ]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let dto_file = dir.join("dto.ts");
        let (decl_file, sym) = project.resolve(&dto_file, "Dto").expect("resolve Dto");
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        let kind = schema
            .get("properties")
            .and_then(|p| p.get("kind"))
            .unwrap();
        assert!(
            kind.get("$ref").is_none(),
            "no bare $ref beside siblings, got {kind}"
        );
        assert_eq!(
            kind.pointer("/allOf/0/$ref").and_then(|v| v.as_str()),
            Some("#/components/schemas/Kind"),
            "reference kept as an allOf branch, got {kind}"
        );
        assert_eq!(
            kind.get("description").and_then(|v| v.as_str()),
            Some("Account's type")
        );
        // An undocumented reference stays a bare $ref — nothing to carry.
        let (project2, dir2, sources2, file_starts2) = project_with_sources(&[
            ("kind.ts", "export interface Kind { label: string; }"),
            (
                "dto.ts",
                "import { Kind } from './kind';\nexport class Dto {\n  kind?: Kind;\n}",
            ),
        ]);
        let mut components2 = ComponentSchemas::default();
        let mut ctx2 = SchemaCtx {
            project: &project2,
            sources: &sources2,
            file_starts: &file_starts2,
            components: &mut components2,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let dto_file2 = dir2.join("dto.ts");
        let (decl_file2, sym2) = project2.resolve(&dto_file2, "Dto").expect("resolve Dto");
        let bare = ctx2.symbol_to_schema(&decl_file2, &sym2);
        assert_eq!(
            bare.pointer("/properties/kind/$ref")
                .and_then(|v| v.as_str()),
            Some("#/components/schemas/Kind"),
            "undocumented reference stays bare, got {bare}"
        );
    }

    #[test]
    fn nullable_unions_use_json_schema_null_not_nullable_keyword() {
        // `nullable: true` is a 3.0 keyword with no meaning in the 3.1 document
        // nestoa emits, and beside a `$ref`/`oneOf` it is a no-op even in 3.0.
        let (project, dir, sources, file_starts) = project_with_sources(&[
            ("kind.ts", "export interface Kind { label: string; }"),
            (
                "dto.ts",
                "import { Kind } from './kind';\nexport interface Dto {\n  name: string | null;\n  kind: Kind | null;\n  state: 'a' | 'b' | null;\n  mixed: string | number | null;\n  maybe?: Kind | undefined;\n}",
            ),
        ]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let dto_file = dir.join("dto.ts");
        let (decl_file, sym) = project.resolve(&dto_file, "Dto").expect("resolve Dto");
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        let props = schema.get("properties").unwrap();
        assert!(
            !schema.to_string().contains("nullable"),
            "no 3.0 nullable keyword anywhere, got {schema}"
        );

        // A primitive widens its `type` keyword.
        assert_eq!(
            props.pointer("/name/type").unwrap(),
            &json!(["string", "null"])
        );

        // A reference has no type keyword of its own: add a null branch.
        let kind = props.get("kind").unwrap();
        assert_eq!(
            kind.pointer("/oneOf/0/$ref").and_then(|v| v.as_str()),
            Some("#/components/schemas/Kind"),
            "got {kind}"
        );
        assert_eq!(kind.pointer("/oneOf/1/type").unwrap(), &json!("null"));

        // An enum constrains values independently of `type`, so `null` joins both.
        let state = props.get("state").unwrap();
        assert_eq!(state.get("type").unwrap(), &json!(["string", "null"]));
        assert_eq!(state.get("enum").unwrap(), &json!(["a", "b", null]));

        // A oneOf gains an explicit null member rather than a sibling keyword.
        let mixed = props.get("mixed").unwrap();
        let branches = mixed.get("oneOf").and_then(|v| v.as_array()).unwrap();
        assert_eq!(branches.len(), 3, "got {mixed}");
        assert_eq!(branches[2].get("type").unwrap(), &json!("null"));

        // `| undefined` has no wire representation: it must not leak a JSON null
        // into a `oneOf`, and optionality is already carried by `required`.
        assert_eq!(
            props.pointer("/maybe/$ref").and_then(|v| v.as_str()),
            Some("#/components/schemas/Kind"),
            "got {}",
            props.get("maybe").unwrap()
        );
    }
}
