//! Warnings reported to the user, recorded so `--strict` can fail on any.

use std::cell::RefCell;
use std::fmt::Display;

use serde_json::Value;

thread_local! {
    static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub fn warn(message: impl Display) {
    let message = message.to_string();
    eprintln!("warning: {message}");
    WARNINGS.with_borrow_mut(|w| w.push(message));
}

pub fn warning_count() -> usize {
    WARNINGS.with_borrow(Vec::len)
}

/// The warnings recorded so far on this thread, clearing them.
#[cfg(test)]
pub fn take_warnings() -> Vec<String> {
    WARNINGS.take()
}

/// Every `x-unresolved` marker in `doc`, as `(JSON pointer, unresolved type)`.
pub fn unresolved(doc: &Value) -> Vec<(String, String)> {
    fn walk(v: &Value, pointer: &mut String, out: &mut Vec<(String, String)>) {
        let len = pointer.len();
        match v {
            Value::Object(map) => {
                if let Some(name) = map.get("x-unresolved") {
                    let name = name
                        .as_str()
                        .map_or_else(|| name.to_string(), str::to_string);
                    out.push((format!("#{pointer}"), name));
                }
                for (key, child) in map {
                    pointer.push('/');
                    pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                    walk(child, pointer, out);
                    pointer.truncate(len);
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    pointer.push_str(&format!("/{i}"));
                    walk(child, pointer, out);
                    pointer.truncate(len);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(doc, &mut String::new(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unresolved_markers_are_located_by_json_pointer() {
        let doc = json!({
            "paths": {"/items/{id}": {"get": {"responses": {"200": {"content": {
                "application/json": {"schema": {"x-unresolved": "Acc"}}
            }}}}}},
            "components": {"schemas": {"Page": {"properties": {"items": {
                "type": "array", "items": {"anyOf": [{"x-unresolved": "T"}, {"type": "null"}]}
            }}}}}
        });
        assert_eq!(
            unresolved(&doc),
            [
                (
                    "#/paths/~1items~1{id}/get/responses/200/content/application~1json/schema"
                        .to_string(),
                    "Acc".to_string()
                ),
                (
                    "#/components/schemas/Page/properties/items/items/anyOf/0".to_string(),
                    "T".to_string()
                ),
            ]
        );
    }
}
