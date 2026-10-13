//! `export_metadata` JSON, with V2's exact key order, omission rules and list sorting.
//!
//! V2 builds the top level, sources, tables and a few other objects with hand-written
//! serialisers (fixed key orders, `Serialization.hs`); every other sub-object comes out with
//! lexicographically sorted keys. Matching both keeps `hasura metadata diff` quiet.

use serde_json::{Map, Value};

use crate::Metadata;

const TOP_LEVEL: &[&str] = &[
    "version",
    "sources",
    "remote_schemas",
    "query_collections",
    "allowlist",
    "actions",
    "custom_types",
    "cron_triggers",
    "rest_endpoints",
    "api_limits",
    "metrics_config",
    "inherited_roles",
    "graphql_schema_introspection",
    "network",
    "backend_configs",
    "opentelemetry",
];

const SOURCE: &[&str] = &[
    "name",
    "kind",
    "tables",
    "functions",
    "native_queries",
    "stored_procedures",
    "logical_models",
    "configuration",
    "query_tags",
    "customization",
    "health_check",
];

const TABLE: &[&str] = &[
    "table",
    "is_enum",
    "configuration",
    "object_relationships",
    "array_relationships",
    "computed_fields",
    "remote_relationships",
    "insert_permissions",
    "select_permissions",
    "update_permissions",
    "delete_permissions",
    "event_triggers",
    "apollo_federation_config",
    "logical_model",
];

const INSERT_PERM: &[&str] = &["check", "set", "columns", "backend_only", "validate_input"];
const SELECT_PERM: &[&str] = &[
    "columns",
    "computed_fields",
    "filter",
    "limit",
    "allow_aggregations",
    "query_root_fields",
    "subscription_root_fields",
];
const UPDATE_PERM: &[&str] = &[
    "columns",
    "filter",
    "check",
    "set",
    "backend_only",
    "validate_input",
];
const DELETE_PERM: &[&str] = &["filter", "backend_only", "validate_input"];

const EVENT_TRIGGER: &[&str] = &[
    "name",
    "definition",
    "retry_conf",
    "webhook",
    "webhook_from_env",
    "headers",
    "request_transform",
    "response_transform",
    "cleanup_config",
    "trigger_on_replication",
];

const REMOTE_SCHEMA: &[&str] = &[
    "name",
    "definition",
    "comment",
    "permissions",
    "remote_relationships",
];
const REMOTE_SCHEMA_DEF: &[&str] = &[
    "url",
    "url_from_env",
    "timeout_seconds",
    "customization",
    "headers",
    "introspection_headers",
    "forward_client_headers",
];

const ACTION: &[&str] = &["name", "definition", "comment", "permissions"];
const ACTION_DEF: &[&str] = &[
    "handler",
    "output_type",
    "forward_client_headers",
    "ignored_client_headers",
    "headers",
    "arguments",
    "request_transform",
    "response_transform",
    "type",
    "kind",
    "timeout",
];

const CRON_TRIGGER: &[&str] = &[
    "name",
    "webhook",
    "schedule",
    "include_in_metadata",
    "payload",
    "retry_conf",
    "headers",
    "comment",
    "request_transform",
    "response_transform",
];

const FUNCTION: &[&str] = &["function", "configuration", "permissions", "comment"];
const COMPUTED_FIELD: &[&str] = &["name", "definition", "comment"];
const RELATIONSHIP: &[&str] = &["name", "using", "comment"];
const PERMISSION: &[&str] = &["role", "permission", "comment"];
const COLLECTION: &[&str] = &["name", "definition", "comment"];
const INHERITED_ROLE: &[&str] = &["role_name", "role_set"];
const CUSTOM_TYPES: &[&str] = &["input_objects", "objects", "scalars", "enums"];

/// The `export_metadata` document for `metadata`.
pub fn export_json(metadata: &Metadata) -> Value {
    let value = serde_json::to_value(metadata).unwrap_or(Value::Null);
    let Value::Object(mut top) = value else {
        return value;
    };
    for (key, sort_key) in [
        ("sources", "name"),
        ("remote_schemas", "name"),
        ("query_collections", "name"),
        ("allowlist", "collection"),
        ("actions", "name"),
        ("cron_triggers", "name"),
        ("rest_endpoints", "url"),
        ("inherited_roles", "role_name"),
    ] {
        if let Some(Value::Array(items)) = top.get_mut(key) {
            sort_by_key(items, sort_key);
        }
    }
    let mut out = ordered(&mut top, TOP_LEVEL, |key, v| match key {
        "version" => v,
        "sources" => map_array(v, export_source),
        "remote_schemas" => map_array(v, |rs| {
            let Value::Object(mut obj) = rs else {
                return rs;
            };
            if let Some(def) = obj.remove("definition") {
                obj.insert("definition".into(), fixed(def, REMOTE_SCHEMA_DEF));
            }
            fixed(Value::Object(obj), REMOTE_SCHEMA)
        }),
        "query_collections" => map_array(v, |c| fixed(c, COLLECTION)),
        "actions" => map_array(v, |a| {
            let Value::Object(mut obj) = a else {
                return a;
            };
            if let Some(def) = obj.remove("definition") {
                obj.insert(
                    "definition".into(),
                    fixed(typed_transforms(def), ACTION_DEF),
                );
            }
            fixed(Value::Object(obj), ACTION)
        }),
        "custom_types" => fixed(v, CUSTOM_TYPES),
        "cron_triggers" => map_array(v, |c| fixed(c, CRON_TRIGGER)),
        "inherited_roles" => map_array(v, |r| fixed(r, INHERITED_ROLE)),
        _ => sorted(v),
    });
    // `sources` is always present; everything else only when non-empty.
    if !out.contains_key("sources") {
        out.insert("sources".into(), Value::Array(vec![]));
    }
    let mut result = Map::new();
    for key in TOP_LEVEL {
        if let Some(v) = out.remove(*key) {
            result.insert((*key).to_owned(), v);
        }
    }
    Value::Object(result)
}

/// Transforms go through their typed `ToJSON` (spec 06 §2.7): `version` and
/// `template_engine` are always written, `request_headers` has both lists. Their keys are
/// then sorted like every aeson object.
fn typed_transforms(def: Value) -> Value {
    let Value::Object(mut d) = def else {
        return def;
    };
    if let Some(t) = d.get("request_transform")
        && let Ok(t) = hge_transforms::RequestTransform::from_json(t)
    {
        d.insert("request_transform".into(), t.to_json());
    }
    if let Some(t) = d.get("response_transform")
        && let Ok(t) = hge_transforms::ResponseTransform::from_json(t)
    {
        d.insert("response_transform".into(), t.to_json());
    }
    Value::Object(d)
}

fn export_source(source: Value) -> Value {
    let Value::Object(mut obj) = source else {
        return source;
    };
    if obj.get("kind").and_then(Value::as_str) == Some("pg") {
        obj.insert("kind".into(), Value::String("postgres".into()));
    }
    if let Some(Value::Array(tables)) = obj.get_mut("tables") {
        tables.sort_by_key(table_key);
    }
    if let Some(Value::Array(functions)) = obj.get_mut("functions") {
        functions.sort_by_key(|a| table_key_of(a, "function"));
    }
    let mut out = ordered(&mut obj, SOURCE, |key, v| match key {
        "tables" => map_array(v, export_table),
        "functions" => map_array(v, |f| fixed(f, FUNCTION)),
        _ => sorted(v),
    });
    if !out.contains_key("tables") {
        out.insert("tables".into(), Value::Array(vec![]));
    }
    Value::Object(reorder(out, SOURCE))
}

fn table_key(table: &Value) -> (String, String) {
    table_key_of(table, "table")
}

/// `(schema, name)` of a qualified object in either JSON form.
fn table_key_of(obj: &Value, key: &str) -> (String, String) {
    match obj.get(key) {
        Some(Value::String(name)) => ("public".to_owned(), name.clone()),
        Some(Value::Object(o)) => (
            o.get("schema")
                .and_then(Value::as_str)
                .unwrap_or("public")
                .to_owned(),
            o.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        _ => Default::default(),
    }
}

fn export_table(table: Value) -> Value {
    let Value::Object(mut obj) = table else {
        return table;
    };
    for (key, sort_key) in [
        ("object_relationships", "name"),
        ("array_relationships", "name"),
        ("computed_fields", "name"),
        ("remote_relationships", "name"),
        ("insert_permissions", "role"),
        ("select_permissions", "role"),
        ("update_permissions", "role"),
        ("delete_permissions", "role"),
        ("event_triggers", "name"),
    ] {
        if let Some(Value::Array(items)) = obj.get_mut(key) {
            sort_by_key(items, sort_key);
        }
    }
    let out = ordered(&mut obj, TABLE, |key, v| match key {
        "object_relationships" | "array_relationships" => map_array(v, |r| fixed(r, RELATIONSHIP)),
        "computed_fields" => map_array(v, |c| fixed(c, COMPUTED_FIELD)),
        "insert_permissions" => map_array(v, |p| permission(p, INSERT_PERM)),
        "select_permissions" => map_array(v, |p| permission(p, SELECT_PERM)),
        "update_permissions" => map_array(v, |p| {
            let mut p = permission(p, UPDATE_PERM);
            // V2 always emits `check` for update permissions, `null` when unset.
            if let Some(Value::Object(perm)) = p.get_mut("permission")
                && !perm.contains_key("check")
            {
                perm.insert("check".into(), Value::Null);
                let reordered = reorder(std::mem::take(perm), UPDATE_PERM);
                *perm = reordered;
            }
            p
        }),
        "delete_permissions" => map_array(v, |p| permission(p, DELETE_PERM)),
        "event_triggers" => map_array(v, |t| fixed(t, EVENT_TRIGGER)),
        _ => sorted(v),
    });
    Value::Object(reorder(out, TABLE))
}

fn permission(p: Value, order: &[&str]) -> Value {
    let Value::Object(mut obj) = p else {
        return p;
    };
    if let Some(perm) = obj.remove("permission") {
        obj.insert("permission".into(), fixed(perm, order));
    }
    fixed(Value::Object(obj), PERMISSION)
}

/// An object with a fixed key order: known keys first (in `order`, values sorted
/// recursively unless already handled), then any unknown keys sorted.
fn fixed(v: Value, order: &[&str]) -> Value {
    let Value::Object(mut obj) = v else {
        return sorted(v);
    };
    let out = ordered(&mut obj, order, |_, v| sorted(v));
    Value::Object(reorder(out, order))
}

/// Transform the values of `obj` with `f` (known keys) or [`sorted`] (unknown keys).
fn ordered(
    obj: &mut Map<String, Value>,
    order: &[&str],
    mut f: impl FnMut(&str, Value) -> Value,
) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in std::mem::take(obj) {
        let v = if order.contains(&k.as_str()) {
            f(&k, v)
        } else {
            sorted(v)
        };
        out.insert(k, v);
    }
    out
}

/// Keys in `order` first, then the remaining keys lexicographically.
fn reorder(mut obj: Map<String, Value>, order: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for key in order {
        if let Some(v) = obj.remove(*key) {
            out.insert((*key).to_owned(), v);
        }
    }
    let mut rest: Vec<_> = obj.into_iter().collect();
    rest.sort_by(|a, b| a.0.cmp(&b.0));
    out.extend(rest);
    out
}

/// Recursively sort object keys (array order is kept).
pub fn sorted(v: Value) -> Value {
    match v {
        Value::Object(obj) => {
            let mut entries: Vec<_> = obj.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

fn map_array(v: Value, f: impl FnMut(Value) -> Value) -> Value {
    match v {
        Value::Array(items) => Value::Array(items.into_iter().map(f).collect()),
        other => sorted(other),
    }
}

/// Stable sort of objects by a string field.
fn sort_by_key(items: &mut [Value], key: &str) {
    items.sort_by(|a, b| {
        let ka = a.get(key).map(sort_text).unwrap_or_default();
        let kb = b.get(key).map(sort_text).unwrap_or_default();
        ka.cmp(&kb)
    });
}

fn sort_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transforms_export_through_their_typed_form() {
        let md = crate::from_json(json!({
            "version": 3, "sources": [],
            "actions": [{"name": "a", "definition": {"handler": "http://h", "output_type": "String",
                "request_transform": {"body": "{{ $body }}", "request_headers": {"add_headers": {"b": "1", "a": "2"}}},
                "response_transform": {"version": 2, "body": {"action": "remove"}}}}]
        }))
        .unwrap();
        let def = &export_json(&md)["actions"][0]["definition"];
        assert_eq!(
            def["request_transform"].to_string(),
            r#"{"body":"{{ $body }}","request_headers":{"add_headers":{"a":"2","b":"1"},"remove_headers":[]},"template_engine":"Kriti","version":1}"#
        );
        assert_eq!(
            def["response_transform"].to_string(),
            r#"{"body":{"action":"remove"},"template_engine":"Kriti","version":2}"#
        );
    }

    #[test]
    fn export_orders_and_sorts() {
        let md: Metadata = serde_json::from_value(json!({
            "version": 3,
            "inherited_roles": [{"role_set": ["a"], "role_name": "r"}],
            "sources": [
                {"name": "z", "kind": "pg", "tables": [], "configuration": {"connection_info": {"use_prepared_statements": true, "database_url": "x"}}},
                {"name": "a", "kind": "postgres", "configuration": {"connection_info": {"database_url": "y"}},
                 "tables": [
                    {"table": {"schema": "public", "name": "b"},
                     "update_permissions": [{"role": "u", "permission": {"filter": {}, "columns": ["id"]}}],
                     "select_permissions": [
                        {"role": "z", "permission": {"filter": {"b": 1, "a": 2}, "columns": "*"}},
                        {"role": "m", "permission": {"limit": 1, "columns": ["x"], "filter": {}}}
                     ]},
                    {"table": {"name": "a", "schema": "public"}}
                 ]}
            ]
        }))
        .unwrap();
        let out = export_json(&md);
        let text = out.to_string();
        assert!(
            text.starts_with(r#"{"version":3,"sources":[{"name":"a","kind":"postgres","tables":[{"table":{"name":"a","schema":"public"}},{"table":{"name":"b","schema":"public"},"select_permissions":[{"role":"m","permission":{"columns":["x"],"filter":{},"limit":1}},{"role":"z","permission":{"columns":"*","filter":{"a":2,"b":1}}}],"update_permissions":[{"role":"u","permission":{"columns":["id"],"filter":{},"check":null}}]}],"configuration":"#),
            "{text}"
        );
        assert_eq!(out["sources"][1]["kind"], "postgres");
        assert_eq!(
            out["sources"][1]["configuration"].to_string(),
            r#"{"connection_info":{"database_url":"x","use_prepared_statements":true}}"#
        );
        assert!(text.ends_with(r#""inherited_roles":[{"role_name":"r","role_set":["a"]}]}"#));
    }

    #[test]
    fn empty_metadata_exports_sources() {
        let out = export_json(&Metadata::empty());
        assert_eq!(out, json!({"version": 3, "sources": []}));
    }
}
