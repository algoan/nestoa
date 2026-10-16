use std::collections::{HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use swc_common::{Span, Spanned};
use swc_ecma_ast::{
    Expr, Lit, TsArrayType, TsEntityName, TsKeywordTypeKind, TsLit, TsType, TsTypeElement,
    TsTypeLit, TsTypeOperatorOp, TsTypeParamInstantiation, TsTypeQueryExpr, TsTypeRef,
    TsUnionOrIntersectionType,
};

use crate::jsdoc::JsDoc;
use crate::symbols::{EnumValue, Project, Symbol, enum_member_values};

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
    /// Property-set edits whose operand was still being lowered: the
    /// component name reserved for the result, the operand, and the edit.
    deferred: Vec<(String, Value, ObjectEdit)>,
}

impl ComponentSchemas {
    /// Apply the property-set edits deferred while their operand was being
    /// lowered, now that every component is complete. One deferred edit may
    /// operate on another's result, so this repeats until nothing changes;
    /// an operand that still cannot be flattened is emitted unchanged.
    pub fn resolve_deferred(&mut self) {
        let mut pending = std::mem::take(&mut self.deferred);
        loop {
            let before = pending.len();
            pending.retain(
                |(name, base, edit)| match reshape(&self.schemas, base, edit) {
                    Some(edited) => {
                        self.schemas.insert(name.clone(), edited);
                        false
                    }
                    None => true,
                },
            );
            if pending.len() == before {
                break;
            }
        }
        for (name, base, _) in pending {
            self.schemas.insert(name, base);
        }
    }

    /// The name of a component `schema` references, directly or through an
    /// `allOf` branch, that is reserved but not yet lowered.
    fn pending_ref(&self, schema: &Value) -> Option<String> {
        if let Some(name) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/components/schemas/"))
        {
            let pending = self.allocated.contains(name) && !self.schemas.contains_key(name);
            return pending.then(|| name.to_string());
        }
        schema
            .get("allOf")?
            .as_array()?
            .iter()
            .find_map(|branch| self.pending_ref(branch))
    }

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
    /// Whether the bound type admits `undefined`, decided where the argument
    /// was written. Set alongside `resolved`, whose schema drops `undefined`.
    pub admits_undefined: Option<bool>,
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
    let sig: Vec<(&Value, Option<bool>)> = params
        .iter()
        .filter_map(|p| {
            let binding = env.get(p)?;
            Some((binding.resolved.as_ref()?, binding.admits_undefined))
        })
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

/// What `SchemaCtx::class_member` reads off a class field or a constructor
/// parameter property.
struct ClassMember<'a> {
    span: Span,
    decorators: &'a [swc_ecma_ast::Decorator],
    type_ann: Option<&'a swc_ecma_ast::TsTypeAnn>,
    init: Option<&'a Expr>,
    /// Declared with `?`.
    optional: bool,
}

pub struct SchemaCtx<'a> {
    pub project: &'a Project,
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
            TsType::TsKeywordType(k) if k.kind == TsKeywordTypeKind::TsSymbolKeyword => {
                self.unsupported(file, ty, "symbol")
            }
            TsType::TsKeywordType(k) => keyword_schema(k.kind),
            TsType::TsLitType(l) => match &l.lit {
                TsLit::Str(s) => {
                    json!({ "type": "string", "enum": [s.value.to_string_lossy().into_owned()] })
                }
                TsLit::Number(n) => json!({ "type": "number", "enum": [number_json(n.value)] }),
                TsLit::Bool(b) => json!({ "type": "boolean", "enum": [b.value] }),
                // A template literal type is a string whose pattern is not kept.
                TsLit::Tpl(_) => {
                    let mut schema = self.unsupported(file, ty, "template literal");
                    schema["type"] = json!("string");
                    schema
                }
                TsLit::BigInt(_) => self.unsupported(file, ty, "bigint literal"),
            },
            TsType::TsArrayType(TsArrayType { elem_type, .. }) => json!({
                "type": "array",
                "items": self.ts_type_to_schema(file, elem_type),
            }),
            TsType::TsTupleType(t) => {
                let mut prefix = Vec::new();
                let mut tail = Value::Bool(false);
                for (i, e) in t.elem_types.iter().enumerate() {
                    match &*e.ty {
                        TsType::TsRestType(r) if i + 1 == t.elem_types.len() => {
                            let rest = self.ts_type_to_schema(file, &r.type_ann);
                            let (more, rest_tail) = spread_elements(&self.components.schemas, rest)
                                .unwrap_or_else(|| {
                                    (
                                        Vec::new(),
                                        self.unsupported(file, &r.type_ann, "rest element"),
                                    )
                                });
                            prefix.extend(more);
                            tail = rest_tail;
                        }
                        TsType::TsRestType(_) => {
                            let mut schema = self.unsupported(file, ty, "tuple");
                            schema["type"] = json!("array");
                            schema["items"] = json!({});
                            return schema;
                        }
                        elem => prefix.push(self.ts_type_to_schema(file, elem)),
                    }
                }
                tuple_schema(prefix, tail)
            }
            // `[a, b?]`: positions are never required, so `b?` is just `b`.
            TsType::TsOptionalType(o) => self.ts_type_to_schema(file, &o.type_ann),
            TsType::TsUnionOrIntersectionType(uoi) => self.uoi_schema(file, uoi),
            TsType::TsTypeRef(r) => self.type_ref_schema(file, r),
            TsType::TsTypeLit(lit) => self.type_lit_schema(file, lit),
            TsType::TsParenthesizedType(p) => self.ts_type_to_schema(file, &p.type_ann),
            TsType::TsTypeOperator(op) if op.op == TsTypeOperatorOp::ReadOnly => {
                self.ts_type_to_schema(file, &op.type_ann)
            }
            TsType::TsTypeOperator(op) if op.op == TsTypeOperatorOp::KeyOf => self
                .keyof_schema(file, &op.type_ann)
                .unwrap_or_else(|| self.unsupported(file, ty, "keyof")),
            TsType::TsTypeOperator(_) => self.unsupported(file, ty, "unique"),
            TsType::TsIndexedAccessType(ia) => self
                .indexed_access_schema(file, &ia.obj_type, &ia.index_type)
                .unwrap_or_else(|| self.unsupported(file, ty, "indexed access")),
            TsType::TsMappedType(_) => self.unsupported(file, ty, "mapped type"),
            TsType::TsConditionalType(_) => self.unsupported(file, ty, "conditional type"),
            TsType::TsInferType(_) => self.unsupported(file, ty, "infer"),
            TsType::TsTypeQuery(_) => self.unsupported(file, ty, "typeof"),
            TsType::TsImportType(_) => self.unsupported(file, ty, "import type"),
            TsType::TsRestType(_) => self.unsupported(file, ty, "rest element"),
            TsType::TsThisType(_) => self.unsupported(file, ty, "this"),
            TsType::TsFnOrConstructorType(_) => self.unsupported(file, ty, "function"),
            TsType::TsTypePredicate(_) => self.unsupported(file, ty, "type predicate"),
        }
    }

    /// `keyof typeof SomeEnum`: the enum's member names. Any other `keyof` is
    /// `None`: an object's `properties` leave out its index signatures, so its
    /// key set cannot be read off its schema.
    fn keyof_schema(&self, file: &Path, operand: &TsType) -> Option<Value> {
        let TsType::TsTypeQuery(q) = operand else {
            return None;
        };
        let TsTypeQueryExpr::TsEntityName(TsEntityName::Ident(id)) = &q.expr_name else {
            return None;
        };
        let (_, Symbol::Enum(e)) = self.project.resolve(file, id.sym.as_ref())? else {
            return None;
        };
        let names: Vec<String> = enum_member_values(&e).into_iter().map(|(n, _)| n).collect();
        Some(if names.is_empty() {
            json!({ "not": {} })
        } else {
            json!({ "type": "string", "enum": names })
        })
    }

    /// `X['k']`: the schema of property `k` of an object-shaped `X`.
    fn indexed_access_schema(
        &mut self,
        file: &Path,
        obj: &TsType,
        index: &TsType,
    ) -> Option<Value> {
        let [key] = self.string_union_keys(file, index, 0)?.try_into().ok()?;
        let schema = self.ts_type_to_schema(file, obj);
        let mut obj = flatten_object(&self.components.schemas, &schema, 0)?;
        obj.get_mut("properties")?.as_object_mut()?.remove(&key)
    }

    fn jsdoc(&self, file: &Path, span: Span) -> Option<JsDoc> {
        self.project.files.get(file)?.docs.jsdoc(span.lo)
    }

    /// A type form nestoa does not lower, marked with its source text (or
    /// `kind` when the source is not at hand) so the gap stays visible.
    fn unsupported(&self, file: &Path, ty: &TsType, kind: &str) -> Value {
        let span = ty.span();
        let text = self.project.files.get(file).and_then(|f| f.text(span));
        json!({ "x-unresolved": text.unwrap_or_else(|| kind.to_string()) })
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
                    return if nullable {
                        json!({ "type": "null" })
                    } else {
                        json!({})
                    };
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
                    json!({ "anyOf": non_null })
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
            if let Some((_, Symbol::Enum(e))) = self.project.resolve(file, &parent)
                && let Some((_, value)) = enum_member_values(&e)
                    .into_iter()
                    .find(|(name, _)| *name == member)
            {
                return match value {
                    Some(v) => {
                        json!({ "type": enum_value_type(&v), "enum": [enum_value_json(&v)] })
                    }
                    None => json!({ "x-unresolved": format!("{parent}.{member}") }),
                };
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
            "Promise" | "Observable" | "Awaited" => {
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
            "Readonly" => {
                if let Some(args) = &r.type_params
                    && let Some(first) = args.params.first()
                {
                    return self.ts_type_to_schema(file, first);
                }
            }
            "NonNullable" => {
                if let Some(args) = &r.type_params
                    && let Some(first) = args.params.first()
                {
                    let schema = self.ts_type_to_schema(file, first);
                    return strip_null(&self.components.schemas, schema);
                }
            }
            "Set" | "ReadonlySet" => {
                if let Some(args) = &r.type_params
                    && let Some(first) = args.params.first()
                {
                    return json!({
                        "type": "array",
                        "items": self.ts_type_to_schema(file, first),
                        "uniqueItems": true,
                    });
                }
            }
            "Map" | "ReadonlyMap" => {
                if let Some(args) = &r.type_params
                    && let Some(value_t) = args.params.get(1)
                {
                    return json!({
                        "type": "object",
                        "additionalProperties": self.ts_type_to_schema(file, value_t),
                    });
                }
            }
            "Omit" | "Pick" => {
                if let Some(args) = &r.type_params
                    && let Some(first) = args.params.first()
                {
                    let base = self.ts_type_to_schema(file, first);
                    let Some(keys) = args
                        .params
                        .get(1)
                        .and_then(|k| self.string_union_keys(file, k, 0))
                    else {
                        return json!({ "x-unresolved": format!("{name}<…>") });
                    };
                    let edit = if name == "Omit" {
                        ObjectEdit::Omit(keys)
                    } else {
                        ObjectEdit::Pick(keys)
                    };
                    return self.reshape_object(base, &edit);
                }
            }
            "Exclude" | "Extract" => {
                if let Some(args) = &r.type_params
                    && let [from, filter] = args.params.as_slice()
                {
                    let from = self.ts_type_to_schema(file, from);
                    let filter = self.ts_type_to_schema(file, filter);
                    return filter_union(
                        &self.components.schemas,
                        from,
                        &filter,
                        name == "Extract",
                    )
                    .unwrap_or_else(|| json!({ "x-unresolved": format!("{name}<…>") }));
                }
            }
            "Record" => {
                if let Some(args) = &r.type_params
                    && let [key_t, value_t] = args.params.as_slice()
                {
                    let value = self.ts_type_to_schema(file, value_t);
                    if let Some(keys) = self.string_union_keys(file, key_t, 0) {
                        let props: Map<String, Value> =
                            keys.iter().map(|k| (k.clone(), value.clone())).collect();
                        let mut record = json!({ "type": "object", "properties": props });
                        if !self.admits_undefined(file, value_t, 0) {
                            record["required"] = json!(keys);
                        }
                        return record;
                    }
                    return json!({
                        "type": "object",
                        "additionalProperties": value,
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
    ///
    /// A declaration is identified by its own name, so references through
    /// different import aliases share one component; it is *named* after the
    /// first reference, keeping an alias chosen to tell same-named declarations
    /// apart (`Account as MastercardAccount`).
    fn component_ref(&mut self, decl_file: &Path, type_name: &str, sym: &Symbol) -> Value {
        let key = (decl_file.to_path_buf(), sym.name());
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
            let admits_undefined = self.admits_undefined(file, arg, 0);
            new_env.insert(
                param.clone(),
                TypeBinding {
                    origin_file: file.to_path_buf(),
                    ty: (**arg).clone(),
                    resolved: Some(resolved),
                    admits_undefined: Some(admits_undefined),
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
                let Some(name) = property_key(&p.key) else {
                    continue;
                };
                // `@internal`/`@private` members never reach the wire.
                if self.jsdoc(file, p.span).is_some_and(|d| d.internal) {
                    continue;
                }
                let schema = p
                    .type_ann
                    .as_ref()
                    .map(|t| self.ts_type_to_schema(file, &t.type_ann))
                    .unwrap_or_else(|| json!({}));
                props.insert(name.clone(), schema);
                if !p.optional
                    && !p
                        .type_ann
                        .as_ref()
                        .is_some_and(|t| self.admits_undefined(file, &t.type_ann, 0))
                {
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

    /// The schema a class inherits from its `extends` clause: a plain or
    /// namespace-qualified class, or a `@nestjs/mapped-types` /
    /// `@nestjs/swagger` helper call, which may nest
    /// (`PartialType(OmitType(CreateDto, ['id']))`). `None` when any part of it
    /// cannot be lowered, rather than a partial result.
    fn heritage_schema(
        &mut self,
        file: &Path,
        expr: &Expr,
        type_args: Option<&TsTypeParamInstantiation>,
    ) -> Option<Value> {
        match expr {
            Expr::Ident(_) | Expr::Member(_) => {
                let reference = TsTypeRef {
                    span: expr.span(),
                    type_name: entity_name_of(expr)?,
                    type_params: type_args.map(|a| Box::new(a.clone())),
                };
                Some(self.type_ref_schema(file, &reference))
            }
            Expr::Paren(p) => self.heritage_schema(file, &p.expr, type_args),
            Expr::Call(call) => {
                let swc_ecma_ast::Callee::Expr(callee) = &call.callee else {
                    return None;
                };
                let helper = self.mapped_type_helper(file, callee)?;
                let mut args = call.args.iter().map(|a| a.expr.as_ref());
                if helper == "IntersectionType" {
                    let parts = args
                        .map(|a| self.heritage_schema(file, a, None))
                        .collect::<Option<Vec<Value>>>()?;
                    return Some(json!({ "allOf": parts }));
                }
                let base = self.heritage_schema(file, args.next()?, None)?;
                let edit = match helper.as_str() {
                    "PartialType" => ObjectEdit::AllOptional,
                    "OmitType" => ObjectEdit::Omit(string_array_keys(args.next()?)?),
                    _ => ObjectEdit::Pick(string_array_keys(args.next()?)?),
                };
                Some(self.reshape_object(base, &edit))
            }
            _ => None,
        }
    }

    /// The mapped-type helper a call's callee names, when it is imported from
    /// a NestJS package — directly, under an alias (`PartialType as
    /// MakePartial`), through a namespace (`swagger.PartialType`) or an in-repo
    /// barrel. A same-named helper of the project's own is not one.
    fn mapped_type_helper(&self, file: &Path, callee: &Expr) -> Option<String> {
        let (package, name) = match callee {
            Expr::Ident(id) => self.project.import_origin(file, id.sym.as_ref())?,
            Expr::Member(m) => {
                let (Expr::Ident(ns), swc_ecma_ast::MemberProp::Ident(prop)) =
                    (m.obj.as_ref(), &m.prop)
                else {
                    return None;
                };
                self.project
                    .namespace_member_origin(file, ns.sym.as_ref(), prop.sym.as_ref())?
            }
            _ => return None,
        };
        let from_nest = matches!(
            package.as_str(),
            "@nestjs/mapped-types" | "@nestjs/swagger" | "@nestjs/graphql"
        );
        let helper = matches!(
            name.as_str(),
            "PartialType" | "OmitType" | "PickType" | "IntersectionType"
        );
        (from_nest && helper).then_some(name)
    }

    /// The keys a `Pick`/`Omit` key type names: string literals, unions of
    /// them, and type aliases of either. `None` for anything else (`keyof`,
    /// a template literal), so the key set is never partial.
    fn string_union_keys(&self, file: &Path, ty: &TsType, depth: usize) -> Option<Vec<String>> {
        const MAX_ALIAS_DEPTH: usize = 8;
        match ty {
            TsType::TsLitType(l) => match &l.lit {
                TsLit::Str(s) => Some(vec![s.value.to_string_lossy().into_owned()]),
                _ => None,
            },
            TsType::TsUnionOrIntersectionType(TsUnionOrIntersectionType::TsUnionType(u)) => {
                let mut keys: Vec<String> = Vec::new();
                for t in &u.types {
                    for key in self.string_union_keys(file, t, depth)? {
                        if !keys.contains(&key) {
                            keys.push(key);
                        }
                    }
                }
                Some(keys)
            }
            TsType::TsParenthesizedType(p) => self.string_union_keys(file, &p.type_ann, depth),
            TsType::TsTypeRef(r) if r.type_params.is_none() && depth < MAX_ALIAS_DEPTH => {
                let TsEntityName::Ident(id) = &r.type_name else {
                    return None;
                };
                let (decl_file, Symbol::TypeAlias(alias)) =
                    self.project.resolve(file, id.sym.as_ref())?
                else {
                    return None;
                };
                self.string_union_keys(&decl_file, &alias.type_ann, depth + 1)
            }
            _ => None,
        }
    }

    /// Apply a property-set edit (`Partial`, `Omit`, …) to an object schema
    /// (see `reshape`). An operand that is a component still being lowered (a
    /// recursive `Partial<Node>` inside `Node`) cannot be inlined yet, so the
    /// edit is registered as its own component and applied by
    /// `ComponentSchemas::resolve_deferred`. Any other schema that cannot be
    /// flattened, such as a union, is returned unchanged.
    fn reshape_object(&mut self, schema: Value, edit: &ObjectEdit) -> Value {
        if let Some(edited) = reshape(&self.components.schemas, &schema, edit) {
            return edited;
        }
        let Some(base) = self.components.pending_ref(&schema) else {
            return schema;
        };
        let name = self
            .components
            .fresh_name("", &format!("{}{base}", edit.prefix()));
        self.components.allocated.insert(name.clone());
        self.components
            .deferred
            .push((name.clone(), schema, edit.clone()));
        ref_value(&name)
    }

    /// A class field as `(name, schema, optional)`, or nothing when it never
    /// reaches the wire. A `static` member lives on the constructor, never on
    /// an instance, and routinely holds internals (a logger, a cache).
    /// Accessibility is deliberately *not* considered: TypeScript erases
    /// `private`, so such a field is still serialized.
    fn class_prop(
        &mut self,
        file: &Path,
        p: &swc_ecma_ast::ClassProp,
        hidden: &mut Vec<String>,
    ) -> Vec<(String, Value, bool)> {
        if p.is_static {
            return Vec::new();
        }
        let Some(name) = prop_name_key(&p.key) else {
            return Vec::new();
        };
        let member = ClassMember {
            span: p.span,
            decorators: &p.decorators,
            type_ann: p.type_ann.as_deref(),
            init: p.value.as_deref(),
            optional: p.is_optional,
        };
        self.class_member(file, name, member, hidden)
            .into_iter()
            .collect()
    }

    /// A constructor parameter property (`constructor(public name: string)`),
    /// which declares a field like a class property does.
    fn param_prop(
        &mut self,
        file: &Path,
        pp: &swc_ecma_ast::TsParamProp,
        hidden: &mut Vec<String>,
    ) -> Option<(String, Value, bool)> {
        let (binding, init) = match &pp.param {
            swc_ecma_ast::TsParamPropParam::Ident(b) => (b, None),
            swc_ecma_ast::TsParamPropParam::Assign(a) => match &*a.left {
                swc_ecma_ast::Pat::Ident(b) => (b, Some(&*a.right)),
                _ => return None,
            },
        };
        let member = ClassMember {
            span: pp.span,
            decorators: &pp.decorators,
            type_ann: binding.type_ann.as_deref(),
            init,
            optional: binding.id.optional,
        };
        self.class_member(file, binding.id.sym.to_string(), member, hidden)
    }

    /// Lower a class member as `(name, schema, optional)`, or nothing when
    /// its JSDoc or decorators keep it off the wire. That is decided before
    /// its type is lowered, so a hidden member's types never become components.
    /// A hidden member's name is pushed to `hidden`.
    fn class_member(
        &mut self,
        file: &Path,
        name: String,
        member: ClassMember,
        hidden: &mut Vec<String>,
    ) -> Option<(String, Value, bool)> {
        let doc = self.jsdoc(file, member.span);
        // `@internal`/`@private` members never reach the wire.
        if doc.as_ref().is_some_and(|d| d.internal) || member.decorators.iter().any(hides_property)
        {
            hidden.push(name);
            return None;
        }
        let mut schema = match (member.type_ann, member.init) {
            (Some(t), _) => self.ts_type_to_schema(file, &t.type_ann),
            (None, Some(init)) => self.initializer_schema(file, init),
            (None, None) => json!({}),
        };
        if let Some(doc) = &doc {
            attach_doc(&mut schema, doc);
        }
        let optional = member.optional
            || member
                .type_ann
                .is_some_and(|t| self.admits_undefined(file, &t.type_ann, 0))
            || member.decorators.iter().any(marks_optional);
        Some((name, schema, optional))
    }

    /// Whether a property's declared type admits `undefined`, directly or
    /// through type aliases, which makes it optional on the wire like a `?`.
    fn admits_undefined(&self, file: &Path, ty: &TsType, depth: usize) -> bool {
        const MAX_ALIAS_DEPTH: usize = 8;
        match ty {
            TsType::TsKeywordType(k) => matches!(
                k.kind,
                TsKeywordTypeKind::TsUndefinedKeyword | TsKeywordTypeKind::TsVoidKeyword
            ),
            TsType::TsParenthesizedType(p) => self.admits_undefined(file, &p.type_ann, depth),
            TsType::TsUnionOrIntersectionType(TsUnionOrIntersectionType::TsUnionType(u)) => u
                .types
                .iter()
                .any(|t| self.admits_undefined(file, t, depth)),
            TsType::TsTypeRef(r) if depth < MAX_ALIAS_DEPTH => {
                // A bound type parameter, checked where its type was written
                // (see `type_ref_schema`).
                if let TsEntityName::Ident(id) = &r.type_name
                    && r.type_params.is_none()
                    && let Some(binding) = self
                        .type_env
                        .as_ref()
                        .and_then(|env| env.get(id.sym.as_ref()))
                {
                    return binding.admits_undefined.unwrap_or_else(|| {
                        self.admits_undefined(&binding.origin_file, &binding.ty, depth + 1)
                    });
                }
                if let TsEntityName::Ident(id) = &r.type_name
                    && id.sym == "Awaited"
                    && let Some(first) = r.type_params.as_ref().and_then(|a| a.params.first())
                {
                    return self.admits_undefined(file, first, depth + 1);
                }
                let resolved = match &r.type_name {
                    TsEntityName::Ident(id) => self.project.resolve(file, id.sym.as_ref()),
                    TsEntityName::TsQualifiedName(q) => self.project.resolve_namespace_member(
                        file,
                        &entity_name(&q.left),
                        q.right.sym.as_ref(),
                    ),
                };
                match resolved {
                    Some((decl_file, Symbol::TypeAlias(alias))) => {
                        self.admits_undefined(&decl_file, &alias.type_ann, depth + 1)
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// The type TypeScript infers for an unannotated field from its
    /// initialiser, widened as for a mutable field (`a = 1` is a `number`).
    fn initializer_schema(&mut self, file: &Path, init: &Expr) -> Value {
        match init {
            Expr::Lit(Lit::Str(_)) | Expr::Tpl(_) => json!({ "type": "string" }),
            Expr::Lit(Lit::Num(_)) => json!({ "type": "number" }),
            Expr::Lit(Lit::Bool(_)) => json!({ "type": "boolean" }),
            Expr::Unary(u)
                if matches!(
                    u.op,
                    swc_ecma_ast::UnaryOp::Minus | swc_ecma_ast::UnaryOp::Plus
                ) && matches!(*u.arg, Expr::Lit(Lit::Num(_))) =>
            {
                json!({ "type": "number" })
            }
            Expr::New(n) if matches!(&*n.callee, Expr::Ident(id) if id.sym == "Date") => {
                json!({ "type": "string", "format": "date-time" })
            }
            Expr::TsAs(a) => self.ts_type_to_schema(file, &a.type_ann),
            Expr::Paren(p) => self.initializer_schema(file, &p.expr),
            _ => {
                let text = self
                    .project
                    .files
                    .get(file)
                    .and_then(|f| f.text(init.span()));
                json!({ "x-unresolved": text.unwrap_or_else(|| "initializer".to_string()) })
            }
        }
    }

    pub fn symbol_to_schema(&mut self, file: &Path, sym: &Symbol) -> Value {
        match sym {
            Symbol::Interface(i) => {
                let mut props: IndexMap<String, Value> = IndexMap::new();
                let mut required: Vec<String> = Vec::new();
                let mut all_of: Vec<Value> = Vec::new();
                for parent in i.decls().flat_map(|d| &d.extends) {
                    all_of.push(
                        self.heritage_schema(file, &parent.expr, parent.type_args.as_deref())
                            .unwrap_or_else(
                                || json!({ "x-unresolved": heritage_label(&parent.expr) }),
                            ),
                    );
                }
                for el in i.decls().flat_map(|d| &d.body.body) {
                    if let TsTypeElement::TsPropertySignature(p) = el {
                        let Some(name) = property_key(&p.key) else {
                            continue;
                        };
                        let doc = self.jsdoc(file, p.span);
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
                        let optional = p.optional
                            || p.type_ann
                                .as_ref()
                                .is_some_and(|t| self.admits_undefined(file, &t.type_ann, 0));
                        props.insert(name.clone(), schema);
                        if !optional && !required.contains(&name) {
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
                let object_schema = Value::Object(object);
                let mut schema = if all_of.is_empty() {
                    object_schema
                } else {
                    all_of.push(object_schema);
                    json!({ "allOf": all_of })
                };
                if let Some(doc) = self.jsdoc(file, i.span) {
                    attach_doc(&mut schema, &doc);
                }
                schema
            }
            Symbol::TypeAlias(t) => self.ts_type_to_schema(file, &t.type_ann),
            Symbol::Enum(e) => {
                let values: Option<Vec<EnumValue>> =
                    enum_member_values(e).into_iter().map(|(_, v)| v).collect();
                let mut obj = Map::new();
                match values {
                    Some(values) => {
                        let mut types: Vec<&str> = Vec::new();
                        for v in &values {
                            let t = enum_value_type(v);
                            if !types.contains(&t) {
                                types.push(t);
                            }
                        }
                        match types.as_slice() {
                            // `enum Empty {}` admits no value; `type`/`enum`
                            // arrays must be non-empty, so say it with `not`.
                            [] => {
                                obj.insert("not".into(), json!({}));
                            }
                            [one] => {
                                obj.insert("type".into(), json!(one));
                            }
                            _ => {
                                obj.insert("type".into(), json!(types));
                            }
                        }
                        if !values.is_empty() {
                            obj.insert(
                                "enum".into(),
                                Value::Array(values.iter().map(enum_value_json).collect()),
                            );
                        }
                    }
                    // A member whose value nestoa cannot compute: listing the
                    // others would reject it, so the constraint is left out.
                    None => {
                        obj.insert("x-unresolved".into(), json!(e.id.sym.to_string()));
                    }
                }
                if let Some(doc) = self.jsdoc(file, e.span) {
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
                if let Some(super_expr) = &c.super_class {
                    let parent = self
                        .heritage_schema(file, super_expr, c.super_type_params.as_deref())
                        .unwrap_or_else(|| json!({ "x-unresolved": heritage_label(super_expr) }));
                    all_of.push(parent);
                }
                let mut hidden: Vec<String> = Vec::new();
                let mut optional_names: Vec<String> = Vec::new();
                for member in &c.body {
                    let lowered = match member {
                        swc_ecma_ast::ClassMember::ClassProp(p) => {
                            self.class_prop(file, p, &mut hidden)
                        }
                        swc_ecma_ast::ClassMember::Constructor(ctor) => ctor
                            .params
                            .iter()
                            .filter_map(|param| match param {
                                swc_ecma_ast::ParamOrTsParamProp::TsParamProp(pp) => {
                                    self.param_prop(file, pp, &mut hidden)
                                }
                                swc_ecma_ast::ParamOrTsParamProp::Param(_) => None,
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    for (name, schema, optional) in lowered {
                        props.insert(name.clone(), schema);
                        if optional {
                            optional_names.push(name);
                        } else {
                            required.push(name);
                        }
                    }
                }
                // A hidden or optional override hides or relaxes the inherited
                // property too. The parent is inlined only when it declares such
                // a property, so the shared component stays untouched and other
                // subclasses keep their `$ref`.
                if let Some(parent) = all_of.first_mut()
                    && let Some(inherited) = inherited_object(&self.components.schemas, parent)
                {
                    let declares = |k: &String| {
                        inherited
                            .get("properties")
                            .and_then(Value::as_object)
                            .is_some_and(|p| p.contains_key(k))
                    };
                    let requires = |k: &String| {
                        inherited
                            .get("required")
                            .and_then(Value::as_array)
                            .is_some_and(|r| r.iter().any(|v| v.as_str() == Some(k)))
                    };
                    let hide: Vec<String> = hidden.into_iter().filter(declares).collect();
                    let relax: Vec<String> = optional_names.into_iter().filter(requires).collect();
                    if !hide.is_empty() {
                        *parent = self.reshape_object(parent.take(), &ObjectEdit::Omit(hide));
                    }
                    if !relax.is_empty() {
                        *parent = self.reshape_object(parent.take(), &ObjectEdit::Relax(relax));
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
                let object_schema = Value::Object(obj);
                let mut schema = if all_of.is_empty() {
                    object_schema
                } else {
                    all_of.push(object_schema);
                    json!({ "allOf": all_of })
                };
                if let Some(doc) = self.jsdoc(file, c.span) {
                    attach_doc(&mut schema, &doc);
                }
                schema
            }
        }
    }
}

/// The name of an interface or type-literal member, `None` for a computed key.
fn property_key(key: &Expr) -> Option<String> {
    match key {
        Expr::Ident(i) => Some(i.sym.to_string()),
        Expr::Lit(Lit::Str(s)) => Some(s.value.to_string_lossy().into_owned()),
        Expr::Lit(Lit::Num(n)) => Some(number_json(n.value).to_string()),
        _ => None,
    }
}

/// The name of a class member, `None` for a computed or private (`#x`) key.
fn prop_name_key(key: &swc_ecma_ast::PropName) -> Option<String> {
    match key {
        swc_ecma_ast::PropName::Ident(i) => Some(i.sym.to_string()),
        swc_ecma_ast::PropName::Str(s) => Some(s.value.to_string_lossy().into_owned()),
        swc_ecma_ast::PropName::Num(n) => Some(number_json(n.value).to_string()),
        _ => None,
    }
}

/// The decorator's own name, without a namespace (`@ns.Exclude()` -> `Exclude`).
fn decorator_base_name(d: &swc_ecma_ast::Decorator) -> String {
    let name = decorator_name(d);
    name.rsplit('.').next().unwrap_or_default().to_string()
}

/// `@Exclude()` (class-transformer) and `@ApiHideProperty()` (`@nestjs/swagger`)
/// keep a property off the wire, except `@Exclude({ toClassOnly: true })`,
/// which only drops it from incoming payloads.
fn hides_property(d: &swc_ecma_ast::Decorator) -> bool {
    match decorator_base_name(d).as_str() {
        "ApiHideProperty" => true,
        "Exclude" => decorator_option_bool(d, "toClassOnly") != Some(true),
        _ => false,
    }
}

/// `@IsOptional()`, `@ApiPropertyOptional()` and `@ApiProperty({ required: false })`.
fn marks_optional(d: &swc_ecma_ast::Decorator) -> bool {
    match decorator_base_name(d).as_str() {
        "IsOptional" | "ApiPropertyOptional" => true,
        "ApiProperty" => decorator_option_bool(d, "required") == Some(false),
        _ => false,
    }
}

/// A boolean literal option of a decorator's first-argument object literal.
fn decorator_option_bool(d: &swc_ecma_ast::Decorator, key: &str) -> Option<bool> {
    let Expr::Call(call) = &*d.expr else {
        return None;
    };
    let Expr::Object(obj) = &*call.args.first()?.expr else {
        return None;
    };
    obj.props.iter().find_map(|p| {
        let swc_ecma_ast::PropOrSpread::Prop(p) = p else {
            return None;
        };
        let swc_ecma_ast::Prop::KeyValue(kv) = &**p else {
            return None;
        };
        if prop_name_key(&kv.key).as_deref() != Some(key) {
            return None;
        }
        match &*kv.value {
            Expr::Lit(Lit::Bool(b)) => Some(b.value),
            _ => None,
        }
    })
}

fn enum_value_type(v: &EnumValue) -> &'static str {
    match v {
        EnumValue::Str(_) => "string",
        EnumValue::Num(_) => "number",
    }
}

/// An enum value as JSON, a whole number written as an integer (`1`, not `1.0`).
fn enum_value_json(v: &EnumValue) -> Value {
    match v {
        EnumValue::Str(s) => Value::String(s.clone()),
        EnumValue::Num(n) => number_json(*n),
    }
}

/// A number as JSON, written as an integer when it is a whole one.
pub(crate) fn number_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        Value::from(n as i64)
    } else {
        Value::from(n)
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
        Symbol::Class(c) => c.type_params.as_ref(),
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

/// One member of a lowered union, as `Exclude`/`Extract` compare them.
#[derive(Clone, PartialEq)]
enum Member {
    /// A literal value and its JSON type.
    Lit(&'static str, Value),
    /// A whole primitive type (`string`).
    Prim(String),
    Null,
    /// An unconstrained schema: `any`, `unknown` or `object`, which the
    /// schema cannot tell apart.
    Any,
    /// Anything else, compared by its schema.
    Other(Value),
}

/// Split a lowered schema into its union members, reading enum components
/// through their `$ref` and nested unions through their branches.
fn union_members(
    schemas: &IndexMap<String, Value>,
    schema: &Value,
    out: &mut Vec<Member>,
    depth: usize,
) {
    const MAX_DEPTH: usize = 16;
    // `void`/`undefined`/`never` lower to a JSON null and add no member.
    let Value::Object(map) = schema else {
        return;
    };
    if map.is_empty() {
        out.push(Member::Any);
        return;
    }
    if let Some(target) = map
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/components/schemas/"))
        .and_then(|name| schemas.get(name))
        && depth < MAX_DEPTH
    {
        let mut inner = Vec::new();
        union_members(schemas, target, &mut inner, depth + 1);
        match inner.as_slice() {
            [Member::Other(_)] => out.push(Member::Other(schema.clone())),
            _ => out.extend(inner),
        }
        return;
    }
    if let Some(Value::Array(branches)) = map.get("oneOf").or_else(|| map.get("anyOf")) {
        for branch in branches {
            union_members(schemas, branch, out, depth + 1);
        }
        return;
    }
    if let Some(Value::Array(values)) = map.get("enum") {
        out.extend(values.iter().map(|v| match v {
            Value::Null => Member::Null,
            Value::String(_) => Member::Lit("string", v.clone()),
            Value::Bool(_) => Member::Lit("boolean", v.clone()),
            _ => Member::Lit("number", v.clone()),
        }));
        return;
    }
    let types: Option<Vec<&str>> = match map.get("type") {
        Some(Value::String(t)) if map.len() == 1 => Some(vec![t]),
        Some(Value::Array(ts)) if map.len() == 1 => ts.iter().map(Value::as_str).collect(),
        _ => None,
    };
    match types {
        Some(types) => out.extend(types.into_iter().map(|t| match t {
            "null" => Member::Null,
            t => Member::Prim(t.to_string()),
        })),
        None => out.push(Member::Other(schema.clone())),
    }
}

/// Whether union member `m` is assignable to the union `to`, as TypeScript
/// decides it for `Exclude`/`Extract`. `None` when the schemas cannot tell,
/// such as two different object types, or an unresolved one.
fn assignable(m: &Member, to: &[Member]) -> Option<bool> {
    let opaque = |m: &Member| matches!(m, Member::Other(s) if s.get("x-unresolved").is_some());
    if to.contains(&Member::Any) {
        return None;
    }
    if to.contains(m) {
        return Some(true);
    }
    match m {
        Member::Lit(t, _) if to.contains(&Member::Prim(t.to_string())) => Some(true),
        Member::Lit(..) | Member::Prim(_) | Member::Null if to.iter().any(opaque) => None,
        Member::Lit(..) | Member::Prim(_) | Member::Null => Some(false),
        // An object type is never assignable to a primitive or a literal.
        Member::Other(_) if opaque(m) && to.iter().any(|t| *t != Member::Null) => None,
        Member::Other(_) if to.iter().any(|t| matches!(t, Member::Other(_))) => None,
        Member::Other(_) => Some(false),
        Member::Any => None,
    }
}

/// `Exclude<T, U>` (or `Extract<T, U>` when `keep_assignable`): the members of
/// `from` that are not (are) assignable to `filter`. A `from` that loses no
/// member is returned as written, so `Exclude<string, 'a'>` stays `string`.
/// `None` when a member's assignability cannot be decided. An unconstrained
/// `T` is kept by `Exclude`, which can only narrow it.
fn filter_union(
    schemas: &IndexMap<String, Value>,
    from: Value,
    filter: &Value,
    keep_assignable: bool,
) -> Option<Value> {
    let mut to = Vec::new();
    union_members(schemas, filter, &mut to, 0);
    let mut members = Vec::new();
    union_members(schemas, &from, &mut members, 0);
    if members.contains(&Member::Any) {
        return (!keep_assignable).then_some(from);
    }
    // `boolean` is `true | false`, so a boolean literal can split it.
    if to.iter().any(|t| matches!(t, Member::Lit("boolean", _))) {
        members = members
            .into_iter()
            .flat_map(|m| match m {
                Member::Prim(t) if t == "boolean" => vec![
                    Member::Lit("boolean", json!(true)),
                    Member::Lit("boolean", json!(false)),
                ],
                m => vec![m],
            })
            .collect();
    }
    let mut kept = Vec::new();
    for m in &members {
        if assignable(m, &to)? == keep_assignable {
            kept.push(m);
        }
    }
    if kept.len() == members.len() {
        return Some(from);
    }
    let mut branches: Vec<Value> = Vec::new();
    let mut nullable = false;
    for m in kept {
        match m {
            Member::Lit(t, v) => match branches
                .iter_mut()
                .find(|b| b.get("type") == Some(&json!(t)) && b.get("enum").is_some())
            {
                Some(b) => b["enum"].as_array_mut()?.push(v.clone()),
                None => branches.push(json!({ "type": t, "enum": [v] })),
            },
            Member::Prim(t) => branches.push(json!({ "type": t })),
            Member::Null => nullable = true,
            Member::Other(s) => branches.push(s.clone()),
            Member::Any => {}
        }
    }
    let schema = match branches.len() {
        0 if nullable => return Some(json!({ "type": "null" })),
        0 => json!({ "not": {} }),
        1 => branches.remove(0),
        _ => json!({ "anyOf": branches }),
    };
    Some(if nullable {
        make_nullable(schema)
    } else {
        schema
    })
}

/// A property-set edit, as made by `Partial`/`Required`/`Omit`/`Pick` and
/// their `@nestjs/mapped-types` counterparts.
#[derive(Clone)]
enum ObjectEdit {
    AllOptional,
    AllRequired,
    Omit(Vec<String>),
    Pick(Vec<String>),
    /// Make these properties optional.
    Relax(Vec<String>),
}

impl ObjectEdit {
    /// Prefix of the component name a deferred edit is registered under.
    fn prefix(&self) -> &'static str {
        match self {
            Self::AllOptional => "Partial",
            Self::AllRequired => "Required",
            Self::Omit(_) => "Omit",
            Self::Pick(_) => "Pick",
            Self::Relax(_) => "Optional",
        }
    }
}

/// The object `schema` flattens to, or, past an unresolved ancestor, the one
/// the branches beside it flatten to.
fn inherited_object(
    schemas: &IndexMap<String, Value>,
    schema: &Value,
) -> Option<Map<String, Value>> {
    if let Some(obj) = flatten_object(schemas, schema, 0) {
        return Some(obj);
    }
    let mut unresolved = Vec::new();
    let mut resolvable = Vec::new();
    split_unresolved(schemas, schema, &mut unresolved, &mut resolvable, 0)?;
    flatten_object(schemas, &json!({ "allOf": resolvable }), 0)
}

/// Apply a property-set edit to `schema`, inlined into one object first so
/// inherited properties are edited too. A parent nestoa could not resolve
/// (`x-unresolved`) cannot be inlined: it is kept as an `allOf` branch beside
/// the edited remainder, so the gap stays visible. `None` when `schema` is not
/// object-shaped.
fn reshape(schemas: &IndexMap<String, Value>, schema: &Value, edit: &ObjectEdit) -> Option<Value> {
    if let Some(obj) = flatten_object(schemas, schema, 0) {
        return Some(apply_edit(obj, edit));
    }
    let mut unresolved = Vec::new();
    let mut resolvable = Vec::new();
    split_unresolved(schemas, schema, &mut unresolved, &mut resolvable, 0)?;
    if unresolved.is_empty() || resolvable.is_empty() {
        return None;
    }
    let obj = flatten_object(schemas, &json!({ "allOf": resolvable }), 0)?;
    unresolved.push(apply_edit(obj, edit));
    Some(json!({ "allOf": unresolved }))
}

/// Split an object schema into its `x-unresolved` parents and the branches
/// that flatten, following `$ref`s and `allOf`s. `None` if any other part
/// cannot be flattened.
fn split_unresolved(
    schemas: &IndexMap<String, Value>,
    schema: &Value,
    unresolved: &mut Vec<Value>,
    resolvable: &mut Vec<Value>,
    depth: usize,
) -> Option<()> {
    const MAX_DEPTH: usize = 32;
    if depth > MAX_DEPTH {
        return None;
    }
    if flatten_object(schemas, schema, depth).is_some() {
        resolvable.push(schema.clone());
        return Some(());
    }
    let map = schema.as_object()?;
    if map.contains_key("x-unresolved") {
        unresolved.push(schema.clone());
        return Some(());
    }
    if let Some(target) = map.get("$ref").and_then(Value::as_str) {
        let target = schemas.get(target.strip_prefix("#/components/schemas/")?)?;
        return split_unresolved(schemas, target, unresolved, resolvable, depth + 1);
    }
    let branches = map.get("allOf")?.as_array()?;
    if map.keys().any(|k| k != "allOf" && !is_annotation(k)) {
        return None;
    }
    for branch in branches {
        split_unresolved(schemas, branch, unresolved, resolvable, depth + 1)?;
    }
    Some(())
}

/// Edit the property set of a flattened object schema.
fn apply_edit(mut obj: Map<String, Value>, edit: &ObjectEdit) -> Value {
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
        ObjectEdit::Relax(keys) => required.retain(|k| !keys.contains(k)),
        ObjectEdit::AllRequired => required = keys,
        ObjectEdit::Omit(drop) | ObjectEdit::Pick(drop) => {
            let keep = |k: &str| drop.iter().any(|d| d == k) == matches!(edit, ObjectEdit::Pick(_));
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
/// component and `allOf` branches are merged as an intersection: their
/// `properties` and `required` are combined, a property several branches
/// declare keeps every schema, and other keywords go through `merge_keyword`.
/// `None` for anything that is not object-shaped, or whose branches carry
/// constraints one object cannot combine (including any dictionary keyword).
fn flatten_object(
    schemas: &IndexMap<String, Value>,
    schema: &Value,
    depth: usize,
) -> Option<Map<String, Value>> {
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
        let mut out = flatten_object(schemas, schemas.get(name)?, depth + 1)?;
        out.extend(siblings("$ref"));
        return Some(out);
    }
    if let Some(Value::Array(branches)) = map.get("allOf") {
        let mut out = Map::new();
        let mut props = Map::new();
        let mut required: Vec<Value> = Vec::new();
        for branch in branches {
            let mut b = flatten_object(schemas, branch, depth + 1)?;
            // A dictionary keyword only governs the properties its own branch
            // does not declare; beside merged `properties` it would stop
            // applying to the other branches' fields, so such a shape stays
            // unflattened.
            if branches.len() > 1 && DICTIONARY_KEYWORDS.iter().any(|k| b.contains_key(*k)) {
                return None;
            }
            if let Some(Value::Object(p)) = b.remove("properties") {
                intersect_schema_map(&mut props, p);
            }
            if let Some(Value::Array(r)) = b.remove("required") {
                for k in r {
                    if !required.contains(&k) {
                        required.push(k);
                    }
                }
            }
            for (k, v) in b {
                merge_keyword(&mut out, k, v)?;
            }
        }
        out.extend(siblings("allOf"));
        out.insert("type".into(), Value::String("object".into()));
        out.insert("properties".into(), Value::Object(props));
        if !required.is_empty() {
            out.insert("required".into(), Value::Array(required));
        }
        return Some(out);
    }
    let is_object =
        map.get("type").and_then(Value::as_str) == Some("object") || map.contains_key("properties");
    is_object.then(|| map.clone())
}

/// Merge `incoming` into a `properties` map as an intersection: a name both
/// declare keeps both schemas, combined with `allOf`.
fn intersect_schema_map(into: &mut Map<String, Value>, incoming: Map<String, Value>) {
    for (k, v) in incoming {
        match into.get_mut(&k) {
            Some(existing) if *existing != v => {
                *existing = json!({ "allOf": [existing.take(), v] });
            }
            Some(_) => {}
            None => {
                into.insert(k, v);
            }
        }
    }
}

/// Keywords constraining the properties a schema does not declare by name.
const DICTIONARY_KEYWORDS: [&str; 4] = [
    "additionalProperties",
    "patternProperties",
    "propertyNames",
    "unevaluatedProperties",
];

/// Merge one keyword of an `allOf` branch into the flattened object.
/// Annotations keep the later value; `None` means two constraints differ in a
/// way a single object cannot express.
fn merge_keyword(out: &mut Map<String, Value>, key: String, value: Value) -> Option<()> {
    match out.get_mut(&key) {
        None => {
            out.insert(key, value);
        }
        Some(existing) if *existing == value => {}
        Some(existing) if is_annotation(&key) => *existing = value,
        Some(_) => return None,
    }
    Some(())
}

/// Keywords that document a schema without constraining its instances.
fn is_annotation(key: &str) -> bool {
    matches!(
        key,
        "title" | "description" | "example" | "examples" | "default" | "deprecated"
    ) || key.starts_with("x-")
}

/// The string literals of an array expression such as the key list of
/// `OmitType(Dto, ['a', 'b'] as const)`. `None` if any entry is not a string
/// literal (a constant, a spread), so the key set is never partial.
fn string_array_keys(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::TsConstAssertion(c) => string_array_keys(&c.expr),
        Expr::TsAs(a) => string_array_keys(&a.expr),
        Expr::Paren(p) => string_array_keys(&p.expr),
        Expr::Array(arr) => arr
            .elems
            .iter()
            .map(|el| match el {
                Some(el) if el.spread.is_none() => match el.expr.as_ref() {
                    Expr::Lit(Lit::Str(s)) => Some(s.value.to_string_lossy().into_owned()),
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// A class reference (`Dto`, `Models.Dto`) as a type-entity name.
fn entity_name_of(expr: &Expr) -> Option<TsEntityName> {
    match expr {
        Expr::Ident(id) => Some(TsEntityName::Ident(id.clone())),
        Expr::Member(m) => {
            let swc_ecma_ast::MemberProp::Ident(prop) = &m.prop else {
                return None;
            };
            Some(TsEntityName::TsQualifiedName(Box::new(
                swc_ecma_ast::TsQualifiedName {
                    span: m.span,
                    left: entity_name_of(&m.obj)?,
                    right: prop.clone(),
                },
            )))
        }
        _ => None,
    }
}

/// How an `extends` clause nestoa cannot lower is named in `x-unresolved`.
fn heritage_label(expr: &Expr) -> String {
    match expr {
        Expr::Call(call) => match &call.callee {
            swc_ecma_ast::Callee::Expr(callee) => match entity_name_of(callee) {
                Some(name) => format!("{}(…)", entity_name(&name)),
                None => "extends".to_string(),
            },
            _ => "extends".to_string(),
        },
        Expr::Paren(p) => heritage_label(&p.expr),
        _ => entity_name_of(expr)
            .map(|n| entity_name(&n))
            .unwrap_or_else(|| "extends".to_string()),
    }
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

/// A tuple from its fixed positions and the schema of every later one
/// (`false` for a closed tuple). `prefixItems` must not be empty, so a tuple
/// without fixed positions is a plain array, and `[]` admits no element.
fn tuple_schema(prefix: Vec<Value>, tail: Value) -> Value {
    match (prefix.is_empty(), tail) {
        (true, Value::Bool(false)) => json!({ "type": "array", "items": {}, "maxItems": 0 }),
        (true, tail) => json!({ "type": "array", "items": tail }),
        (false, tail) => json!({ "type": "array", "prefixItems": prefix, "items": tail }),
    }
}

/// What a tuple's rest element `...R` adds: the fixed positions of a spread
/// tuple, and the schema of the positions after them. An `x-unresolved` rest
/// is kept as that schema so the gap stays visible; `None` for a rest that is
/// not an array, such as a union of them.
fn spread_elements(schemas: &IndexMap<String, Value>, rest: Value) -> Option<(Vec<Value>, Value)> {
    const MAX_DEPTH: usize = 16;
    let mut rest = rest;
    for _ in 0..MAX_DEPTH {
        match rest
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/components/schemas/"))
            .and_then(|name| schemas.get(name))
        {
            Some(target) => rest = target.clone(),
            None => break,
        }
    }
    if rest.get("x-unresolved").is_some() {
        return Some((Vec::new(), rest));
    }
    if rest.get("type") != Some(&json!("array")) {
        return None;
    }
    // `[]`, which `tuple_schema` writes with `maxItems: 0` rather than `items`.
    if rest.get("maxItems") == Some(&json!(0)) {
        return Some((Vec::new(), Value::Bool(false)));
    }
    let prefix = match rest.get("prefixItems") {
        Some(Value::Array(p)) => p.clone(),
        _ => Vec::new(),
    };
    let tail = rest.get("items").cloned().unwrap_or_else(|| json!({}));
    Some((prefix, tail))
}

/// `NonNullable<T>`: `schema` without its `null` member. A `$ref` to a
/// nullable component is inlined, since the component itself must stay as is.
fn strip_null(schemas: &IndexMap<String, Value>, schema: Value) -> Value {
    strip_null_at(schemas, schema, 0)
}

fn strip_null_at(schemas: &IndexMap<String, Value>, schema: Value, depth: usize) -> Value {
    const MAX_DEPTH: usize = 16;
    let Value::Object(mut m) = schema else {
        return schema;
    };
    if let Some(target) = m
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/components/schemas/"))
        .and_then(|name| schemas.get(name))
        && depth < MAX_DEPTH
    {
        let stripped = strip_null_at(schemas, target.clone(), depth + 1);
        return if stripped == *target {
            Value::Object(m)
        } else {
            stripped
        };
    }
    match m.get("type").cloned() {
        Some(Value::String(t)) if t == "null" => return json!({ "not": {} }),
        Some(Value::Array(types)) => {
            let kept: Vec<Value> = types
                .into_iter()
                .filter(|t| t.as_str() != Some("null"))
                .collect();
            let kept = match <[Value; 1]>::try_from(kept) {
                Ok([single]) => single,
                Err(kept) => Value::Array(kept),
            };
            m.insert("type".into(), kept);
        }
        _ => {}
    }
    if let Some(Value::Array(values)) = m.get_mut("enum") {
        values.retain(|v| !v.is_null());
    }
    // Every conjunct must admit the value, so each loses its null; a null-only
    // one becomes unsatisfiable rather than being dropped.
    if let Some(Value::Array(conjuncts)) = m.get_mut("allOf") {
        for conjunct in conjuncts.iter_mut() {
            *conjunct = strip_null_at(schemas, conjunct.take(), depth + 1);
        }
    }
    let only_key = m.len() == 1;
    for key in ["anyOf", "oneOf"] {
        let Some(Value::Array(branches)) = m.get_mut(key) else {
            continue;
        };
        branches.retain(|b| b.get("type").and_then(Value::as_str) != Some("null"));
        for branch in branches.iter_mut() {
            *branch = strip_null_at(schemas, branch.take(), depth + 1);
        }
        if branches.len() == 1 && only_key {
            return branches.pop().expect("one branch");
        }
    }
    Value::Object(m)
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
    json!({ "anyOf": [Value::Object(m), { "type": "null" }] })
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
        lower_with(&[("dto.ts", dto)], main, names)
    }

    /// `lower_all` with any set of sibling files next to `main.ts`.
    fn lower_with(files: &[(&str, &str)], main: &str, names: &[&str]) -> Vec<Value> {
        let mut all = files.to_vec();
        all.push(("main.ts", main));
        let (project, dir) = project_from(&all);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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

    #[test]
    fn namespace_members_lower_like_top_level_ones() {
        let [holder] = lower_with(
            &[(
                "types.ts",
                "export namespace Types {\n\
                   /** The outcome. */\n\
                   export interface Result { ok: boolean; }\n\
                   export namespace Inner { export enum Status { A = 'a', B = 'b' } }\n\
                 }",
            )],
            "import { Types } from './types';\n\
             export interface Holder { result: Types.Result; status: Types.Inner.Status.B; }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        assert_eq!(
            holder["properties"]["result"]["$ref"],
            "#/components/schemas/Result"
        );
        assert_eq!(
            holder["properties"]["status"],
            json!({"type": "string", "enum": ["b"]})
        );
    }

    #[test]
    fn interface_declarations_merge() {
        let [merged] = lower_all(
            "",
            "interface Base { id: string; }\n\
             interface Merged { a: string; }\n\
             interface Merged extends Base { b?: number; a: string; }",
            &["Merged"],
        )
        .try_into()
        .unwrap();
        let object = &merged["allOf"][1];
        assert_eq!(prop_names(object), ["a", "b"]);
        assert_eq!(object["required"], json!(["a"]));
        assert_eq!(merged["allOf"][0]["$ref"], "#/components/schemas/Base");
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
    fn import_aliases_share_the_declaration_component() {
        let [first, second, direct] = lower_with(
            &[
                ("dto.ts", "export class Base { id: string; }"),
                ("other.ts", "export class Base { name: string; }"),
            ],
            "import { Base as First, Base } from './dto';\n\
             import { Base as Second } from './other';\n\
             export type A = First;\n\
             export type B = Second;\n\
             export type C = Base;",
            &["A", "B", "C"],
        )
        .try_into()
        .unwrap();
        // Distinct declarations keep the aliases that tell them apart…
        assert_eq!(first, json!({ "$ref": "#/components/schemas/First" }));
        assert_eq!(second, json!({ "$ref": "#/components/schemas/Second" }));
        // …and a direct reference reuses its declaration's component.
        assert_eq!(direct, first);
    }

    #[test]
    fn enums_follow_typescript_member_values() {
        let [
            level,
            neg,
            strings,
            mixed,
            computed,
            member,
            implicit_member,
        ] = lower_all(
            "export enum Level { A = 1, B, C }\n\
             export enum Neg { A = -1, B }\n\
             export enum S { X = 'x', Y = 'y' }\n\
             export enum Mixed { A = 'a', B = 2 }\n\
             export enum Computed { A = 1 << 1, B }",
            "import { Level, Neg, S, Mixed, Computed } from './dto';\n\
             export type M = Level.C;\n\
             export type U = Computed.B;",
            &["Level", "Neg", "S", "Mixed", "Computed", "M", "U"],
        )
        .try_into()
        .unwrap();
        assert_eq!(level, json!({ "type": "number", "enum": [1, 2, 3] }));
        assert_eq!(neg, json!({ "type": "number", "enum": [-1, 0] }));
        assert_eq!(strings, json!({ "type": "string", "enum": ["x", "y"] }));
        assert_eq!(
            mixed,
            json!({ "type": ["string", "number"], "enum": ["a", 2] })
        );
        assert_eq!(computed, json!({ "type": "number", "enum": [2, 3] }));
        assert_eq!(member, json!({ "type": "number", "enum": [3] }));
        assert_eq!(implicit_member, json!({ "type": "number", "enum": [3] }));
    }

    #[test]
    fn ambient_enum_implicit_members_are_unknown() {
        let [ambient, constant] = lower_all(
            "export declare enum Ambient { A = 1, B }\n\
             export declare const enum Constant { A = 1, B }",
            "export { Ambient, Constant } from './dto';",
            &["Ambient", "Constant"],
        )
        .try_into()
        .unwrap();
        assert_eq!(ambient, json!({ "x-unresolved": "Ambient" }));
        assert_eq!(constant, json!({ "type": "number", "enum": [1, 2] }));
    }

    #[test]
    fn empty_enum_admits_no_value() {
        let [empty] = lower_all(
            "export enum Empty {}",
            "export { Empty } from './dto';",
            &["Empty"],
        )
        .try_into()
        .unwrap();
        assert_eq!(empty, json!({ "not": {} }));
    }

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
    fn flattening_declines_branches_with_dictionary_keywords() {
        let [records, mixed] = lower_all(
            "export class Base { id: string; }",
            "export type R = Partial<Record<string, string> & Record<string, number>>;\n\
             export type M = Partial<Record<string, string> & { count: number }>;",
            &["R", "M"],
        )
        .try_into()
        .unwrap();
        // Hoisting `additionalProperties` beside merged properties would stop
        // it from applying to `count`, so both stay intersections.
        assert_eq!(records["allOf"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            mixed,
            json!({ "allOf": [
                { "type": "object", "additionalProperties": { "type": "string" } },
                { "type": "object", "properties": { "count": { "type": "number" } }, "required": ["count"] },
            ] })
        );

        let mut out = Map::new();
        merge_keyword(&mut out, "maxProperties".into(), json!(1)).unwrap();
        assert!(merge_keyword(&mut out, "maxProperties".into(), json!(2)).is_none());
        merge_keyword(&mut out, "description".into(), json!("a")).unwrap();
        merge_keyword(&mut out, "description".into(), json!("b")).unwrap();
        assert_eq!(out["description"], "b");
    }

    #[test]
    fn edit_on_a_component_still_being_lowered_is_deferred() {
        let (project, dir) = project_from(&[(
            "node.ts",
            "export class Node { id: string; name: string; child?: Partial<Node>; }",
        )]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let node = ctx.named_type_to_schema(&dir.join("node.ts"), "Node");
        assert_eq!(node, json!({ "$ref": "#/components/schemas/Node" }));
        components.resolve_deferred();
        let child = &components.schemas["Node"]["properties"]["child"];
        assert_eq!(
            *child,
            json!({ "$ref": "#/components/schemas/PartialNode" })
        );
        let partial = &components.schemas["PartialNode"];
        assert_eq!(prop_names(partial), ["id", "name", "child"]);
        assert!(required_names(partial).is_empty());
        assert_eq!(partial["properties"]["child"], *child);
    }

    #[test]
    fn mapped_type_helpers_resolve_through_imports_or_stay_visible() {
        let schemas = lower_all(
            BASE_AND_CHILD,
            "import { PartialType as MakePartial, PickType, IntersectionType } from '@nestjs/mapped-types';\n\
             import * as swagger from '@nestjs/swagger';\n\
             import * as Models from './dto';\n\
             import { Child } from './dto';\n\
             class Extra { tag: string; }\n\
             const KEY = 'id';\n\
             export class Aliased extends MakePartial(Child) {}\n\
             export class Namespaced extends swagger.PickType(Child, ['id']) {}\n\
             export class Qualified extends IntersectionType(Models.Base, Extra) {}\n\
             export class DynamicKeys extends PickType(Child, [KEY]) {}\n\
             export class PartialOperand extends IntersectionType(Child, makeMixin()) {}",
            &[
                "Aliased",
                "Namespaced",
                "Qualified",
                "DynamicKeys",
                "PartialOperand",
            ],
        );
        let parent = |s: &Value| s["allOf"][0].clone();
        let [aliased, namespaced, qualified, dynamic, partial_operand] =
            <[Value; 5]>::try_from(schemas.iter().map(parent).collect::<Vec<_>>()).unwrap();
        assert_eq!(prop_names(&aliased), ["id", "secret", "extra", "note"]);
        assert!(required_names(&aliased).is_empty());
        assert_eq!(prop_names(&namespaced), ["id"]);
        assert_eq!(
            qualified["allOf"][0],
            json!({ "$ref": "#/components/schemas/Base" })
        );
        // Never a partial edit or a silently dropped operand.
        assert_eq!(dynamic, json!({ "x-unresolved": "PickType(…)" }));
        assert_eq!(
            partial_operand,
            json!({ "x-unresolved": "IntersectionType(…)" })
        );
    }

    #[test]
    fn only_nest_mapped_type_helpers_get_their_semantics() {
        let schemas = lower_with(
            &[
                ("dto.ts", BASE_AND_CHILD),
                (
                    "barrel.ts",
                    "export { PartialType } from '@nestjs/swagger';\n\
                     export * as swagger from '@nestjs/swagger';",
                ),
                (
                    "own.ts",
                    "export function PickType(base: any, keys: any) { return base; }",
                ),
                ("star.ts", "export * from './barrel';"),
                (
                    "alias.ts",
                    "import { PartialType } from '@nestjs/swagger';\n\
                     import * as sw from '@nestjs/swagger';\n\
                     export { PartialType as MakePartial, sw as nest };",
                ),
            ],
            "import { PartialType, swagger } from './barrel';\n\
             import { swagger as starred } from './star';\n\
             import { MakePartial, nest } from './alias';\n\
             import { PickType } from './own';\n\
             import { Child } from './dto';\n\
             function OmitType(base: any, keys: any) { return base; }\n\
             export class ViaBarrel extends PartialType(Child) {}\n\
             export class Imported extends PickType(Child, ['id']) {}\n\
             export class Local extends OmitType(Child, ['id']) {}\n\
             export class NamespaceBarrel extends swagger.PickType(Child, ['id']) {}\n\
             export class StarBarrel extends starred.PickType(Child, ['id']) {}\n\
             export class LocalAlias extends MakePartial(Child) {}\n\
             export class LocalNamespace extends nest.PickType(Child, ['id']) {}",
            &[
                "ViaBarrel",
                "Imported",
                "Local",
                "NamespaceBarrel",
                "StarBarrel",
                "LocalAlias",
                "LocalNamespace",
            ],
        );
        assert_eq!(prop_names(&schemas[3]["allOf"][0]), ["id"]);
        assert_eq!(prop_names(&schemas[4]["allOf"][0]), ["id"]);
        assert_eq!(required_names(&schemas[5]["allOf"][0]), Vec::<&str>::new());
        assert_eq!(prop_names(&schemas[5]["allOf"][0]).len(), 4);
        assert_eq!(prop_names(&schemas[6]["allOf"][0]), ["id"]);
        assert_eq!(
            prop_names(&schemas[0]["allOf"][0]),
            ["id", "secret", "extra", "note"]
        );
        assert_eq!(
            schemas[1]["allOf"][0],
            json!({ "x-unresolved": "PickType(…)" })
        );
        assert_eq!(
            schemas[2]["allOf"][0],
            json!({ "x-unresolved": "OmitType(…)" })
        );
    }

    #[test]
    fn local_export_aliases_and_namespaces_through_star_barrels_resolve() {
        let [aliased, namespaced] = lower_with(
            &[
                (
                    "dto.ts",
                    "class Base { id: string; }\nexport { Base as Renamed };",
                ),
                ("enums.ts", "export enum Kind { A = 'a' }"),
                ("ns.ts", "export * as Enums from './enums';"),
                ("barrel.ts", "export * from './ns';"),
            ],
            "import { Renamed } from './dto';\n\
             import { Enums } from './barrel';\n\
             export type A = Renamed;\n\
             export type B = Enums.Kind;",
            &["A", "B"],
        )
        .try_into()
        .unwrap();
        assert_eq!(aliased, json!({ "$ref": "#/components/schemas/Renamed" }));
        assert_eq!(namespaced, json!({ "$ref": "#/components/schemas/Kind" }));
    }

    #[test]
    fn javascript_specifiers_reach_typescript_declarations() {
        let [item, cts_item, dto] = lower_with(
            &[
                (
                    "types.d.mts",
                    "export declare interface Item { id: string; }",
                ),
                (
                    "types.d.cts",
                    "export declare interface CtsItem { id: string; }",
                ),
                ("barrel.ts", "export * from './types.mjs';"),
                ("cts-barrel.ts", "export * from './types.cjs';"),
                ("dto.ts", "export class Dto { name: string; }"),
            ],
            "import { Item } from './barrel.js';\n\
             import { CtsItem } from './cts-barrel.js';\n\
             import { Dto } from './dto.js';\n\
             export type A = Item;\n\
             export type C = CtsItem;\n\
             export type B = Dto;",
            &["A", "C", "B"],
        )
        .try_into()
        .unwrap();
        assert_eq!(item, json!({ "$ref": "#/components/schemas/Item" }));
        assert_eq!(cts_item, json!({ "$ref": "#/components/schemas/CtsItem" }));
        assert_eq!(dto, json!({ "$ref": "#/components/schemas/Dto" }));
    }

    #[test]
    fn utility_keys_resolve_aliases_and_never_apply_partially() {
        let [aliased, keyof, mixed] = lower_all(
            BASE_AND_CHILD,
            "import { Child } from './dto';\n\
             type Keys = 'id' | Extra;\n\
             type Extra = 'note';\n\
             export type A = Pick<Child, Keys>;\n\
             export type K = Pick<Child, keyof Child>;\n\
             export type M = Omit<Child, 'id' | keyof Child>;",
            &["A", "K", "M"],
        )
        .try_into()
        .unwrap();
        assert_eq!(prop_names(&aliased), ["id", "note"]);
        assert_eq!(keyof, json!({ "x-unresolved": "Pick<…>" }));
        assert_eq!(mixed, json!({ "x-unresolved": "Omit<…>" }));
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
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
    fn interface_extends_a_namespace_member_or_stays_visible() {
        let [known, unknown] = lower_with(
            &[("base.ts", "export interface Base { a: string; }")],
            "import * as Models from './base';\n\
             export interface Known extends Models.Base { b: number; }\n\
             export interface Unknown extends Missing.Base { b: number; }",
            &["Known", "Unknown"],
        )
        .try_into()
        .unwrap();
        assert_eq!(
            known["allOf"][0],
            json!({ "$ref": "#/components/schemas/Base" })
        );
        assert_eq!(
            unknown["allOf"][0],
            json!({ "x-unresolved": "Missing.Base" })
        );
    }

    #[test]
    fn type_operators_and_indexed_access_are_lowered() {
        let [shapes] = lower_all(
            "export enum Level { Low = 'low', High = 'high' }\n\
             export class Base { id: string; count?: number; }",
            "import { Base, Level } from './dto';\n\
             export class Shapes {\n\
               list: readonly string[];\n\
               level: keyof typeof Level;\n\
               id: Base['id'];\n\
               pair: [string, number?];\n\
             }",
            &["Shapes"],
        )
        .try_into()
        .unwrap();
        let p = &shapes["properties"];
        assert_eq!(
            p["list"],
            json!({ "type": "array", "items": { "type": "string" } })
        );
        assert_eq!(
            p["level"],
            json!({ "type": "string", "enum": ["Low", "High"] })
        );
        assert_eq!(p["id"], json!({ "type": "string" }));
        assert_eq!(p["pair"]["prefixItems"][1], json!({ "type": "number" }));
    }

    #[test]
    fn unsupported_type_forms_stay_visible() {
        let main = "const config = { a: 1 };\n\
             export class Dict { [k: string]: number; }\n\
             export class Gaps {\n\
               mapped: { [K in 'a']: K };\n\
               cond: string extends number ? 1 : 2;\n\
               query: typeof config;\n\
               keys: keyof typeof config;\n\
               dict: keyof Dict;\n\
               missing: Dict['nope'];\n\
               tpl: `v${number}`;\n\
             }";
        let (project, dir) = project_from(&[("main.ts", main)]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let file = dir.join("main.ts");
        let (decl_file, sym) = project.resolve(&file, "Gaps").unwrap();
        let p = ctx.symbol_to_schema(&decl_file, &sym)["properties"].clone();
        let unresolved = |k: &str| p[k]["x-unresolved"].clone();
        assert_eq!(unresolved("mapped"), json!("{ [K in 'a']: K }"));
        assert_eq!(unresolved("cond"), json!("string extends number ? 1 : 2"));
        assert_eq!(unresolved("query"), json!("typeof config"));
        assert_eq!(unresolved("keys"), json!("keyof typeof config"));
        assert_eq!(unresolved("missing"), json!("Dict['nope']"));
        assert_eq!(
            p["tpl"],
            json!({ "type": "string", "x-unresolved": "`v${number}`" })
        );
        // An index signature is not a property, so the class's keys are unknown
        // even though its `properties` is empty.
        assert_eq!(unresolved("dict"), json!("keyof Dict"));
    }

    #[test]
    fn exclude_and_extract_filter_union_members() {
        let schemas = lower_all(
            "export enum Status { Ok = 'ok', Invalid = 'invalid', Failed = 'failed' }\n\
             export type Failure = Status.Invalid | Status.Failed;\n\
             export class Item { id: string; }\n\
             export class Other { name: string; }",
            "import { Status, Failure, Item, Other } from './dto';\n\
             export type Letters = Exclude<'a' | 'b' | 'c', 'b'>;\n\
             export type Ok = Exclude<Status, Failure>;\n\
             export type Name = Exclude<string, 'a' | 'b'>;\n\
             export type Failures = Extract<Status, Failure | 'other'>;\n\
             export type Strings = Extract<'a' | 1 | true, string>;\n\
             export type Never = Extract<string, 'a'>;\n\
             export type Present = Exclude<Item | null | undefined, null | undefined>;\n\
             export type Flag = Exclude<boolean | null, false>;\n\
             export type Mixed = Exclude<Item | 'none', string>;\n\
             export type Unknown = Exclude<Item | Other, Item>;\n\
             export type NotObject = Exclude<string | Item, object>;\n\
             export type Symbols = Extract<string | symbol, symbol>;\n\
             export type FromAny = Exclude<any, string>;\n\
             export type Scalars = Exclude<string | number | boolean, boolean>;",
            &[
                "Letters",
                "Ok",
                "Name",
                "Failures",
                "Strings",
                "Never",
                "Present",
                "Flag",
                "Mixed",
                "Unknown",
                "NotObject",
                "Symbols",
                "FromAny",
                "Scalars",
            ],
        );
        let item = json!({ "$ref": "#/components/schemas/Item" });
        let expected = [
            json!({ "type": "string", "enum": ["a", "c"] }),
            json!({ "type": "string", "enum": ["ok"] }),
            json!({ "type": "string" }),
            json!({ "type": "string", "enum": ["invalid", "failed"] }),
            json!({ "type": "string", "enum": ["a"] }),
            json!({ "not": {} }),
            item.clone(),
            json!({ "type": ["boolean", "null"], "enum": [true, null] }),
            item,
            // `Item` and `Other` are both objects: the schemas cannot tell
            // whether one is assignable to the other.
            json!({ "x-unresolved": "Exclude<…>" }),
            // `object`, like `any`, lowers to `{}`: it filters nothing decidably.
            json!({ "x-unresolved": "Exclude<…>" }),
            json!({ "x-unresolved": "Extract<…>" }),
            json!({}),
            json!({ "anyOf": [{ "type": "string" }, { "type": "number" }] }),
        ];
        assert_eq!(schemas.len(), expected.len());
        for (got, want) in schemas.iter().zip(expected) {
            assert_eq!(*got, want);
        }
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
        let passthrough = vec!["PopulatedDoc".to_string()];
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
        let mut ctx = SchemaCtx {
            project: &project,
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
        let (project, dir) = project_from(&[(
            "dto.ts",
            "export class Dto {\n  /** Public field */\n  name?: string;\n  /**\n   * Raw data from the supplier\n   * @internal\n   */\n  rawData?: string;\n  /** @private */\n  secret?: string;\n}",
        )]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
        let (project, dir) = project_from(&[(
            "iface.ts",
            "export interface Iface {\n  keep: string;\n  /** @internal */\n  drop: string;\n}",
        )]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
        let (project, dir) = project_from(&[
            ("kind.ts", "export interface Kind { label: string; }"),
            (
                "dto.ts",
                "import { Kind } from './kind';\nexport class Dto {\n  /** Account's type */\n  kind?: Kind;\n  /** Plain field */\n  name?: string;\n}",
            ),
        ]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
        let (project2, dir2) = project_from(&[
            ("kind.ts", "export interface Kind { label: string; }"),
            (
                "dto.ts",
                "import { Kind } from './kind';\nexport class Dto {\n  kind?: Kind;\n}",
            ),
        ]);
        let mut components2 = ComponentSchemas::default();
        let mut ctx2 = SchemaCtx {
            project: &project2,
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
    fn jsdoc_before_export_and_decorators_documents_the_declaration() {
        let (project, dir) = project_from(&[(
            "dto.ts",
            "/** A widget */\nexport interface Widget { id: string; }\n\
             /** A gadget */\n@Schema()\nexport class Gadget { id: string; }\n\
             /** A colour */\nexport enum Colour { Red = 'red' }\n\
             /** A part */\n@Schema()\nclass Part { id: string; }\n\
             export { Part };\n\
             /** A child */\nexport interface Child extends Widget { name: string; }\n\
             /** A sub-part */\n@Schema()\nexport class SubPart extends Part { name: string; }",
        )]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        for (name, doc) in [
            ("Widget", "A widget"),
            ("Gadget", "A gadget"),
            ("Colour", "A colour"),
            ("Part", "A part"),
            ("Child", "A child"),
            ("SubPart", "A sub-part"),
        ] {
            let (decl_file, sym) = project.resolve(&dir.join("dto.ts"), name).expect(name);
            let schema = ctx.symbol_to_schema(&decl_file, &sym);
            assert_eq!(
                schema.get("description"),
                Some(&json!(doc)),
                "{name}: {schema}"
            );
        }
    }

    #[test]
    fn unions_use_any_of_so_overlapping_members_validate() {
        // A `Child` is also a valid `Base`: under `oneOf` it would match both
        // branches and be rejected.
        let (project, dir) = project_from(&[(
            "dto.ts",
            "export interface Base { id: string; }\nexport interface Child extends Base { name: string; }\nexport interface Dto { item: Base | Child; }",
        )]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let (decl_file, sym) = project
            .resolve(&dir.join("dto.ts"), "Dto")
            .expect("resolve Dto");
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        assert_eq!(
            schema.pointer("/properties/item").unwrap(),
            &json!({"anyOf": [
                {"$ref": "#/components/schemas/Base"},
                {"$ref": "#/components/schemas/Child"}
            ]})
        );
    }

    #[test]
    fn nullable_unions_use_json_schema_null_not_nullable_keyword() {
        // `nullable: true` is a 3.0 keyword with no meaning in the 3.1 document
        // nestoa emits, and beside a `$ref`/`anyOf` it is a no-op even in 3.0.
        let (project, dir) = project_from(&[
            ("kind.ts", "export interface Kind { label: string; }"),
            (
                "dto.ts",
                "import { Kind } from './kind';\nexport interface Dto {\n  name: string | null;\n  kind: Kind | null;\n  state: 'a' | 'b' | null;\n  mixed: string | number | null;\n  maybe?: Kind | undefined;\n}",
            ),
        ]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
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
            kind.pointer("/anyOf/0/$ref").and_then(|v| v.as_str()),
            Some("#/components/schemas/Kind"),
            "got {kind}"
        );
        assert_eq!(kind.pointer("/anyOf/1/type").unwrap(), &json!("null"));

        // An enum constrains values independently of `type`, so `null` joins both.
        let state = props.get("state").unwrap();
        assert_eq!(state.get("type").unwrap(), &json!(["string", "null"]));
        assert_eq!(state.get("enum").unwrap(), &json!(["a", "b", null]));

        // An anyOf gains an explicit null member rather than a sibling keyword.
        let mixed = props.get("mixed").unwrap();
        let branches = mixed.get("anyOf").and_then(|v| v.as_array()).unwrap();
        assert_eq!(branches.len(), 3, "got {mixed}");
        assert_eq!(branches[2].get("type").unwrap(), &json!("null"));

        // `| undefined` has no wire representation: it must not leak a JSON null
        // into an `anyOf`, and optionality is already carried by `required`.
        assert_eq!(
            props.pointer("/maybe/$ref").and_then(|v| v.as_str()),
            Some("#/components/schemas/Kind"),
            "got {}",
            props.get("maybe").unwrap()
        );
    }

    #[test]
    fn constructor_parameter_properties_are_lowered() {
        let [pet] = lower_all(
            "",
            "export class Pet {\n\
               species = 'cat';\n\
               constructor(public name: string, private age?: number, readonly tag = 1, plain: boolean) {}\n\
             }",
            &["Pet"],
        )
        .try_into()
        .unwrap();
        assert_eq!(prop_names(&pet), ["species", "name", "age", "tag"]);
        assert_eq!(pet["properties"]["age"], json!({"type": "number"}));
        assert_eq!(pet["properties"]["tag"], json!({"type": "number"}));
        assert_eq!(required_names(&pet), ["species", "name", "tag"]);
    }

    #[test]
    fn a_union_with_undefined_makes_a_property_optional() {
        let [class, iface, alias] = lower_all(
            "",
            "export class Book { a: string | undefined; b: (number | void); c: string; }\n\
             export interface Author { a: string | undefined; c: string; }\n\
             export type Order = { a: undefined | string; c: string };",
            &["Book", "Author", "Order"],
        )
        .try_into()
        .unwrap();
        assert_eq!(required_names(&class), ["c"]);
        assert_eq!(class["properties"]["a"], json!({"type": "string"}));
        assert_eq!(required_names(&iface), ["c"]);
        assert_eq!(required_names(&alias), ["c"]);
    }

    #[test]
    fn serialization_decorators_hide_or_relax_properties() {
        let [user] = lower_all(
            "",
            "export class User {\n\
               @Exclude() password: string;\n\
               @ApiHideProperty() internal: string;\n\
               @Exclude({ toClassOnly: true }) computed: string;\n\
               @ApiPropertyOptional() nickname: string;\n\
               @ApiProperty({ required: false, description: 'd' }) bio: string;\n\
               @ApiProperty({ required: true }) email: string;\n\
               @Transformer.Exclude() token: string;\n\
             }",
            &["User"],
        )
        .try_into()
        .unwrap();
        assert_eq!(prop_names(&user), ["computed", "nickname", "bio", "email"]);
        assert_eq!(required_names(&user), ["computed", "email"]);
    }

    #[test]
    fn a_hidden_property_type_never_becomes_a_component() {
        let (project, dir) = project_from(&[(
            "dto.ts",
            "export class Secret { value: string; }\n\
             export class Internal { value: string; }\n\
             export class User {\n\
               @Exclude() secret: Secret;\n\
               /** @internal */ internal: Internal;\n\
               name: string;\n\
             }",
        )]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            components: &mut components,
            type_env: None,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let (decl_file, sym) = project.resolve(&dir.join("dto.ts"), "User").unwrap();
        let user = ctx.symbol_to_schema(&decl_file, &sym);
        assert_eq!(prop_names(&user), ["name"]);
        assert!(components.schemas.is_empty());
        assert!(components.allocated.is_empty());
    }

    #[test]
    fn untyped_fields_are_inferred_from_their_initialiser() {
        let [widget] = lower_all(
            "",
            "export class Widget {\n\
               a = 1; b = 'x'; c = true; d = -2; e = new Date(); f = `t`;\n\
               g = 0 as number; h = compute(); i;\n\
             }",
            &["Widget"],
        )
        .try_into()
        .unwrap();
        let p = &widget["properties"];
        assert_eq!(p["a"], json!({"type": "number"}));
        assert_eq!(p["b"], json!({"type": "string"}));
        assert_eq!(p["c"], json!({"type": "boolean"}));
        assert_eq!(p["d"], json!({"type": "number"}));
        assert_eq!(p["e"], json!({"type": "string", "format": "date-time"}));
        assert_eq!(p["f"], json!({"type": "string"}));
        assert_eq!(p["g"], json!({"type": "number"}));
        assert_eq!(p["h"], json!({"x-unresolved": "compute()"}));
        assert_eq!(p["i"], json!({}));
    }

    #[test]
    fn utility_types_edit_the_resolvable_part_of_an_unresolved_parent() {
        let [partial, pick, omit] = lower_all(
            "import { External } from '@acme/missing';\n\
             export class Mid extends External { title: string; }\n\
             export class Book extends Mid { pages: number; }",
            "import { Book } from './dto';\n\
             export type A = Partial<Book>;\n\
             export type B = Pick<Book, 'title'>;\n\
             export type C = Omit<Book, 'title'>;",
            &["A", "B", "C"],
        )
        .try_into()
        .unwrap();
        let unresolved = json!({"x-unresolved": "External"});
        assert_eq!(partial["allOf"][0], unresolved, "got {partial}");
        assert_eq!(prop_names(&partial["allOf"][1]), ["title", "pages"]);
        assert!(
            partial["allOf"][1].get("required").is_none(),
            "got {partial}"
        );
        assert_eq!(pick["allOf"][0], unresolved);
        assert_eq!(prop_names(&pick["allOf"][1]), ["title"]);
        assert_eq!(required_names(&pick["allOf"][1]), ["title"]);
        assert_eq!(prop_names(&omit["allOf"][1]), ["pages"]);
        assert_eq!(required_names(&omit["allOf"][1]), ["pages"]);
    }

    #[test]
    fn non_nullable_strips_null() {
        let [holder] = lower_all(
            "export type MaybeTitle = string | null;",
            "import { MaybeTitle } from './dto';\n\
             export interface Holder {\n\
               a: NonNullable<string | null>;\n\
               b: NonNullable<'x' | 'y' | null>;\n\
               c: NonNullable<Holder | null>;\n\
               d: NonNullable<MaybeTitle>;\n\
               e: NonNullable<string | number | null>;\n\
             }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let p = &holder["properties"];
        assert_eq!(p["a"], json!({"type": "string"}));
        assert_eq!(p["b"], json!({"type": "string", "enum": ["x", "y"]}));
        assert_eq!(p["c"], json!({"$ref": "#/components/schemas/Holder"}));
        assert_eq!(p["d"], json!({"type": "string"}));
        assert_eq!(
            p["e"],
            json!({"anyOf": [{"type": "string"}, {"type": "number"}]})
        );
    }

    #[test]
    fn record_with_literal_keys_lists_its_properties() {
        let [holder] = lower_all(
            "export type Slot = 'am' | 'pm';",
            "import { Slot } from './dto';\n\
             export interface Holder {\n\
               a: Record<'x' | 'y', number>;\n\
               b: Partial<Record<Slot, string>>;\n\
               c: Record<string, number>;\n\
             }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let p = &holder["properties"];
        assert_eq!(
            p["a"],
            json!({
                "type": "object",
                "properties": {"x": {"type": "number"}, "y": {"type": "number"}},
                "required": ["x", "y"]
            })
        );
        assert_eq!(
            p["b"],
            json!({
                "type": "object",
                "properties": {"am": {"type": "string"}, "pm": {"type": "string"}}
            })
        );
        assert_eq!(
            p["c"],
            json!({"type": "object", "additionalProperties": {"type": "number"}})
        );
    }

    #[test]
    fn tuple_rest_element_becomes_the_open_items() {
        let [holder] = lower_all(
            "",
            "export interface Holder {\n\
               a: [string, ...number[]];\n\
               b: [string, ...Array<boolean>];\n\
               c: [...string[], number];\n\
             }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let p = &holder["properties"];
        assert_eq!(
            p["a"],
            json!({"type": "array", "prefixItems": [{"type": "string"}], "items": {"type": "number"}})
        );
        assert_eq!(p["b"]["items"], json!({"type": "boolean"}));
        assert_eq!(
            p["c"],
            json!({"type": "array", "items": {}, "x-unresolved": "[...string[], number]"})
        );
    }

    #[test]
    fn whole_number_literal_types_are_integers() {
        let [holder] = lower_all(
            "",
            "export interface Holder { a: 42; b: -1; c: 1.5; d: 1 | 2; }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let p = &holder["properties"];
        assert_eq!(p["a"], json!({"type": "number", "enum": [42]}));
        assert_eq!(p["b"], json!({"type": "number", "enum": [-1]}));
        assert_eq!(p["c"], json!({"type": "number", "enum": [1.5]}));
        assert_eq!(p["d"], json!({"type": "number", "enum": [1, 2]}));
    }

    #[test]
    fn enum_members_computed_from_siblings_are_evaluated() {
        let [flags, aliases] = lower_all(
            "",
            "export enum Flags { None = 0, A = 1 << 0, B = 1 << 1, AB = A | B, C = Flags.AB + 1, D = ~0 }\n\
             export enum Aliases { First = 'first', Same = First, Next = 3, After }",
            &["Flags", "Aliases"],
        )
        .try_into()
        .unwrap();
        assert_eq!(flags["enum"], json!([0, 1, 2, 3, 4, -1]));
        assert_eq!(aliases["type"], json!(["string", "number"]));
        assert_eq!(aliases["enum"], json!(["first", "first", 3, 4]));
    }

    #[test]
    fn awaited_set_and_map_are_lowered() {
        let [holder] = lower_all(
            "",
            "export interface Holder {\n\
               a: Awaited<Promise<string>>;\n\
               b: Set<number>;\n\
               c: ReadonlySet<string>;\n\
               d: Map<string, boolean>;\n\
               e: ReadonlyMap<string, number>;\n\
             }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let p = &holder["properties"];
        assert_eq!(p["a"], json!({"type": "string"}));
        assert_eq!(
            p["b"],
            json!({"type": "array", "items": {"type": "number"}, "uniqueItems": true})
        );
        assert_eq!(p["c"]["uniqueItems"], json!(true));
        assert_eq!(
            p["d"],
            json!({"type": "object", "additionalProperties": {"type": "boolean"}})
        );
        assert_eq!(p["e"]["additionalProperties"], json!({"type": "number"}));
    }

    #[test]
    fn numeric_property_keys_are_kept() {
        let [iface, class, lit] = lower_all(
            "",
            "export interface Pair { 0: string; 1?: number; }\n\
             export class Grid { 0: string; }\n\
             export type Cell = { 2: boolean };",
            &["Pair", "Grid", "Cell"],
        )
        .try_into()
        .unwrap();
        assert_eq!(prop_names(&iface), ["0", "1"]);
        assert_eq!(required_names(&iface), ["0"]);
        assert_eq!(prop_names(&class), ["0"]);
        assert_eq!(prop_names(&lit), ["2"]);
    }

    #[test]
    fn tuple_rest_operands_resolve_through_aliases_and_spreads() {
        let [holder] = lower_all(
            "export type Tail = number[];",
            "import { Tail } from './dto';\n\
             export interface Holder {\n\
               a: [string, ...Tail];\n\
               b: [string, ...[number, boolean]];\n\
               c: [string, ...Missing];\n\
               d: [...number[]];\n\
               e: [];\n\
             }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let p = &holder["properties"];
        assert_eq!(p["a"]["items"], json!({"type": "number"}));
        assert_eq!(
            p["b"],
            json!({
                "type": "array",
                "prefixItems": [{"type": "string"}, {"type": "number"}, {"type": "boolean"}],
                "items": false
            })
        );
        assert_eq!(p["c"]["items"], json!({"x-unresolved": "Missing"}));
        assert_eq!(
            p["d"],
            json!({"type": "array", "items": {"type": "number"}})
        );
        assert_eq!(p["e"], json!({"type": "array", "items": {}, "maxItems": 0}));
    }

    #[test]
    fn spreading_an_empty_tuple_closes_the_tuple() {
        let [holder] = lower_all(
            "export type Nothing = [];",
            "import { Nothing } from './dto';
             export interface Holder { a: [string, ...[]]; b: [...[]]; c: [string, ...Nothing]; }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let p = &holder["properties"];
        let closed = json!({"type": "array", "prefixItems": [{"type": "string"}], "items": false});
        assert_eq!(p["a"], closed);
        assert_eq!(p["b"], json!({"type": "array", "items": {}, "maxItems": 0}));
        assert_eq!(p["c"], closed);
    }

    #[test]
    fn record_keys_are_deduplicated() {
        let [holder] = lower_all(
            "export type A = 'a' | 'b';\nexport type B = 'b' | 'c';",
            "import { A, B } from './dto';\n\
             export interface Holder { r: Record<A | B, number>; }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        assert_eq!(
            holder["properties"]["r"]["required"],
            json!(["a", "b", "c"])
        );
    }

    #[test]
    fn an_alias_admitting_undefined_makes_a_property_optional() {
        let [book, author] = lower_all(
            "export type Maybe = string | undefined;\n\
             export type Opt<T> = T | undefined;\n\
             export type Loop = Loop | string;",
            "import { Maybe, Opt, Loop } from './dto';\n\
             export interface Book { title: Maybe; pages: Opt<number>; loop: Loop; isbn: string; }\n\
             export class Author { name: Maybe; }",
            &["Book", "Author"],
        )
        .try_into()
        .unwrap();
        assert_eq!(required_names(&book), ["loop", "isbn"]);
        assert!(author.get("required").is_none(), "got {author}");
    }

    #[test]
    fn non_nullable_strips_null_inside_union_branches() {
        let [holder] = lower_all(
            "export type Maybe = string | null;",
            "import { Maybe } from './dto';\n\
             export interface Holder { r: NonNullable<Maybe | number>; }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        assert_eq!(
            holder["properties"]["r"],
            json!({"anyOf": [{"type": "string"}, {"type": "number"}]})
        );
    }

    #[test]
    fn a_hidden_override_hides_the_inherited_property() {
        let [user, base, plain] = lower_all(
            "",
            "export class Base { password = ''; name: string; }\n\
             export class User extends Base { @Exclude() password = ''; }\n\
             export class Plain extends Base { extra: string; }",
            &["User", "Base", "Plain"],
        )
        .try_into()
        .unwrap();
        assert_eq!(prop_names(&user["allOf"][0]), ["name"], "got {user}");
        assert_eq!(required_names(&user["allOf"][0]), ["name"]);
        assert_eq!(prop_names(&base), ["password", "name"]);
        assert_eq!(
            plain["allOf"][0],
            json!({"$ref": "#/components/schemas/Base"})
        );
    }

    #[test]
    fn non_finite_enum_members_are_unknown() {
        let [div, overflow] = lower_all(
            "",
            "export enum Div { A = 1 / 0 }\nexport enum Overflow { A = 10 ** 400, B = 0 / 0 }",
            &["Div", "Overflow"],
        )
        .try_into()
        .unwrap();
        assert_eq!(div, json!({"x-unresolved": "Div"}));
        assert_eq!(overflow, json!({"x-unresolved": "Overflow"}));
    }

    #[test]
    fn a_hidden_override_hides_a_property_beside_an_unresolved_ancestor() {
        let [user, base] = lower_all(
            "import { External } from '@acme/missing';\n\
             export class Base extends External { password: string; name: string; }",
            "import { Base } from './dto';\n\
             export class User extends Base { @Exclude() password: string; }\n\
             export { Base };",
            &["User", "Base"],
        )
        .try_into()
        .unwrap();
        let parent = &user["allOf"][0];
        assert_eq!(
            parent["allOf"][0],
            json!({"x-unresolved": "External"}),
            "got {user}"
        );
        assert_eq!(prop_names(&parent["allOf"][1]), ["name"]);
        assert_eq!(prop_names(&base["allOf"][1]), ["password", "name"]);
    }

    #[test]
    fn a_namespace_qualified_alias_admitting_undefined_is_optional() {
        let [book] = lower_with(
            &[("types.ts", "export type Maybe = string | undefined;")],
            "import * as Types from './types';\n\
             export interface Book { title: Types.Maybe; isbn: string; }",
            &["Book"],
        )
        .try_into()
        .unwrap();
        assert_eq!(required_names(&book), ["isbn"]);
    }

    #[test]
    fn an_optional_override_relaxes_the_inherited_property() {
        let [book, base] = lower_all(
            "",
            "export class Base { name: string; isbn: string; }\n\
             export class Book extends Base { @ApiPropertyOptional() name: string; }",
            &["Book", "Base"],
        )
        .try_into()
        .unwrap();
        assert_eq!(
            prop_names(&book["allOf"][0]),
            ["name", "isbn"],
            "got {book}"
        );
        assert_eq!(required_names(&book["allOf"][0]), ["isbn"]);
        assert_eq!(required_names(&base), ["name", "isbn"]);
    }

    #[test]
    fn a_rest_operand_that_is_not_an_array_stays_unresolved() {
        let [holder] = lower_all(
            "export type Tail = number[] | string[];",
            "import { Tail } from './dto';\n\
             export interface Holder { row: [boolean, ...Tail]; }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        assert_eq!(
            holder["properties"]["row"]["items"],
            json!({"x-unresolved": "Tail"})
        );
    }

    #[test]
    fn non_nullable_strips_null_inside_intersections() {
        let [holder] = lower_all(
            "",
            "export interface Holder {\n\
               a: NonNullable<(string | null) & ('x' | null)>;\n\
               b: NonNullable<string & null>;\n\
             }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let p = &holder["properties"];
        assert_eq!(
            p["a"],
            json!({"allOf": [{"type": "string"}, {"type": "string", "enum": ["x"]}]})
        );
        assert_eq!(p["b"], json!({"allOf": [{"type": "string"}, {"not": {}}]}));
    }

    #[test]
    fn a_record_value_admitting_undefined_leaves_its_keys_optional() {
        let [holder] = lower_all(
            "",
            "export interface Holder { r: Record<'a' | 'b', string | undefined>; }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let r = &holder["properties"]["r"];
        assert_eq!(prop_names(r), ["a", "b"]);
        assert!(r.get("required").is_none(), "got {r}");
    }

    #[test]
    fn a_bound_type_parameter_admitting_undefined_is_optional() {
        let (project, dir) = project_from(&[(
            "dto.ts",
            "export type Bound = string | undefined;\n\
             export interface Wrap<T> { first: T; count: number; }",
        )]);
        let file = dir.join("dto.ts");
        let (_, Symbol::TypeAlias(bound)) = project.resolve(&file, "Bound").unwrap() else {
            panic!("Bound is an alias");
        };
        let env: TypeEnv = HashMap::from([(
            "T".to_string(),
            TypeBinding {
                origin_file: file.clone(),
                ty: (*bound.type_ann).clone(),
                resolved: None,
                admits_undefined: None,
            },
        )]);
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            components: &mut components,
            type_env: Some(env),
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let (decl_file, sym) = project.resolve(&file, "Wrap").unwrap();
        let wrap = ctx.symbol_to_schema(&decl_file, &sym);
        assert_eq!(wrap["properties"]["first"], json!({"type": "string"}));
        assert_eq!(required_names(&wrap), ["count"]);
    }

    /// Lower `name` from `dto.ts` with the interceptor env `T -> Bound`, the
    /// way `interceptors.rs` does, returning the schema and the components.
    fn lower_under_env(dto: &str, name: &str) -> (Value, IndexMap<String, Value>) {
        let (project, dir) = project_from(&[("dto.ts", dto)]);
        let file = dir.join("dto.ts");
        let mut env = TypeEnv::new();
        if let Some((_, Symbol::TypeAlias(bound))) = project.resolve(&file, "Bound") {
            env.insert(
                "T".to_string(),
                TypeBinding {
                    origin_file: file.clone(),
                    ty: (*bound.type_ann).clone(),
                    resolved: None,
                    admits_undefined: None,
                },
            );
        }
        let mut components = ComponentSchemas::default();
        let mut ctx = SchemaCtx {
            project: &project,
            components: &mut components,
            type_env: Some(env),
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &[],
        };
        let (decl_file, sym) = project.resolve(&file, name).unwrap();
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        (schema, components.schemas)
    }

    #[test]
    fn a_nested_binding_keeps_the_callers_optionality() {
        let (outer, _) = lower_under_env(
            "export type Bound = string | undefined;\n\
             export interface Envelope<T> { value: T; tag: string; }\n\
             export interface Outer<T> { e: Envelope<T>; }",
            "Outer",
        );
        assert_eq!(
            required_names(&outer["properties"]["e"]),
            ["tag"],
            "got {outer}"
        );
    }

    #[test]
    fn instantiations_differing_only_in_optionality_are_distinct() {
        for order in [["a", "b"], ["b", "a"]] {
            let fields = order.map(|f| match f {
                "a" => "a: Box<string>;",
                _ => "b: Box<string | undefined>;",
            });
            let (out, schemas) = lower_under_env(
                &format!(
                    "export interface Box<T> {{ value: T; next?: Box<T>; }}\n\
                     export interface Out {{ {} {} }}",
                    fields[0], fields[1]
                ),
                "Out",
            );
            let component = |p: &str| {
                let name = out["properties"][p]["$ref"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap();
                schemas[name].clone()
            };
            assert_eq!(
                required_names(&component("a")),
                ["value"],
                "order {order:?}: {out}"
            );
            assert!(
                component("b").get("required").is_none(),
                "order {order:?}: {out}"
            );
        }
    }

    #[test]
    fn awaited_admitting_undefined_is_optional() {
        let [holder] = lower_all(
            "",
            "export interface Holder { value: Awaited<string | undefined>; id: string; }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        assert_eq!(required_names(&holder), ["id"]);
    }

    #[test]
    fn a_non_final_rest_tuple_keeps_an_items_schema() {
        let [holder] = lower_all(
            "",
            "export interface Holder { c: [...string[], number]; }",
            &["Holder"],
        )
        .try_into()
        .unwrap();
        let c = &holder["properties"]["c"];
        assert_eq!(
            *c,
            json!({"type": "array", "items": {}, "x-unresolved": "[...string[], number]"})
        );
        let mut down = c.clone();
        crate::dialect::downgrade_to_30(&mut down);
        assert_eq!(down["items"], json!({}));
    }
}
