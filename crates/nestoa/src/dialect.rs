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
//! | `anyOf: [<schema>, {type: "null"}]`      | `<schema>` — nullability is lost   |
//! | `type: "null"` on its own                | unconstrained — the type is lost   |
//! | `type: [A, B]` (multi-type)              | `anyOf: [{type: A}, {type: B}]`    |
//! | `const: v`                               | `enum: [v]`                        |
//! | `exclusiveMinimum: n`                    | `minimum: n` + `exclusiveMinimum: true` |
//! | `examples: [a, …]` on a schema           | `example: a`                       |
//!
//! The nullable-union and standalone-null rows are real losses, and
//! unavoidable rather than shortcuts:
//! `nullable` only means something beside a `type` keyword, and a union — which
//! is how a nullable `$ref`, `allOf` or `x-unresolved` is expressed — has no
//! `type` of its own to attach it to. Keeping `nullable` there would produce a
//! no-op that linters reject (Redocly's `nullable-type-sibling`), so the branch
//! is dropped instead. A 3.0.3 document simply cannot say "this `$ref` may be
//! null", and it has no null-only schema for a `null`-typed property either.
//!
//! The pass walks the whole document rather than the OpenAPI object model, so
//! it keeps working wherever a schema turns up — including one handed over
//! verbatim in the config. To make that safe it skips the keywords whose
//! values are instance data (see `is_instance_data`): an `example` that looks
//! like a schema is documented payload, not something to rewrite. Those
//! keywords only count in keyword position: inside a `properties` map the same
//! word is a field name, and a DTO with a field called `example` still needs
//! its schema downgraded (`is_name_keyed_map`).
//!
//! Tuples degrade rather than disappear. `[A, B]` lowers to `prefixItems` +
//! `items: false` in 3.1, and `[A, ...B[]]` to `prefixItems` + `items: B`;
//! 3.0.3 arrays are homogeneous, so the element schemas are merged into a
//! single `items` and a closed length becomes `maxItems`. Element *order* is
//! what gets lost.

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
            downgrade_const(map);
            downgrade_exclusive_bound(map, "exclusiveMinimum", "minimum");
            downgrade_exclusive_bound(map, "exclusiveMaximum", "maximum");
            downgrade_examples(map);
            for (key, value) in map.iter_mut() {
                if is_name_keyed_map(key) {
                    // `properties`, `paths`, `content`, … are keyed by names
                    // the user chose, so a key here means nothing about what
                    // its value is. Walk every value.
                    if let Value::Object(entries) = value {
                        entries.values_mut().for_each(downgrade_to_30);
                    }
                } else if !is_instance_data(key) {
                    downgrade_to_30(value);
                }
            }
        }
        _ => {}
    }
}

/// `type: [T, "null"]` -> `type: T` + `nullable: true`.
///
/// 3.0.3 allows only a single string `type`, so any array has to go. The
/// analyser itself only ever produces `[T, "null"]` (`make_nullable` widens one
/// concrete type), but a schema handed over verbatim in the config can carry a
/// genuine multi-type array; that becomes an `anyOf` of one-type branches, and
/// takes the same nullability loss as any other union.
fn downgrade_null_type(map: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = map.get("type") else {
        return;
    };
    let concrete: Vec<Value> = types
        .iter()
        .filter(|t| !is_null_type_name(t))
        .cloned()
        .collect();
    match concrete.as_slice() {
        // `["null"]` — 3.0.3 has no null-only schema, as for a scalar one.
        [] => {
            map.remove("type");
        }
        [single] => {
            let nullable = concrete.len() != types.len();
            map.insert("type".into(), single.clone());
            if nullable {
                map.insert("nullable".into(), Value::Bool(true));
            }
        }
        many => {
            // `anyOf`, not `oneOf`: the members can overlap (an integer is
            // also a number), and `oneOf` rejects a value matching more than
            // one branch. Nullability rides on the first branch, which has a
            // `type` for `nullable` to attach to, so it survives here.
            let nullable = many.len() != types.len();
            let mut branches: Vec<Value> = many.iter().map(|t| json!({ "type": t })).collect();
            if nullable && let Some(Value::Object(first)) = branches.first_mut() {
                first.insert("nullable".into(), Value::Bool(true));
            }
            map.remove("type");
            // A union is already there to constrain the value, and merging two
            // is not something a generated document should be inventing.
            if !map.contains_key("oneOf") && !map.contains_key("anyOf") {
                map.insert("anyOf".into(), Value::Array(branches));
            }
        }
    }
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
    // `enum: [null]` is a null-only schema, which 3.0.3 cannot express, and an
    // empty `enum` is invalid: drop the constraint, as for `type: "null"`.
    if kept.is_empty() {
        map.remove("enum");
        return;
    }
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
    let mut unwrapped = false;
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
                unwrapped = true;
            }
            [] => {
                map.remove(key);
            }
            _ => {
                map.insert(key.into(), Value::Array(kept));
            }
        }
    }
    unwrapped
}

/// Whether a keyword's value is instance data (a documented payload) or an
/// opaque extension, rather than a schema.
///
/// The walk must not descend into these. An `example` is what the API says it
/// returns; if one happens to look like a schema — an object with a `type`
/// key, say — rewriting it would change the documented payload. `enum` and
/// `const` values are read at the schema level but are likewise instance data
/// inside.
///
/// Only meaningful in keyword position: inside a `properties` map the same
/// word is a field name, which is what `is_name_keyed_map` exists to catch.
fn is_instance_data(key: &str) -> bool {
    matches!(key, "example" | "examples" | "default" | "enum" | "const") || key.starts_with("x-")
}

/// Whether a keyword's value is a map keyed by user-chosen names rather than
/// by schema keywords — so its keys carry no meaning for this pass and each
/// value has to be walked regardless of what it is called.
///
/// A DTO with a field named `example` or `default` is ordinary TypeScript, and
/// without this its schema would be skipped and keep its 3.1 constructs.
fn is_name_keyed_map(key: &str) -> bool {
    matches!(
        key,
        "properties"
            | "patternProperties"
            | "$defs"
            | "definitions"
            | "schemas"
            | "paths"
            | "responses"
            | "content"
            | "headers"
            | "encoding"
            | "callbacks"
            | "links"
            | "securitySchemes"
            | "variables"
    )
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

/// A `prefixItems` tuple -> a homogeneous array approximating it.
///
/// 3.0.3 arrays have one element schema, so the per-position schemas, and the
/// tail schema of an open tuple (`[A, ...B[]]` lowers to `items: B`), are
/// merged into a single `items`. Element order is what gets lost. A closed
/// tuple (`items: false`) caps the length with `maxItems`; an open one has no
/// upper bound.
///
/// Note what is *not* synthesized: `prefixItems` constrains the positions an
/// array happens to have, it does not require them, so a shorter array is
/// valid against the source. Adding `minItems` would reject values the 3.1
/// document accepts and leave the two dialects describing different
/// contracts. (That the 3.1 lowering is itself looser than the TypeScript
/// tuple it comes from is a `schema.rs` question, not one for the downgrade.)
fn downgrade_tuple(map: &mut Map<String, Value>) {
    let Some(Value::Array(prefix)) = map.remove("prefixItems") else {
        return;
    };
    let arity = prefix.len();
    let mut elements = prefix;
    let closed = match map.remove("items") {
        Some(Value::Bool(false)) => true,
        // Absent or `true`: every position past the prefix admits anything.
        None | Some(Value::Bool(true)) => {
            elements.push(json!({}));
            false
        }
        Some(tail) => {
            elements.push(tail);
            false
        }
    };

    // A position accepting anything (`true`, or `{}`) makes the whole
    // homogeneous approximation unconstrained — no single element schema
    // admits it while still narrowing the others.
    let constrains = |e: &Value| e.as_object().is_some_and(|m| !m.is_empty());
    if elements.iter().all(constrains) {
        let mut distinct: Vec<Value> = Vec::new();
        for element in elements {
            if !distinct.contains(&element) {
                distinct.push(element);
            }
        }
        match distinct.len() {
            0 => {
                map.insert("items".into(), json!({}));
            }
            1 => {
                map.insert("items".into(), distinct.into_iter().next().unwrap());
            }
            // `anyOf` for the same overlap reason as a multi-type array.
            _ => {
                map.insert("items".into(), json!({ "anyOf": distinct }));
            }
        }
    } else {
        // 3.0.3 requires `items` on an array.
        map.insert("items".into(), json!({}));
    }

    if !closed {
        return;
    }
    // A closed tuple caps the length. Intersect rather than overwrite, so an
    // existing tighter bound survives.
    let capped = match map.get("maxItems").and_then(Value::as_u64) {
        Some(existing) => existing.min(arity as u64),
        None => arity as u64,
    };
    map.insert("maxItems".into(), json!(capped));
}

/// A numeric `exclusiveMinimum: n` (2020-12) -> `minimum: n` +
/// `exclusiveMinimum: true` (3.0.3, where the keyword is a flag on the bound),
/// and likewise for the maximum. An inclusive bound already tighter than `n`
/// is kept on its own.
fn downgrade_exclusive_bound(map: &mut Map<String, Value>, exclusive: &str, bound: &str) {
    let Some(limit) = map.get(exclusive).filter(|v| v.is_number()).cloned() else {
        return;
    };
    let tighter = match map.get(bound).and_then(|b| compare_numbers(b, &limit)) {
        Some(order) if exclusive == "exclusiveMinimum" => order.is_gt(),
        Some(order) => order.is_lt(),
        None => false,
    };
    if tighter {
        map.remove(exclusive);
    } else {
        map.insert(bound.into(), limit);
        map.insert(exclusive.into(), Value::Bool(true));
    }
}

/// Order two JSON numbers, exactly whenever both are whole (an `f64` cannot
/// tell `2^54` from `2^54 + 2` apart, whichever way each is spelled).
fn compare_numbers(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (whole_number(a), whole_number(b)) {
        (Some(x), Some(y)) => Some(x.cmp(&y)),
        _ => a.as_f64()?.partial_cmp(&b.as_f64()?),
    }
}

/// A JSON number as an exact integer, when it is a whole one.
fn whole_number(v: &Value) -> Option<i128> {
    if let Some(n) = v.as_i64() {
        return Some(n.into());
    }
    if let Some(n) = v.as_u64() {
        return Some(n.into());
    }
    let f = v.as_f64()?;
    // Every whole `f64` below 2^127 converts to `i128` exactly.
    (f.fract() == 0.0 && f.abs() < 1.7e38).then_some(f as i128)
}

/// A schema's `examples: [a, …]` (2020-12) -> `example: a`. 3.0.3 schemas only
/// have the single `example`; an `examples` map (on a parameter or a media
/// type) is valid 3.0.3 and is left alone.
fn downgrade_examples(map: &mut Map<String, Value>) {
    if !map.get("examples").is_some_and(Value::is_array) {
        return;
    }
    let Some(Value::Array(examples)) = map.remove("examples") else {
        return;
    };
    if !map.contains_key("example")
        && let Some(first) = examples.into_iter().next()
    {
        map.insert("example".into(), first);
    }
}

/// `const: v` -> `enum: [v]`, the 3.0.3 way to pin a single value.
///
/// Skipping the walk into `const` protects the instance value; the keyword
/// itself still has to go, since 3.0.3 has no `const`. A `const` of `null` is
/// a null-only schema by another name and degrades the same way.
fn downgrade_const(map: &mut Map<String, Value>) {
    let Some(value) = map.remove("const") else {
        return;
    };
    if value.is_null() {
        return;
    }
    // `const` is the tighter constraint, so an existing `enum` intersects down
    // to it; an empty result means the source was already unsatisfiable.
    match map.get("enum") {
        Some(Value::Array(values)) => {
            let kept: Vec<Value> = values.iter().filter(|v| **v == value).cloned().collect();
            map.insert("enum".into(), Value::Array(kept));
        }
        _ => {
            map.insert("enum".into(), Value::Array(vec![value]));
        }
    }
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
            down(json!({"anyOf": [{"$ref": "#/components/schemas/A"}, {"type": "null"}]})),
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
                "anyOf": [{"$ref": "#/c/A"}, {"$ref": "#/c/B"}, {"type": "null"}],
                "description": "d"
            })),
            json!({"anyOf": [{"$ref": "#/c/A"}, {"$ref": "#/c/B"}], "description": "d"})
        );
    }

    #[test]
    fn keeps_a_sole_member_wrapped_when_the_union_has_siblings() {
        assert_eq!(
            down(json!({"anyOf": [{"$ref": "#/c/A"}, {"type": "null"}], "description": "d"})),
            json!({"anyOf": [{"$ref": "#/c/A"}], "description": "d"})
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
            json!({"type": "array", "items": {"type": "number"}, "maxItems": 2})
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
                "items": {"anyOf": [{"type": "string"}, {"type": "number"}]},
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
                "maxItems": 2
            })
        );
    }

    #[test]
    fn empty_tuple_becomes_a_zero_length_array() {
        assert_eq!(
            down(json!({"type": "array", "prefixItems": [], "items": false})),
            json!({"type": "array", "items": {}, "maxItems": 0})
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
            json!({"type": "array", "items": {"type": "number"}, "maxItems": 1})
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
    fn handles_a_null_branch_in_both_union_keywords() {
        assert_eq!(
            down(json!({
                "oneOf": [{"type": "string"}, {"type": "null"}],
                "anyOf": [{"type": "integer"}, {"type": "null"}]
            })),
            json!({"oneOf": [{"type": "string"}], "anyOf": [{"type": "integer"}]})
        );
    }

    #[test]
    fn never_rewrites_instance_data() {
        // `example`/`default`/`enum` values are documented payloads, not
        // schemas; a payload that happens to look like one must survive.
        let doc = json!({
            "type": "object",
            "example": {"type": "null", "range": {"prefixItems": [{"type": "number"}]}},
            "default": {"type": ["string", "null"]},
            "enum": [{"type": "null"}],
            "x-vendor": {"type": ["string", "null"]}
        });
        assert_eq!(down(doc.clone()), doc);
    }

    #[test]
    fn downgrades_a_property_whose_name_is_a_skipped_keyword() {
        // `properties` keys are field names, not keywords: a DTO with a field
        // called `example` or `default` still needs its schema downgraded.
        assert_eq!(
            down(json!({
                "type": "object",
                "properties": {
                    "example": {"type": ["string", "null"]},
                    "default": {"type": ["number", "null"]},
                    "x-status": {"type": ["boolean", "null"]}
                }
            })),
            json!({
                "type": "object",
                "properties": {
                    "example": {"type": "string", "nullable": true},
                    "default": {"type": "number", "nullable": true},
                    "x-status": {"type": "boolean", "nullable": true}
                }
            })
        );
    }

    #[test]
    fn splits_a_multi_type_array_into_a_union() {
        // `anyOf`, not `oneOf`: overlapping members such as integer/number
        // match more than one branch, which `oneOf` rejects.
        assert_eq!(
            down(json!({"type": ["integer", "number"]})),
            json!({"anyOf": [{"type": "integer"}, {"type": "number"}]})
        );
        // Nullability survives here — a branch with a plain `type` can carry
        // the flag, so it does not have to be dropped.
        assert_eq!(
            down(json!({"type": ["string", "number", "null"]})),
            json!({"anyOf": [{"type": "string", "nullable": true}, {"type": "number"}]})
        );
    }

    #[test]
    fn converts_const_to_a_single_value_enum() {
        assert_eq!(
            down(json!({"type": "string", "const": "ok"})),
            json!({"type": "string", "enum": ["ok"]})
        );
        // A const of null is a null-only schema by another name.
        assert_eq!(down(json!({"const": null})), json!({}));
        // The instance value itself is never walked.
        assert_eq!(
            down(json!({"const": {"type": "null"}})),
            json!({"enum": [{"type": "null"}]})
        );
    }

    #[test]
    fn merges_an_open_tuple_with_its_tail_and_leaves_it_uncapped() {
        assert_eq!(
            down(json!({
                "type": "array",
                "prefixItems": [{"type": "string"}],
                "items": {"type": "number"}
            })),
            json!({
                "type": "array",
                "items": {"anyOf": [{"type": "string"}, {"type": "number"}]}
            })
        );
        // A tail that admits anything leaves every element unconstrained.
        for tail in [json!(true), Value::Null] {
            let mut doc = json!({"type": "array", "prefixItems": [{"type": "string"}]});
            if !tail.is_null() {
                doc["items"] = tail;
            }
            assert_eq!(down(doc), json!({"type": "array", "items": {}}));
        }
    }

    #[test]
    fn rewrites_numeric_exclusive_bounds_as_boolean_flags() {
        assert_eq!(
            down(json!({"type": "number", "exclusiveMinimum": 0, "exclusiveMaximum": 10})),
            json!({
                "type": "number",
                "minimum": 0,
                "exclusiveMinimum": true,
                "maximum": 10,
                "exclusiveMaximum": true
            })
        );
        // An inclusive bound that is already tighter wins.
        assert_eq!(
            down(json!({"type": "number", "minimum": 5, "exclusiveMinimum": 0})),
            json!({"type": "number", "minimum": 5})
        );
        // Already 3.0.
        let flagged = json!({"type": "number", "minimum": 0, "exclusiveMinimum": true});
        assert_eq!(down(flagged.clone()), flagged);
    }

    #[test]
    fn keeps_the_first_of_a_schema_examples_array_as_its_example() {
        assert_eq!(
            down(json!({"type": "string", "examples": ["a", "b"]})),
            json!({"type": "string", "example": "a"})
        );
        assert_eq!(
            down(json!({"type": "string", "example": "x", "examples": ["a"]})),
            json!({"type": "string", "example": "x"})
        );
        // A media type's `examples` map is valid 3.0.3.
        let media = json!({"examples": {"one": {"value": 1}}});
        assert_eq!(down(media.clone()), media);
    }

    #[test]
    fn a_null_only_enum_becomes_unconstrained() {
        assert_eq!(
            down(json!({"enum": [null], "description": "d"})),
            json!({"description": "d"})
        );
    }

    #[test]
    fn keeps_explicit_bounds_and_intersects_the_cap() {
        // An existing `minItems` is the source's own constraint and survives;
        // `maxItems` is intersected with the closed length, never widened.
        assert_eq!(
            down(json!({
                "type": "array",
                "prefixItems": [{"type": "number"}, {"type": "number"}],
                "items": false,
                "minItems": 1,
                "maxItems": 5
            })),
            json!({
                "type": "array",
                "items": {"type": "number"},
                "minItems": 1,
                "maxItems": 2
            })
        );
        assert_eq!(
            down(json!({
                "type": "array",
                "prefixItems": [{"type": "number"}, {"type": "number"}],
                "items": false,
                "maxItems": 1
            }))["maxItems"],
            json!(1)
        );
    }

    #[test]
    fn an_unconstrained_position_leaves_the_elements_unconstrained() {
        // `true` at one position admits anything there, so no single `items`
        // schema can narrow the others without rejecting it.
        assert_eq!(
            down(json!({
                "type": "array",
                "prefixItems": [{"type": "string"}, true],
                "items": false
            })),
            json!({"type": "array", "items": {}, "maxItems": 2})
        );
    }

    #[test]
    fn intersects_const_with_an_existing_enum() {
        assert_eq!(
            down(json!({"type": "string", "const": "A", "enum": ["A", "B"]})),
            json!({"type": "string", "enum": ["A"]})
        );
    }

    #[test]
    fn a_null_only_type_array_becomes_unconstrained() {
        assert_eq!(
            down(json!({"type": ["null"], "description": "d"})),
            json!({"description": "d"})
        );
    }

    #[test]
    fn is_idempotent_on_an_already_downgraded_document() {
        let once = down(json!({
            "type": "object",
            "properties": {
                "a": {"type": ["string", "null"]},
                "b": {"anyOf": [{"$ref": "#/c/A"}, {"type": "null"}]},
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

    #[test]
    fn compares_large_integer_bounds_exactly() {
        assert_eq!(
            down(json!({
                "type": "integer",
                "minimum": 18014398509481986_i64,
                "exclusiveMinimum": 18014398509481984_i64
            })),
            json!({"type": "integer", "minimum": 18014398509481986_i64})
        );
        assert_eq!(
            down(json!({
                "type": "integer",
                "maximum": 18014398509481984_i64,
                "exclusiveMaximum": 18014398509481986_i64
            })),
            json!({"type": "integer", "maximum": 18014398509481984_i64})
        );
    }

    #[test]
    fn compares_mixed_integer_and_float_bounds_exactly() {
        let doc = json!({
            "type": "integer",
            "minimum": 18014398509481986_i64,
            "exclusiveMinimum": 18014398509481984.0
        });
        assert_eq!(
            down(doc),
            json!({"type": "integer", "minimum": 18014398509481986_i64})
        );
    }
}
