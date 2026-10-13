//! Building the GraphQL result from a handler's JSON (V2's `makeActionResponseNoRelations`)
//! and joining relationship fields to tables.
//!
//! Relationships run in two passes: [`collect_joins`] gathers the distinct join keys per
//! relationship from the handler's JSON, the caller fetches the rows (one round trip per
//! source), and [`project`] builds the result, looking joined rows up by key.

use std::collections::{HashMap, HashSet};

use hge_ir::action::{ActionField, ActionOutput, ActionRelationship, ActionSelection, OutputBase};
use serde_json::{Map, Value};

/// Identity of a relationship field within one operation.
pub type RelId = usize;

pub fn rel_id(rel: &ActionRelationship) -> RelId {
    rel as *const ActionRelationship as usize
}

/// Join keys: relationship → distinct key tuples (in first-seen order).
#[derive(Debug, Default)]
pub struct Joins<'a> {
    pub keys: Vec<(&'a ActionRelationship, Vec<Vec<Value>>)>,
    seen: HashSet<(RelId, Vec<Value>)>,
}

impl<'a> Joins<'a> {
    fn add(&mut self, rel: &'a ActionRelationship, key: Vec<Value>) {
        let id = rel_id(rel);
        if !self.seen.insert((id, key.clone())) {
            return;
        }
        match self.keys.iter_mut().find(|(r, _)| rel_id(r) == id) {
            Some((_, keys)) => keys.push(key),
            None => self.keys.push((rel, vec![key])),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Joined rows, by relationship and key.
pub type Joined = HashMap<(RelId, Vec<Value>), Value>;

/// The key of a relationship in a handler object, or `None` if a key field is null/missing.
fn join_key(rel: &ActionRelationship, obj: &Map<String, Value>) -> Option<Vec<Value>> {
    rel.mapping
        .iter()
        .map(|(field, _, _)| match obj.get(field) {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.clone()),
        })
        .collect()
}

/// Whether the output is returned verbatim (custom scalar outputs).
fn passthrough(output: &ActionOutput) -> bool {
    output.base == OutputBase::CustomScalar
}

pub fn collect_joins<'a>(
    output: &ActionOutput,
    selection: &'a ActionSelection,
    v: &Value,
    joins: &mut Joins<'a>,
) {
    if passthrough(output) {
        return;
    }
    let ActionSelection::Object(fields) = selection else {
        return;
    };
    match v {
        Value::Array(items) => {
            for item in items {
                if let Value::Object(o) = item {
                    collect_object(fields, o, joins);
                }
            }
        }
        Value::Object(o) => collect_object(fields, o, joins),
        _ => {}
    }
}

fn collect_object<'a>(
    fields: &'a [(String, ActionField)],
    obj: &Map<String, Value>,
    joins: &mut Joins<'a>,
) {
    for (_, f) in fields {
        match f {
            ActionField::Relationship(rel) => {
                if let Some(key) = join_key(rel, obj) {
                    joins.add(rel, key);
                }
            }
            ActionField::Object { name, fields } => match obj.get(name) {
                Some(Value::Object(o)) => collect_object(fields, o, joins),
                Some(Value::Array(items)) => {
                    for item in items {
                        if let Value::Object(o) = item {
                            collect_object(fields, o, joins);
                        }
                    }
                }
                _ => {}
            },
            ActionField::Typename(_) | ActionField::Scalar { .. } => {}
        }
    }
}

/// Build the response value of a synchronous action.
pub fn project(
    output: &ActionOutput,
    selection: &ActionSelection,
    v: Value,
    joined: &Joined,
) -> Value {
    if passthrough(output) {
        return v;
    }
    let ActionSelection::Object(fields) = selection else {
        return v;
    };
    match v {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| match item {
                    Value::Object(o) => project_object(fields, &o, joined),
                    other => other,
                })
                .collect(),
        ),
        Value::Object(o) => project_object(fields, &o, joined),
        other => other,
    }
}

fn project_object(
    fields: &[(String, ActionField)],
    obj: &Map<String, Value>,
    joined: &Joined,
) -> Value {
    let mut out = Map::with_capacity(fields.len());
    for (alias, f) in fields {
        match f {
            ActionField::Typename(t) => {
                out.insert(alias.clone(), Value::String(t.clone()));
            }
            ActionField::Scalar { name } => {
                out.insert(alias.clone(), obj.get(name).cloned().unwrap_or(Value::Null));
            }
            // V2 reads nested objects by alias (quirk Q1); we read them by field name.
            ActionField::Object { name, fields } => match obj.get(name) {
                Some(Value::Object(o)) => {
                    out.insert(alias.clone(), project_object(fields, o, joined));
                }
                Some(Value::Array(items)) => {
                    let items = items
                        .iter()
                        .filter_map(|i| i.as_object())
                        .map(|o| project_object(fields, o, joined))
                        .collect();
                    out.insert(alias.clone(), Value::Array(items));
                }
                None | Some(Value::Null) => {
                    out.insert(alias.clone(), Value::Null);
                }
                // A scalar where an object is expected: V2 omits the key (quirk Q2).
                Some(_) => {}
            },
            ActionField::Relationship(rel) => {
                let value = join_key(rel, obj)
                    .and_then(|key| joined.get(&(rel_id(rel), key)).cloned())
                    .unwrap_or_else(|| {
                        if rel.array {
                            Value::Array(vec![])
                        } else {
                            Value::Null
                        }
                    });
                out.insert(alias.clone(), value);
            }
        }
    }
    Value::Object(out)
}
