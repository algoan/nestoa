use std::collections::{HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use swc_ecma_ast::{
    Expr, Lit, TsArrayType, TsEntityName, TsKeywordTypeKind, TsLit, TsType, TsTypeElement,
    TsTypeLit, TsTypeRef, TsUnionOrIntersectionType,
};

use crate::jsdoc::{jsdoc_before, JsDoc};
use crate::symbols::{Project, Symbol};

/// Mutable container for the components/schemas section of the OpenAPI doc.
#[derive(Default)]
pub struct ComponentSchemas {
    pub schemas: IndexMap<String, Value>,
    seen: HashSet<String>,
}

impl ComponentSchemas {
    pub fn insert(&mut self, name: &str, schema: Value) {
        if !self.schemas.contains_key(name) {
            self.schemas.insert(name.to_string(), schema);
        }
    }
    pub fn mark_seen(&mut self, name: &str) -> bool {
        self.seen.insert(name.to_string())
    }
}

pub struct SchemaCtx<'a> {
    pub project: &'a Project,
    pub sources: &'a HashMap<std::path::PathBuf, String>,
    pub file_starts: &'a HashMap<std::path::PathBuf, swc_common::BytePos>,
    pub components: &'a mut ComponentSchemas,
}

impl<'a> SchemaCtx<'a> {
    pub fn ts_type_to_schema(&mut self, file: &Path, ty: &TsType) -> Value {
        match ty {
            TsType::TsKeywordType(k) => keyword_schema(k.kind),
            TsType::TsLitType(l) => match &l.lit {
                TsLit::Str(s) => json!({ "type": "string", "enum": [s.value.to_string_lossy().into_owned()] }),
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
                let non_null: Vec<Value> = parts
                    .into_iter()
                    .filter(|p| p.get("type").and_then(|t| t.as_str()) != Some("null"))
                    .collect();
                let mut schema = if non_null.len() == 1 {
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
                    if let Value::Object(map) = &mut schema {
                        map.insert("nullable".into(), Value::Bool(true));
                    }
                }
                schema
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
                        swc_ecma_ast::TsEnumMemberId::Str(s) => s.value.to_string_lossy().into_owned(),
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
            if let Some((decl_file, sym)) =
                self.project.resolve_namespace_member(file, &parent, &member)
            {
                let schema_name = sanitize_name(&member);
                if self.components.mark_seen(&schema_name) {
                    let resolved = self.symbol_to_schema(&decl_file, &sym);
                    self.components.insert(&schema_name, resolved);
                }
                return json!({ "$ref": format!("#/components/schemas/{}", schema_name) });
            }
            // Unknown — fall through and return placeholder.
            return json!({ "x-unresolved": format!("{}.{}", parent, member) });
        }
        let name = entity_name(&r.type_name);
        // Built-in generic handling
        match name.as_str() {
            "Promise" | "Observable" => {
                if let Some(args) = &r.type_params {
                    if let Some(first) = args.params.first() {
                        return self.ts_type_to_schema(file, first);
                    }
                }
                return json!({});
            }
            "Array" | "ReadonlyArray" => {
                if let Some(args) = &r.type_params {
                    if let Some(first) = args.params.first() {
                        return json!({ "type": "array", "items": self.ts_type_to_schema(file, first) });
                    }
                }
                return json!({ "type": "array" });
            }
            "Partial" | "Required" | "Readonly" | "NonNullable" => {
                if let Some(args) = &r.type_params {
                    if let Some(first) = args.params.first() {
                        return self.ts_type_to_schema(file, first);
                    }
                }
            }
            "Omit" | "Pick" => {
                if let Some(args) = &r.type_params {
                    if let Some(first) = args.params.first() {
                        let mut base = self.ts_type_to_schema(file, first);
                        // If base ended up as a $ref, inline a clone of the referenced
                        // schema so we can mutate its property set.
                        if let Some(refname) = base
                            .get("$ref")
                            .and_then(|v| v.as_str())
                            .and_then(|r| r.strip_prefix("#/components/schemas/"))
                            .map(|s| s.to_string())
                        {
                            if let Some(inlined) = self.components.schemas.get(&refname).cloned() {
                                base = inlined;
                            }
                        }
                        if name == "Omit" {
                            if let (Some(second), Value::Object(obj)) =
                                (args.params.get(1), &mut base)
                            {
                                let to_remove = collect_string_keys(second);
                                if let Some(Value::Object(props)) = obj.get_mut("properties") {
                                    for k in &to_remove {
                                        props.remove(k);
                                    }
                                }
                                if let Some(Value::Array(req)) = obj.get_mut("required") {
                                    req.retain(|v| match v.as_str() {
                                        Some(s) => !to_remove.iter().any(|k| k == s),
                                        None => true,
                                    });
                                }
                            }
                        } else if name == "Pick" {
                            if let (Some(second), Value::Object(obj)) =
                                (args.params.get(1), &mut base)
                            {
                                let keep = collect_string_keys(second);
                                if let Some(Value::Object(props)) = obj.get_mut("properties") {
                                    props.retain(|k, _| keep.iter().any(|x| x == k));
                                }
                                if let Some(Value::Array(req)) = obj.get_mut("required") {
                                    req.retain(|v| match v.as_str() {
                                        Some(s) => keep.iter().any(|k| k == s),
                                        None => true,
                                    });
                                }
                            }
                        }
                        return base;
                    }
                }
            }
            "Record" => {
                if let Some(args) = &r.type_params {
                    if let Some(value_t) = args.params.get(1) {
                        return json!({
                            "type": "object",
                            "additionalProperties": self.ts_type_to_schema(file, value_t),
                        });
                    }
                }
                return json!({ "type": "object" });
            }
            "Date" => return json!({ "type": "string", "format": "date-time" }),
            "ObjectId" | "Buffer" => return json!({ "type": "string" }),
            _ => {}
        }

        // Resolve to a project symbol.
        if let Some((decl_file, sym)) = self.project.resolve(file, &name) {
            let schema_name = sanitize_name(&name);
            if self.components.mark_seen(&schema_name) {
                let resolved = self.symbol_to_schema(&decl_file, &sym);
                self.components.insert(&schema_name, resolved);
            }
            return json!({ "$ref": format!("#/components/schemas/{}", schema_name) });
        }
        // Unknown — leave a permissive placeholder annotated with the name.
        json!({ "x-unresolved": name })
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
                        let mut schema = p
                            .type_ann
                            .as_ref()
                            .map(|t| self.ts_type_to_schema(file, &t.type_ann))
                            .unwrap_or_else(|| json!({}));
                        if let (Some(src), Some(file_start)) =
                            (self.sources.get(file), self.file_starts.get(file))
                        {
                            if let Some(doc) = jsdoc_before(src, *file_start, p.span) {
                                attach_doc(&mut schema, &doc);
                            }
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
                {
                    if let Some(doc) = jsdoc_before(src, *file_start, i.span) {
                        attach_doc_to_map(&mut object, &doc);
                    }
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
                    Value::String(if is_string { "string".into() } else { "number".into() }),
                );
                obj.insert("enum".into(), Value::Array(values));
                if let (Some(src), Some(file_start)) =
                    (self.sources.get(file), self.file_starts.get(file))
                {
                    if let Some(doc) = jsdoc_before(src, *file_start, e.span) {
                        attach_doc_to_map(&mut obj, &doc);
                    }
                }
                Value::Object(obj)
            }
            Symbol::Class(c) => {
                let mut props: IndexMap<String, Value> = IndexMap::new();
                let mut required: Vec<String> = Vec::new();
                // A `class X extends Y` carries Y's members too. Mirror the
                // interface-`extends` handling: emit Y as an allOf branch.
                let mut all_of: Vec<Value> = Vec::new();
                if let Some(super_expr) = &c.class.super_class {
                    if let Expr::Ident(id) = &**super_expr {
                        let dummy_ref = TsTypeRef {
                            span: c.class.span,
                            type_name: TsEntityName::Ident(id.clone()),
                            type_params: c.class.super_type_params.clone(),
                        };
                        all_of.push(self.type_ref_schema(file, &dummy_ref));
                    }
                }
                for member in &c.class.body {
                    if let swc_ecma_ast::ClassMember::ClassProp(p) = member {
                        let name = match &p.key {
                            swc_ecma_ast::PropName::Ident(i) => i.sym.to_string(),
                            swc_ecma_ast::PropName::Str(s) => s.value.to_string_lossy().into_owned(),
                            _ => continue,
                        };
                        let mut schema = p
                            .type_ann
                            .as_ref()
                            .map(|t| self.ts_type_to_schema(file, &t.type_ann))
                            .unwrap_or_else(|| json!({}));
                        if let (Some(src), Some(file_start)) =
                            (self.sources.get(file), self.file_starts.get(file))
                        {
                            if let Some(doc) = jsdoc_before(src, *file_start, p.span) {
                                attach_doc(&mut schema, &doc);
                            }
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
                {
                    if let Some(doc) = jsdoc_before(src, *file_start, c.class.span) {
                        attach_doc_to_map(&mut obj, &doc);
                    }
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
            TsType::TsUnionOrIntersectionType(
                TsUnionOrIntersectionType::TsUnionType(u),
            ) => {
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

pub fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
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

fn attach_doc(schema: &mut Value, doc: &JsDoc) {
    if let Value::Object(m) = schema {
        attach_doc_to_map(m, doc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParsingContext;
    use std::collections::HashMap;

    fn project_from(files: &[(&str, &str)]) -> (Project, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("nestoa-schema-{}", std::process::id()));
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
        };
        let derived_file = dir.join("derived.ts");
        let (decl_file, sym) = project.resolve(&derived_file, "Derived").expect("resolve Derived");
        let schema = ctx.symbol_to_schema(&decl_file, &sym);
        let all_of = schema.get("allOf").and_then(|v| v.as_array()).expect("allOf");
        assert_eq!(
            all_of[0].get("$ref").and_then(|v| v.as_str()),
            Some("#/components/schemas/Base")
        );
        let own = all_of.last().unwrap();
        assert!(own
            .get("properties")
            .and_then(|p| p.get("b"))
            .is_some());
        // The base class is collected into components for the $ref to point at.
        assert!(ctx.components.schemas.contains_key("Base"));
    }
}

fn attach_doc_to_map(m: &mut Map<String, Value>, doc: &JsDoc) {
    let combined = match (&doc.summary, &doc.description) {
        (Some(s), Some(d)) => Some(format!("{s}\n\n{d}")),
        (Some(s), None) => Some(s.clone()),
        (None, Some(d)) => Some(d.clone()),
        (None, None) => None,
    };
    if let Some(c) = combined {
        if !m.contains_key("description") {
            m.insert("description".into(), Value::String(c));
        }
    }
    if doc.deprecated {
        m.insert("deprecated".into(), Value::Bool(true));
    }
    if let Some(ex) = &doc.example {
        m.insert("example".into(), Value::String(ex.clone()));
    }
}
