//! Inconsistent metadata objects, in V2's `inconsistent_objects` format.
//!
//! An object (source, table, relationship, permission, …) that cannot be resolved against the
//! database is left out of the GraphQL schema and reported as an [`InconsistentObject`] by
//! `get_inconsistent_metadata`, `reload_metadata` and `replace_metadata`.

use serde_json::{Map, Value};

use crate::QualifiedTable;

/// One entry of `inconsistent_objects`.
#[derive(Debug, Clone, PartialEq)]
pub struct InconsistentObject {
    /// V2's `moiTypeName`: `source`, `table`, `object_relation`, `select_permission`, …
    pub kind: String,
    /// V2's `moiName`, e.g. `"table author in source default"`.
    pub name: String,
    /// The message after `"Inconsistent object: "`.
    pub reason: String,
    /// The object's metadata (with `source` / `table` added for table sub-objects).
    pub definition: Value,
    /// Optional structured detail (V2's `message` key).
    pub message: Option<Value>,
    /// Whether the reason is reported as `"Inconsistent object: <reason>"` (most objects)
    /// or bare (e.g. REST endpoint path errors).
    pub prefixed: bool,
    /// Other objects (`(kind, name)`) this inconsistency is about, besides its own id.
    pub involves: Vec<(String, String)>,
}

impl InconsistentObject {
    pub fn new(
        kind: impl Into<String>,
        name: impl Into<String>,
        reason: impl Into<String>,
        definition: Value,
    ) -> Self {
        InconsistentObject {
            kind: kind.into(),
            name: name.into(),
            reason: reason.into(),
            definition,
            message: None,
            prefixed: true,
            involves: vec![],
        }
    }

    /// V2's `AmbiguousRestEndpoints`: several REST endpoints can match the same URL and
    /// method. `conflicts` are `(endpoint name, endpoint JSON)`, sorted by name; V2
    /// attributes the group to the first one (its id, what `drop_inconsistent_metadata`
    /// drops, and the only endpoint whose creation reports it as its own error).
    pub fn ambiguous_rest_endpoints(
        reason: impl Into<String>,
        conflicts: Vec<(String, Value)>,
    ) -> Self {
        let reason = reason.into();
        let involves = conflicts
            .iter()
            .take(1)
            .map(|(name, _)| ("rest_endpoint".to_owned(), format!("rest_endpoint {name}")))
            .collect();
        let definition = Value::Array(conflicts.into_iter().map(|(_, v)| v).collect());
        InconsistentObject {
            involves,
            prefixed: false,
            ..Self::new(AMBIGUOUS_REST_ENDPOINTS, reason.clone(), reason, definition)
        }
    }

    /// Report the reason without the `"Inconsistent object: "` prefix.
    pub fn bare(mut self) -> Self {
        self.prefixed = false;
        self
    }

    /// Whether this inconsistency is about the object `(kind, name)`.
    pub fn involves(&self, kind: &str, name: &str) -> bool {
        (self.kind == kind && self.name == name)
            || self.involves.iter().any(|(k, n)| k == kind && n == name)
    }

    /// The reason as V2 reports it.
    pub fn display_reason(&self) -> String {
        if self.prefixed {
            format!("Inconsistent object: {}", self.reason)
        } else {
            self.reason.clone()
        }
    }

    /// A source that could not be connected to or introspected.
    pub fn source(source: &str, reason: impl Into<String>, definition: Value) -> Self {
        Self::new("source", format!("source {source}"), reason, definition)
    }

    /// An action (`definition` is its metadata).
    pub fn action(name: &str, reason: impl Into<String>, definition: Value) -> Self {
        Self::new("action", format!("action {name}"), reason, definition)
    }

    /// The custom types (`definition` is the `custom_types` metadata).
    pub fn custom_types(reason: impl Into<String>, definition: Value) -> Self {
        Self::new(
            "custom_types",
            "custom_types custom_types",
            reason,
            definition,
        )
    }

    /// A tracked table. `definition` is usually the table's name object.
    pub fn table(
        source: &str,
        table: &QualifiedTable,
        reason: impl Into<String>,
        definition: Value,
    ) -> Self {
        Self::new(
            "table",
            format!("table {} in source {source}", table.v2_text()),
            reason,
            definition,
        )
    }

    /// An object attached to a table (`object_relation`, `select_permission`, …).
    /// `item` is the relationship name or the role.
    pub fn table_object(
        kind: &str,
        item: &str,
        source: &str,
        table: &QualifiedTable,
        reason: impl Into<String>,
        mut definition: Value,
    ) -> Self {
        if let Value::Object(obj) = &mut definition {
            obj.insert("source".into(), Value::String(source.to_owned()));
            obj.insert(
                "table".into(),
                serde_json::to_value(table).unwrap_or(Value::Null),
            );
        }
        Self::new(
            kind,
            format!(
                "{kind} {item} in table {} in source {source}",
                table.v2_text()
            ),
            reason,
            definition,
        )
    }

    /// V2's `ConflictingInheritedPermission`: the parents of inherited role `role` define
    /// different permissions for `entity` (`{"table", "source", "permission_type"}`), so the
    /// role gets none. `entity_text` is the entity as the reason names it. Not a metadata
    /// object: it has an `entity` instead of a `definition` and cannot be dropped.
    pub fn conflicting_inherited_permission(role: &str, entity_text: &str, entity: Value) -> Self {
        Self::new(
            CONFLICTING_INHERITED_PERMISSION,
            role,
            format!(
                "Could not inherit permission for the role '{role}' for the entity: '{entity_text}'"
            ),
            entity,
        )
    }

    pub fn with_message(mut self, message: Value) -> Self {
        self.message = Some(message);
        self
    }

    /// Identity used to decide whether an inconsistency is new (V2 compares object ids).
    pub fn id(&self) -> (&str, &str) {
        match self.involves.first() {
            Some((kind, name)) if self.kind == AMBIGUOUS_REST_ENDPOINTS => (kind, name),
            _ => (&self.kind, &self.name),
        }
    }

    /// The JSON object V2 returns.
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        if self.kind == CONFLICTING_INHERITED_PERMISSION {
            obj.insert("entity".into(), self.definition.clone());
            obj.insert("name".into(), Value::String(self.name.clone()));
            obj.insert("reason".into(), Value::String(self.reason.clone()));
            obj.insert("type".into(), Value::String(self.kind.clone()));
            return Value::Object(obj);
        }
        if self.kind == AMBIGUOUS_REST_ENDPOINTS {
            obj.insert("conflicts".into(), sort_keys(self.definition.clone()));
            obj.insert("reason".into(), Value::String(self.reason.clone()));
            return Value::Object(obj);
        }
        obj.insert("definition".into(), sort_keys(self.definition.clone()));
        if let Some(m) = &self.message {
            obj.insert("message".into(), m.clone());
        }
        obj.insert("name".into(), Value::String(self.name.clone()));
        obj.insert("reason".into(), Value::String(self.display_reason()));
        obj.insert("type".into(), Value::String(self.kind.clone()));
        Value::Object(obj)
    }
}

const CONFLICTING_INHERITED_PERMISSION: &str = "inherited role permission inconsistency";
const AMBIGUOUS_REST_ENDPOINTS: &str = "ambiguous_rest_endpoints";

/// V2 renders definitions from plain aeson values, whose keys come out sorted.
fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, sort_keys(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

impl std::fmt::Display for InconsistentObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.reason)
    }
}

/// `[obj, …]` as JSON.
pub fn inconsistencies_json(objects: &[InconsistentObject]) -> Value {
    Value::Array(objects.iter().map(InconsistentObject::to_json).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn conflicting_inherited_permission_json() {
        let entity = json!({"permission_type": "insert", "source": "default", "table": {"schema": "public", "name": "author"}});
        let o = InconsistentObject::conflicting_inherited_permission(
            "staff",
            "insert permission on table \"author\" in source \"default\"",
            entity.clone(),
        );
        assert_eq!(
            o.to_json(),
            json!({
                "entity": entity,
                "name": "staff",
                "reason": "Could not inherit permission for the role 'staff' for the entity: 'insert permission on table \"author\" in source \"default\"'",
                "type": "inherited role permission inconsistency",
            })
        );
    }
}
