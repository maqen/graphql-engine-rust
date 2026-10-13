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
const ACTION_PERMISSION: &[&str] = &["role", "comment"];
const ACTION_ARGUMENT: &[&str] = &["name", "type", "description"];
const CUSTOM_TYPES: &[&str] = &["input_objects", "objects", "scalars", "enums"];
const INPUT_OBJECT_TYPE: &[&str] = &["name", "fields", "description"];
const OBJECT_TYPE: &[&str] = &["name", "fields", "description", "relationships"];
const TYPE_FIELD: &[&str] = &["name", "type", "arguments", "description"];
const SCALAR_TYPE: &[&str] = &["name", "description"];
const ENUM_TYPE: &[&str] = &["name", "values", "description"];

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
            nested(rs, REMOTE_SCHEMA, "definition", |d| {
                fixed(d, REMOTE_SCHEMA_DEF)
            })
        }),
        "query_collections" => map_array(v, |c| fixed(c, COLLECTION)),
        "actions" => map_array(v, export_action),
        "custom_types" => export_custom_types(v),
        "cron_triggers" => map_array(v, |c| fixed(c, CRON_TRIGGER)),
        "inherited_roles" => map_array(v, |r| fixed(r, INHERITED_ROLE)),
        _ => sorted(v),
    });
    // `customTypesToOrdJSON` writes nothing when every type list is empty.
    if out
        .get("custom_types")
        .is_some_and(|t| t.as_object().is_some_and(Map::is_empty))
    {
        out.remove("custom_types");
    }
    // `sources` is always present; everything else only when non-empty.
    if !out.contains_key("sources") {
        out.insert("sources".into(), Value::Array(vec![]));
    }
    let mut result = Map::new();
    for key in TOP_LEVEL {
        if let Some(v) = out.remove(*key)
            && (matches!(*key, "version" | "sources") || !is_empty(&v))
        {
            result.insert((*key).to_owned(), v);
        }
    }
    Value::Object(result)
}

/// `actionMetadataToOrdJSON`: arguments and permissions have fixed key orders too.
fn export_action(action: Value) -> Value {
    // `actionMetadataToOrdJSON` leaves out a `null` comment (of the action, a permission,
    // an argument's description) and an empty permission list.
    let action = match action {
        Value::Object(mut a) => {
            if a.get("comment").is_some_and(Value::is_null) {
                a.remove("comment");
            }
            if a.get("permissions")
                .is_some_and(|p| p.is_null() || p.as_array().is_some_and(Vec::is_empty))
            {
                a.remove("permissions");
            }
            Value::Object(a)
        }
        other => other,
    };
    fixed_with(action, ACTION, |key, v| match key {
        "definition" => fixed_with(
            typed_transforms(action_defaults(v)),
            ACTION_DEF,
            |key, v| match key {
                "arguments" => map_array(v, |a| {
                    fixed(without_null(a, "description"), ACTION_ARGUMENT)
                }),
                _ => sorted(v),
            },
        ),
        "permissions" => map_array(v, |p| fixed(without_null(p, "comment"), ACTION_PERMISSION)),
        _ => sorted(v),
    })
}

/// `v` without `key` when that is `null`.
fn without_null(v: Value, key: &str) -> Value {
    match v {
        Value::Object(mut o) => {
            if o.get(key).is_some_and(Value::is_null) {
                o.remove(key);
            }
            Value::Object(o)
        }
        other => other,
    }
}

/// V2 exports its parsed definition: defaults are written out (`type`,
/// `ignored_client_headers`, `kind` for mutations) and default-valued optional keys are
/// left out, so a definition loaded as written (`replace_metadata`, a metadata directory)
/// exports like one stored by `create_action`.
fn action_defaults(def: Value) -> Value {
    let Value::Object(mut d) = def else {
        return def;
    };
    let ty = match d.get("type") {
        None | Some(Value::Null) => "mutation".to_owned(),
        Some(Value::String(t)) => t.clone(),
        Some(_) => return Value::Object(d),
    };
    match ty.as_str() {
        "query" => {
            d.remove("kind");
        }
        "mutation" => {
            if d.get("kind").is_none_or(Value::is_null) {
                d.insert("kind".into(), Value::String("synchronous".into()));
            }
        }
        _ => return Value::Object(d),
    }
    d.insert("type".into(), Value::String(ty));
    if d.get("ignored_client_headers").is_none_or(Value::is_null) {
        let defaults = crate::actions::DEFAULT_IGNORED_CLIENT_HEADERS
            .iter()
            .map(|h| Value::String((*h).to_owned()))
            .collect();
        d.insert("ignored_client_headers".into(), Value::Array(defaults));
    }
    // `Timeout` is an `Int`: `2.0` is written as `2`.
    if let Some(Value::Number(n)) = d.get("timeout")
        && let Ok(t) = crate::actions::timeout_secs(n)
    {
        d.insert("timeout".into(), Value::from(t));
    }
    let default_valued = |key: &str, v: &Value| match key {
        "forward_client_headers" => matches!(v, Value::Null | Value::Bool(false)),
        "timeout" => v.is_null() || v.as_u64() == Some(crate::actions::DEFAULT_ACTION_TIMEOUT_SECS),
        "headers" | "arguments" => v.is_null() || v.as_array().is_some_and(Vec::is_empty),
        "request_transform" | "response_transform" => v.is_null(),
        _ => false,
    };
    d.retain(|k, v| !default_valued(k, v));
    Value::Object(d)
}

/// `customTypesToOrdJSON`: types and their fields have fixed key orders; relationships
/// and enum values come out of plain `ToJSON` (sorted, enum values with `null`s).
fn export_custom_types(types: Value) -> Value {
    let types = drop_custom_type_defaults(types);
    let with_fields = |order: &'static [&'static str]| {
        move |t: Value| {
            fixed_with(t, order, |key, v| match key {
                "fields" => map_array(v, |f| fixed(f, TYPE_FIELD)),
                _ => sorted(v),
            })
        }
    };
    fixed_with(types, CUSTOM_TYPES, |key, v| match key {
        "input_objects" => map_array(v, with_fields(INPUT_OBJECT_TYPE)),
        "objects" => map_array(v, |t| {
            fixed_with(t, OBJECT_TYPE, |key, v| match key {
                "fields" => map_array(v, |f| fixed(f, TYPE_FIELD)),
                "relationships" => map_array(v, |r| sorted(typed_type_relationship(r))),
                _ => sorted(v),
            })
        }),
        "scalars" => map_array(v, |t| fixed(t, SCALAR_TYPE)),
        "enums" => map_array(v, |t| {
            fixed_with(t, ENUM_TYPE, |key, v| match key {
                // Plain `ToJSON` of `EnumValueDefinition`: unset fields are `null`.
                "values" => map_array(v, |value| {
                    let Value::Object(mut o) = value else {
                        return sorted(value);
                    };
                    for key in ["description", "is_deprecated"] {
                        o.entry(key).or_insert(Value::Null);
                    }
                    sorted(Value::Object(o))
                }),
                _ => sorted(v),
            })
        }),
        _ => sorted(v),
    })
}

/// What `customTypesToOrdJSON` leaves out: empty type lists, `null` descriptions and
/// field arguments, and an empty relationship list.
fn drop_custom_type_defaults(types: Value) -> Value {
    fn clean(v: &mut Value) {
        if let Value::Object(o) = v {
            o.retain(|k, v| {
                !(matches!(k.as_str(), "description" | "arguments") && v.is_null()
                    || k == "relationships" && v.as_array().is_some_and(Vec::is_empty))
            });
            if let Some(Value::Array(fields)) = o.get_mut("fields") {
                fields.iter_mut().for_each(clean);
            }
        }
    }
    let Value::Object(mut t) = types else {
        return types;
    };
    t.retain(|_, v| !(v.is_null() || v.as_array().is_some_and(Vec::is_empty)));
    for list in t.values_mut() {
        if let Value::Array(items) = list {
            items.iter_mut().for_each(clean);
        }
    }
    Value::Object(t)
}

/// A custom type relationship as its parsed `ToJSON` writes it: `source` defaults to
/// `default` and `remote_table` is always `{schema, name}`.
fn typed_type_relationship(rel: Value) -> Value {
    let Value::Object(mut r) = rel else {
        return rel;
    };
    if r.get("source").is_none_or(Value::is_null) {
        r.insert("source".into(), Value::String("default".into()));
    }
    if let Some(Value::String(name)) = r.get("remote_table") {
        let table = serde_json::json!({"schema": "public", "name": name});
        r.insert("remote_table".into(), table);
    }
    Value::Object(r)
}

/// V2 leaves out top-level objects equal to their default. Covered here: nothing but empty
/// lists and objects (`network: {}`, `graphql_schema_introspection: {disabled_for_roles: []}`).
fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(obj) => obj.values().all(is_empty),
        _ => false,
    }
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
    let mut out = ordered(&mut obj, TABLE, |key, v| match key {
        "configuration" => table_config(v),
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
    if out.get("configuration") == Some(&Value::Null) {
        out.remove("configuration");
    }
    Value::Object(reorder(out, TABLE))
}

/// V2's table configuration: `custom_column_names` merged into `column_config` (which
/// wins) and derived back from it, and `column_config`, `custom_column_names` and
/// `custom_root_fields` always present. `null` when the configuration is empty.
fn table_config(v: Value) -> Value {
    let Value::Object(mut obj) = v else {
        return sorted(v);
    };
    let mut column_config = match obj.remove("column_config") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    if let Some(Value::Object(legacy)) = obj.remove("custom_column_names") {
        for (column, name) in legacy {
            let entry = column_config
                .entry(column)
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(cfg) = entry
                && cfg.get("custom_name").is_none_or(Value::is_null)
            {
                cfg.insert("custom_name".into(), name);
            }
        }
    }
    let custom_column_names: Map<String, Value> = column_config
        .iter()
        .filter_map(|(column, cfg)| {
            cfg.get("custom_name")
                .filter(|n| !n.is_null())
                .map(|n| (column.clone(), n.clone()))
        })
        .collect();
    let root_fields = match obj.remove("custom_root_fields") {
        Some(Value::Object(m)) => m.into_iter().filter(|(_, v)| !v.is_null()).collect(),
        _ => Map::new(),
    };
    obj.retain(|_, v| !v.is_null());
    if obj.is_empty() && column_config.is_empty() && root_fields.is_empty() {
        return Value::Null;
    }
    obj.insert("column_config".into(), Value::Object(column_config));
    obj.insert(
        "custom_column_names".into(),
        Value::Object(custom_column_names),
    );
    obj.insert("custom_root_fields".into(), Value::Object(root_fields));
    sorted(Value::Object(obj))
}

fn permission(p: Value, order: &[&str]) -> Value {
    let Value::Object(mut obj) = p else {
        return p;
    };
    let perm = obj.remove("permission");
    let mut out = fixed(Value::Object(obj), PERMISSION);
    if let (Some(perm), Value::Object(o)) = (perm, &mut out) {
        o.insert("permission".into(), fixed(normalise_bool_exps(perm), order));
        *o = reorder(std::mem::take(o), PERMISSION);
    }
    out
}

/// V2 re-serialises boolean expressions: an `_exists` table always comes out qualified.
fn normalise_bool_exps(perm: Value) -> Value {
    let Value::Object(mut obj) = perm else {
        return perm;
    };
    for key in ["filter", "check"] {
        if let Some(exp) = obj.get_mut(key) {
            normalise_bool_exp(exp);
        }
    }
    Value::Object(obj)
}

/// Walk the boolean structure of `exp` (`_and`/`_or`/`_not` and `_exists._where`). V2
/// keeps column and relationship values as raw JSON, so those are left alone; the
/// `$and`/`$or`/`$not` spellings come out as `_and`/`_or`/`_not`.
fn normalise_bool_exp(exp: &mut Value) {
    let Value::Object(obj) = exp else {
        return;
    };
    let entries = std::mem::take(obj);
    for (k, mut v) in entries {
        let key = match k.as_str() {
            "$and" => "_and".to_owned(),
            "$or" => "_or".to_owned(),
            "$not" => "_not".to_owned(),
            _ => k,
        };
        match key.as_str() {
            "_and" | "_or" => match &mut v {
                Value::Array(items) => items.iter_mut().for_each(normalise_bool_exp),
                other => normalise_bool_exp(other),
            },
            "_not" => normalise_bool_exp(&mut v),
            "_exists" => {
                if let Some(Value::String(name)) = v.get("_table").cloned() {
                    v["_table"] = serde_json::json!({"name": name, "schema": "public"});
                }
                if let Some(w) = v.get_mut("_where") {
                    normalise_bool_exp(w);
                }
            }
            _ => {}
        }
        obj.insert(key, v);
    }
}

/// [`fixed`] for `v`, then `f` for its `key` (which `fixed` alone would sort).
fn nested(v: Value, order: &[&str], key: &str, f: impl FnOnce(Value) -> Value) -> Value {
    let mut out = fixed(v, order);
    if let Some(inner) = out.get_mut(key) {
        *inner = f(std::mem::take(inner));
    }
    out
}

/// An object with a fixed key order: known keys first (in `order`, values sorted
/// recursively unless already handled), then any unknown keys sorted.
fn fixed(v: Value, order: &[&str]) -> Value {
    fixed_with(v, order, |_, v| sorted(v))
}

/// [`fixed`], with the values of known keys transformed by `f`.
fn fixed_with(v: Value, order: &[&str], f: impl FnMut(&str, Value) -> Value) -> Value {
    let Value::Object(mut obj) = v else {
        return sorted(v);
    };
    let out = ordered(&mut obj, order, f);
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
    fn actions_and_custom_types_keep_v2_key_orders() {
        let md = crate::from_json(json!({
            "version": 3, "sources": [],
            "actions": [{"permissions": [{"comment": "c", "role": "r"}], "name": "a",
                "definition": {"type": "query", "output_type": "O", "handler": "h",
                    "arguments": [{"type": "Int", "description": "d", "name": "x"}]}},
                {"name": "b", "comment": null, "permissions": [],
                    "definition": {"handler": "h", "output_type": "O", "timeout": 30,
                    "forward_client_headers": false, "headers": [], "ignored_client_headers": [],
                    "arguments": [{"name": "y", "type": "Int", "description": null}]}}],
            "custom_types": {
                "enums": [{"values": [{"value": "A", "description": "d"}], "name": "E"}],
                "objects": [{"relationships": [{"type": "object", "name": "r", "remote_table": "t",
                        "field_mapping": {"f": "id"}}], "description": "d",
                    "fields": [{"type": "Int", "description": "d", "name": "f"}], "name": "O"},
                    {"name": "P", "description": null, "relationships": [],
                        "fields": [{"name": "g", "type": "Int", "description": null}]}],
                "input_objects": [], "scalars": []
            }
        }))
        .unwrap();
        let empty = crate::from_json(json!({"version": 3, "sources": [],
            "custom_types": {"enums": [], "objects": []}}))
        .unwrap();
        assert!(export_json(&empty).get("custom_types").is_none());
        let out = export_json(&md);
        assert_eq!(
            out["actions"][0].to_string(),
            format!(
                r#"{{"name":"a","definition":{{"handler":"h","output_type":"O","ignored_client_headers":{},"arguments":[{{"name":"x","type":"Int","description":"d"}}],"type":"query"}},"permissions":[{{"role":"r","comment":"c"}}]}}"#,
                json!(crate::actions::DEFAULT_IGNORED_CLIENT_HEADERS)
            )
        );
        assert_eq!(
            out["actions"][1].to_string(),
            r#"{"name":"b","definition":{"handler":"h","output_type":"O","ignored_client_headers":[],"arguments":[{"name":"y","type":"Int"}],"type":"mutation","kind":"synchronous"}}"#
        );
        assert_eq!(
            out["custom_types"].to_string(),
            r#"{"objects":[{"name":"O","fields":[{"name":"f","type":"Int","description":"d"}],"description":"d","relationships":[{"field_mapping":{"f":"id"},"name":"r","remote_table":{"name":"t","schema":"public"},"source":"default","type":"object"}]},{"name":"P","fields":[{"name":"g","type":"Int"}]}],"enums":[{"name":"E","values":[{"description":"d","is_deprecated":null,"value":"A"}]}]}"#
        );
    }

    #[test]
    fn bool_exp_normalisation_walks_only_boolean_structure() {
        let mut exp = json!({
            "$and": [
                {"_exists": {"_table": "a", "_where": {"_exists": {"_table": "b", "_where": {}}}}},
                {"$not": {"_exists": {"_table": "c", "_where": {}}}},
                {"rel": {"_exists": {"_table": "raw"}}}
            ],
            "$or": [{"id": {"_eq": 1}}],
            "tags": {"_eq": {"_exists": {"_table": "literal"}}}
        });
        normalise_bool_exp(&mut exp);
        let q = |n: &str| json!({"name": n, "schema": "public"});
        assert_eq!(
            exp.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["_and", "_or", "tags"]
        );
        assert_eq!(exp["_and"][0]["_exists"]["_table"], q("a"));
        assert_eq!(
            exp["_and"][0]["_exists"]["_where"]["_exists"]["_table"],
            q("b")
        );
        assert_eq!(exp["_and"][1]["_not"]["_exists"]["_table"], q("c"));
        // Column and relationship values stay raw JSON.
        assert_eq!(
            exp["_and"][2],
            json!({"rel": {"_exists": {"_table": "raw"}}})
        );
        assert_eq!(exp["tags"]["_eq"]["_exists"]["_table"], json!("literal"));
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
    fn nested_objects_keep_their_fixed_order() {
        let md: Metadata = serde_json::from_value(json!({
            "version": 3,
            "sources": [{"name": "a", "kind": "postgres",
                "configuration": {"connection_info": {"database_url": "y"}},
                "tables": [{"table": {"schema": "public", "name": "b"},
                    "insert_permissions": [{"role": "m", "permission": {"columns": ["x"], "set": {"x": 1}, "check": {}}}],
                    "select_permissions": [{"role": "m", "permission": {"allow_aggregations": true, "limit": 1, "columns": ["x"], "filter": {}}}]}]}],
            "actions": [{"name": "a", "definition": {"type": "query", "output_type": "String", "handler": "http://h"}}]
        }))
        .unwrap();
        let out = export_json(&md);
        let table = &out["sources"][0]["tables"][0];
        assert_eq!(
            table["select_permissions"][0]["permission"].to_string(),
            r#"{"columns":["x"],"filter":{},"limit":1,"allow_aggregations":true}"#
        );
        assert_eq!(
            table["insert_permissions"][0]["permission"].to_string(),
            r#"{"check":{},"set":{"x":1},"columns":["x"]}"#
        );
        let def = out["actions"][0]["definition"].as_object().unwrap();
        let keys: Vec<_> = def.keys().map(String::as_str).collect();
        assert_eq!(keys[..2], ["handler", "output_type"]);
    }

    #[test]
    fn empty_top_level_objects_are_left_out() {
        let md = crate::from_json(json!({
            "version": 3, "sources": [], "network": {},
            "graphql_schema_introspection": {"disabled_for_roles": []},
            "backend_configs": {}, "opentelemetry": {}
        }))
        .unwrap();
        assert_eq!(export_json(&md), json!({"version": 3, "sources": []}));
    }

    #[test]
    fn empty_metadata_exports_sources() {
        let out = export_json(&Metadata::empty());
        assert_eq!(out, json!({"version": 3, "sources": []}));
    }
}
