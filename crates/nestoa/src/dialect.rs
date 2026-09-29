//! Downgrade an emitted document from OpenAPI 3.1 to 3.0.3.
//!
//! The analyser lowers TypeScript into JSON Schema 2020-12, which is the
//! dialect 3.1 uses. Two of the constructs it produces have no 3.0.3 spelling:
//! `"null"` as a type, and tuples. Both are rewritten here, as one pass over
//! the finished document, so the rest of the pipeline never has to know which
//! version it is generating for. Running last also covers schemas nestoa did
//! not lower itself — an `interceptors[].response.schema` supplied verbatim in
//! the config lands in the output like any other.
//!
//! Nullability is the reason a single document cannot satisfy both versions.
//! 3.1 has no `nullable` keyword (a 3.1 consumer ignores it, so the field
//! reads as non-nullable) and 3.0.3 has no `"null"` type, so `T | null` has to
//! be spelled differently in each. What the downgrade can preserve:
//!
//! | 3.1                                      | 3.0.3                              |
//! |------------------------------------------|------------------------------------|
//! | `type: [T, "null"]`                      | `type: T` + `nullable: true`       |
//! | `enum: [a, b, null]`                     | `enum: [a, b]` + `nullable: true`  |
//! | `oneOf: [<schema>, {type: "null"}]`      | `<schema>` — nullability is lost   |
//! | `type: "null"` on its own                | unconstrained — the type is lost   |
//!
//! That last row is a real loss, and it is unavoidable rather than a shortcut:
//! `nullable` only means something beside a `type` keyword, and a union — which
//! is how a nullable `$ref`, `allOf` or `x-unresolved` is expressed — has no
//! `type` of its own to attach it to. Keeping `nullable` there would produce a
//! no-op that linters reject (Redocly's `nullable-type-sibling`), so the branch
//! is dropped instead. A 3.0.3 document simply cannot say "this `$ref` may be
//! null".
//!
//! Tuples degrade rather than disappear. `[A, B]` lowers to `prefixItems` +
//! `items: false` in 3.1; 3.0.3 arrays are homogeneous, so the element schemas
//! are merged into a single `items` and the fixed length is preserved as
//! `minItems`/`maxItems`. Element *order* is what gets lost.

use serde_json::{Map, Value, json};

/// Rewrite every 3.1-only construct in `doc` into its closest 3.0.3
/// equivalent, in place. Safe to call on an already-3.0.3 document: each
/// rewrite is keyed on a shape only the 3.1 dialect produces.
pub fn downgrade_to_30(doc: &mut Value) {
    match doc {
        Value::Array(items) => items.iter_mut().for_each(downgrade_to_30),
        Value::Object(map) => {
            // Unwrapping a union folds its lone member's keys into this object,
            // and that member can itself carry a 3.1 construct. The recursion
            // below only descends into values, so those merged keys would never
            // be revisited — settle the unwrapping first, then rewrite.
            while unwrap_null_union(map) {}
            downgrade_tuple(map);
            downgrade_null_type(map);
            downgrade_null_enum(map);
            downgrade_standalone_null(map);
            map.values_mut().for_each(downgrade_to_30);
        }
        _ => {}
    }
}

/// `type: [T, "null"]` -> `type: T` + `nullable: true`.
///
/// A multi-type array is left alone: 3.0.3 has no way to express one, and the
/// analyser never emits one — `make_nullable` only ever widens a single type.
fn downgrade_null_type(map: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = map.get("type") else {
        return;
    };
    if !types.iter().any(is_null_type_name) {
        return;
    }
    let concrete: Vec<Value> = types
        .iter()
        .filter(|t| !is_null_type_name(t))
        .cloned()
        .collect();
    let [single] = concrete.as_slice() else {
        return;
    };
    map.insert("type".into(), single.clone());
    map.insert("nullable".into(), Value::Bool(true));
}

/// `enum: [a, b, null]` -> `enum: [a, b]` + `nullable: true`.
///
/// `nullable` is only added beside a `type`; without one it would be the same
/// no-op the union case avoids.
fn downgrade_null_enum(map: &mut Map<String, Value>) {
    let Some(Value::Array(values)) = map.get("enum") else {
        return;
    };
    if !values.iter().any(Value::is_null) {
        return;
    }
    let kept: Vec<Value> = values.iter().filter(|v| !v.is_null()).cloned().collect();
    map.insert("enum".into(), Value::Array(kept));
    if map.contains_key("type") {
        map.insert("nullable".into(), Value::Bool(true));
    }
}

/// Drop a `{type: "null"}` branch from a `oneOf`/`anyOf`, unwrapping the union
/// when it was only there to carry that branch.
///
/// Returns whether a lone member was folded up into `map`, which leaves keys
/// the caller has not rewritten yet (and possibly another union to unwrap).
fn unwrap_null_union(map: &mut Map<String, Value>) -> bool {
    for key in ["oneOf", "anyOf"] {
        let Some(Value::Array(branches)) = map.get(key) else {
            continue;
        };
        if !branches.iter().any(is_null_schema) {
            continue;
        }
        let kept: Vec<Value> = branches
            .iter()
            .filter(|b| !is_null_schema(b))
            .cloned()
            .collect();
        match kept.as_slice() {
            // `make_nullable` wraps a `$ref`/`allOf`/`x-unresolved` in a
            // two-branch union purely to hang the null branch off it; with that
            // branch gone the wrapper has no purpose, so fold the lone member
            // back up. Only when the union is the whole schema — a sibling
            // `description` alongside it would have to be merged, and a
            // one-member union is valid 3.0.3 anyway.
            [Value::Object(sole)] if map.len() == 1 => {
                let sole = sole.clone();
                map.remove(key);
                map.extend(sole);
                return true;
            }
            [] => {
                map.remove(key);
            }
            _ => {
                map.insert(key.into(), Value::Array(kept));
            }
        }
        return false;
    }
    false
}

/// `{type: "null"}` on its own -> an unconstrained schema.
///
/// A property declared `null` in TypeScript lowers to this (`keyword_schema`'s
/// `TsNullKeyword` arm). 3.0.3 has no null-only schema, so the constraint is
/// dropped rather than left as invalid syntax; any annotations beside it
/// (`description`, `deprecated`, …) are kept.
fn downgrade_standalone_null(map: &mut Map<String, Value>) {
    if map.get("type").is_some_and(is_null_type_name) {
        map.remove("type");
    }
}

/// `{type: array, prefixItems: [A, B], items: false}` -> a homogeneous array of
/// the merged element schema, with the tuple's arity kept as min/maxItems.
fn downgrade_tuple(map: &mut Map<String, Value>) {
    let Some(Value::Array(prefix)) = map.remove("prefixItems") else {
        return;
    };
    let arity = prefix.len();
    let mut distinct: Vec<Value> = Vec::new();
    for element in prefix {
        if !distinct.contains(&element) {
            distinct.push(element);
        }
    }
    match distinct.len() {
        0 => {
            map.remove("items");
        }
        1 => {
            map.insert("items".into(), distinct.into_iter().next().unwrap());
        }
        _ => {
            map.insert("items".into(), json!({ "oneOf": distinct }));
        }
    }
    map.insert("minItems".into(), json!(arity));
    map.insert("maxItems".into(), json!(arity));
}

fn is_null_type_name(value: &Value) -> bool {
    value.as_str() == Some("null")
}

fn is_null_schema(value: &Value) -> bool {
    value.get("type").is_some_and(is_null_type_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(mut v: Value) -> Value {
        downgrade_to_30(&mut v);
        v
    }

    #[test]
    fn widens_nullable_type_into_nullable_keyword() {
        assert_eq!(
            down(json!({"type": ["string", "null"], "description": "d"})),
            json!({"type": "string", "description": "d", "nullable": true})
        );
    }

    #[test]
    fn rewrites_nullable_types_nested_in_properties_and_items() {
        assert_eq!(
            down(json!({
                "type": "object",
                "properties": {
                    "a": {"type": ["number", "null"]},
                    "b": {"type": "array", "items": {"type": ["string", "null"]}}
                }
            })),
            json!({
                "type": "object",
                "properties": {
                    "a": {"type": "number", "nullable": true},
                    "b": {"type": "array", "items": {"type": "string", "nullable": true}}
                }
            })
        );
    }

    #[test]
    fn moves_enum_null_onto_the_nullable_keyword() {
        assert_eq!(
            down(json!({"type": ["string", "null"], "enum": ["a", "b", null]})),
            json!({"type": "string", "enum": ["a", "b"], "nullable": true})
        );
    }

    #[test]
    fn leaves_nullable_off_a_typeless_enum() {
        assert_eq!(down(json!({"enum": ["a", null]})), json!({"enum": ["a"]}));
    }

    #[test]
    fn unwraps_a_union_that_only_wrapped_a_null_branch() {
        assert_eq!(
            down(json!({"oneOf": [{"$ref": "#/components/schemas/A"}, {"type": "null"}]})),
            json!({"$ref": "#/components/schemas/A"})
        );
        assert_eq!(
            down(json!({"anyOf": [{"x-unresolved": "Foo"}, {"type": "null"}]})),
            json!({"x-unresolved": "Foo"})
        );
    }

    #[test]
    fn keeps_a_real_union_and_only_drops_its_null_branch() {
        assert_eq!(
            down(json!({
                "oneOf": [{"$ref": "#/c/A"}, {"$ref": "#/c/B"}, {"type": "null"}],
                "description": "d"
            })),
            json!({"oneOf": [{"$ref": "#/c/A"}, {"$ref": "#/c/B"}], "description": "d"})
        );
    }

    #[test]
    fn keeps_a_sole_member_wrapped_when_the_union_has_siblings() {
        assert_eq!(
            down(json!({"oneOf": [{"$ref": "#/c/A"}, {"type": "null"}], "description": "d"})),
            json!({"oneOf": [{"$ref": "#/c/A"}], "description": "d"})
        );
    }

    #[test]
    fn merges_a_tuple_into_a_homogeneous_array_keeping_its_arity() {
        assert_eq!(
            down(json!({
                "type": "array",
                "prefixItems": [{"type": "number"}, {"type": "number"}],
                "items": false
            })),
            json!({"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2})
        );
    }

    #[test]
    fn merges_a_mixed_tuple_into_a_union_of_its_element_types() {
        assert_eq!(
            down(json!({
                "type": "array",
                "prefixItems": [{"type": "string"}, {"type": "number"}],
                "items": false
            })),
            json!({
                "type": "array",
                "items": {"oneOf": [{"type": "string"}, {"type": "number"}]},
                "minItems": 2,
                "maxItems": 2
            })
        );
    }

    #[test]
    fn downgrades_a_tuple_of_nullable_elements() {
        assert_eq!(
            down(json!({
                "type": "array",
                "prefixItems": [{"type": ["string", "null"]}, {"type": ["string", "null"]}],
                "items": false
            })),
            json!({
                "type": "array",
                "items": {"type": "string", "nullable": true},
                "minItems": 2,
                "maxItems": 2
            })
        );
    }

    #[test]
    fn empty_tuple_becomes_a_zero_length_array() {
        assert_eq!(
            down(json!({"type": "array", "prefixItems": [], "items": false})),
            json!({"type": "array", "minItems": 0, "maxItems": 0})
        );
    }

    #[test]
    fn rewrites_the_lone_branch_a_union_unwrap_folds_up() {
        assert_eq!(
            down(json!({"oneOf": [{"type": ["string", "null"]}, {"type": "null"}]})),
            json!({"type": "string", "nullable": true})
        );
        assert_eq!(
            down(json!({
                "oneOf": [
                    {"type": "array", "prefixItems": [{"type": "number"}], "items": false},
                    {"type": "null"}
                ]
            })),
            json!({"type": "array", "items": {"type": "number"}, "minItems": 1, "maxItems": 1})
        );
    }

    #[test]
    fn degrades_a_standalone_null_type_to_an_unconstrained_schema() {
        assert_eq!(
            down(json!({"type": "null", "description": "always null"})),
            json!({"description": "always null"})
        );
    }

    #[test]
    fn leaves_a_multi_type_array_alone() {
        // 3.0.3 cannot express it and the analyser never emits one; rewriting
        // it would mean inventing a meaning.
        assert_eq!(
            down(json!({"type": ["string", "number", "null"]})),
            json!({"type": ["string", "number", "null"]})
        );
    }

    #[test]
    fn is_idempotent_on_an_already_downgraded_document() {
        let once = down(json!({
            "type": "object",
            "properties": {
                "a": {"type": ["string", "null"]},
                "b": {"oneOf": [{"$ref": "#/c/A"}, {"type": "null"}]},
                "c": {"type": "array", "prefixItems": [{"type": "number"}], "items": false}
            }
        }));
        assert_eq!(down(once.clone()), once);
    }

    #[test]
    fn leaves_a_3_0_document_untouched() {
        let doc = json!({
            "openapi": "3.0.3",
            "paths": {"/a": {"get": {"responses": {"200": {"description": "OK"}}}}},
            "components": {"schemas": {"A": {"type": "string", "nullable": true}}}
        });
        assert_eq!(down(doc.clone()), doc);
    }
}
