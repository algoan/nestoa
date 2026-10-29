//! Assemble the AsyncAPI 3.0 document of the message events `events.rs`
//! extracts.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::config::{AsyncApi, Config};
use crate::dialect;
use crate::events::{Action, EventOperation};
use crate::routes::reachable_schemas;
use crate::schema::ComponentSchemas;

pub const VERSION: &str = "3.0.0";

/// One operation per handler, action and channel, listing every distinct
/// payload the handler sends or receives there. Channels, operations and
/// messages each get an id no other one takes, so two operations on one
/// channel never overwrite each other.
pub fn to_asyncapi(
    config: &Config,
    asyncapi: &AsyncApi,
    events: &[EventOperation],
    mut components: ComponentSchemas,
) -> Value {
    components.resolve_deferred();

    let mut grouped: BTreeMap<(String, Action, String), Vec<&EventOperation>> = BTreeMap::new();
    for e in events {
        grouped
            .entry((e.handler.clone(), e.action, e.channel.clone()))
            .or_default()
            .push(e);
    }

    let addresses: std::collections::BTreeSet<&str> =
        events.iter().map(|e| e.channel.as_str()).collect();
    let mut channel_ids: BTreeMap<&str, String> = BTreeMap::new();
    let mut taken = std::collections::HashSet::new();
    for address in addresses {
        channel_ids.insert(address, unique(&mut taken, &key_name(address)));
    }

    // Channel id -> message id -> payload, in the order operations use them.
    let mut messages: BTreeMap<String, IndexMap<String, Value>> = BTreeMap::new();
    let mut operations = Map::new();
    let mut operation_ids = std::collections::HashSet::new();
    for ((handler, action, address), ops) in &grouped {
        let channel_id = &channel_ids[address.as_str()];
        let channel_messages = messages.entry(channel_id.clone()).or_default();
        let mut refs: Vec<Value> = Vec::new();
        for op in ops {
            let id = match channel_messages.iter().find(|(_, p)| **p == op.payload) {
                Some((id, _)) => id.clone(),
                None => {
                    let base = message_name(&op.payload).unwrap_or_else(|| channel_id.clone());
                    let mut id = base.clone();
                    let mut n = 1;
                    while channel_messages.contains_key(&id) {
                        n += 1;
                        id = format!("{base}_{n}");
                    }
                    channel_messages.insert(id.clone(), op.payload.clone());
                    id
                }
            };
            let r = json!({ "$ref": format!("#/channels/{channel_id}/messages/{id}") });
            if !refs.contains(&r) {
                refs.push(r);
            }
        }
        let first = ops[0];
        let mut op = Map::new();
        op.insert("action".into(), json!(action.as_str()));
        op.insert(
            "channel".into(),
            json!({ "$ref": format!("#/channels/{channel_id}") }),
        );
        if let Some(s) = &first.summary {
            op.insert("summary".into(), json!(s));
        }
        if let Some(d) = &first.description {
            op.insert("description".into(), json!(d));
        }
        op.insert("tags".into(), json!([{ "name": first.tag }]));
        op.insert("messages".into(), Value::Array(refs));
        let id = unique(&mut operation_ids, &key_name(&handler.replace('.', "_")));
        operations.insert(id, Value::Object(op));
    }

    let mut channels = Map::new();
    for (address, id) in &channel_ids {
        let mut channel_messages = Map::new();
        for (name, payload) in messages.remove(id).unwrap_or_default() {
            let mut message = Map::new();
            message.insert("name".into(), json!(name));
            if !payload.is_null() {
                let mut payload = payload;
                dialect::to_draft07(&mut payload);
                message.insert("payload".into(), payload);
            }
            channel_messages.insert(name, Value::Object(message));
        }
        channels.insert(
            id.clone(),
            json!({ "address": address, "messages": channel_messages }),
        );
    }

    let reachable = reachable_schemas(&channels, &components.schemas);
    let mut schemas: Vec<(String, Value)> = components
        .schemas
        .into_iter()
        .filter(|(name, _)| reachable.contains(name))
        .collect();
    schemas.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, schema) in &mut schemas {
        dialect::to_draft07(schema);
    }

    let info = asyncapi.info.as_ref().unwrap_or(&config.info);
    let mut info_json = Map::new();
    info_json.insert("title".into(), json!(info.title));
    info_json.insert("version".into(), json!(info.version));
    if let Some(d) = &info.description {
        info_json.insert("description".into(), json!(d));
    }

    let mut root = Map::new();
    root.insert("asyncapi".into(), json!(VERSION));
    root.insert("info".into(), Value::Object(info_json));
    root.insert(
        "defaultContentType".into(),
        json!(asyncapi.default_content_type),
    );
    root.insert("channels".into(), Value::Object(channels));
    root.insert("operations".into(), Value::Object(operations));
    if !schemas.is_empty() {
        root.insert(
            "components".into(),
            json!({ "schemas": schemas.into_iter().collect::<Map<_, _>>() }),
        );
    }
    Value::Object(root)
}

/// The component a payload references, which names its message.
fn message_name(payload: &Value) -> Option<String> {
    let target = payload.get("$ref")?.as_str()?;
    target
        .strip_prefix("#/components/schemas/")
        .map(str::to_string)
}

/// `name` with every run of characters an AsyncAPI map key does not allow
/// replaced by one `_`, and trimmed of them.
fn key_name(name: &str) -> String {
    let mut key = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
            key.push(c);
        } else if !key.is_empty() && !key.ends_with('_') {
            key.push('_');
        }
    }
    let key = key.trim_end_matches('_');
    if key.is_empty() {
        "_".to_string()
    } else {
        key.to_string()
    }
}

fn unique(taken: &mut std::collections::HashSet<String>, base: &str) -> String {
    let mut id = base.to_string();
    let mut n = 1;
    while !taken.insert(id.clone()) {
        n += 1;
        id = format!("{base}_{n}");
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(action: Action, channel: &str, handler: &str, payload: Value) -> EventOperation {
        EventOperation {
            action,
            channel: channel.into(),
            payload,
            handler: handler.into(),
            tag: handler.split('.').next().unwrap().into(),
            summary: None,
            description: None,
        }
    }

    fn document(events: &[EventOperation], schemas: &[(&str, Value)]) -> Value {
        let mut components = ComponentSchemas::default();
        for (name, schema) in schemas {
            components.insert(name, schema.clone());
        }
        to_asyncapi(&Config::default(), &AsyncApi::default(), events, components)
    }

    #[test]
    fn operations_sharing_a_channel_each_keep_their_own_id() {
        let book = json!({ "$ref": "#/components/schemas/Book" });
        let doc = document(
            &[
                op(
                    Action::Receive,
                    "book.published",
                    "Listener.onBook",
                    book.clone(),
                ),
                op(
                    Action::Receive,
                    "book.published",
                    "Audit.onBook",
                    book.clone(),
                ),
                op(Action::Send, "book.published", "Books.create", book.clone()),
                op(
                    Action::Send,
                    "book.published",
                    "Books.create",
                    json!({ "type": "string" }),
                ),
                op(
                    Action::Send,
                    "{\"cmd\":\"sum\"}",
                    "Books.create",
                    Value::Null,
                ),
            ],
            &[("Book", json!({ "type": "object" })), ("Unused", json!({}))],
        );
        assert_eq!(
            doc["channels"],
            json!({
                "book.published": {
                    "address": "book.published",
                    "messages": {
                        "Book": { "name": "Book", "payload": book },
                        "book.published": {
                            "name": "book.published",
                            "payload": { "type": "string" }
                        }
                    }
                },
                "cmd_sum": {
                    "address": "{\"cmd\":\"sum\"}",
                    "messages": { "cmd_sum": { "name": "cmd_sum" } }
                }
            })
        );
        let operations = doc["operations"].as_object().unwrap();
        let ids: Vec<&str> = operations.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            [
                "Audit_onBook",
                "Books_create",
                "Books_create_2",
                "Listener_onBook"
            ]
        );
        assert_eq!(
            operations["Books_create"],
            json!({
                "action": "send",
                "channel": { "$ref": "#/channels/book.published" },
                "tags": [{ "name": "Books" }],
                "messages": [
                    { "$ref": "#/channels/book.published/messages/Book" },
                    { "$ref": "#/channels/book.published/messages/book.published" }
                ]
            })
        );
        assert_eq!(operations["Listener_onBook"]["action"], json!("receive"));
        assert_eq!(doc["asyncapi"], json!("3.0.0"));
        assert_eq!(doc["defaultContentType"], json!("application/json"));
        assert_eq!(
            doc["components"],
            json!({ "schemas": { "Book": { "type": "object" } } })
        );
    }

    #[test]
    fn payload_tuples_are_written_in_draft_07() {
        let doc = document(
            &[op(
                Action::Receive,
                "point",
                "Listener.onPoint",
                json!({ "$ref": "#/components/schemas/Point" }),
            )],
            &[(
                "Point",
                json!({ "type": "array", "prefixItems": [{ "type": "number" }], "items": false }),
            )],
        );
        assert_eq!(
            doc["components"]["schemas"]["Point"],
            json!({ "type": "array", "items": [{ "type": "number" }], "additionalItems": false })
        );
    }
}
