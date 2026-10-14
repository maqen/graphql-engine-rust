//! Building the GraphQL result from a handler's JSON (V2's `makeActionResponseNoRelations`)
//! and joining relationship fields to tables.
//!
//! Relationships run in two passes: [`collect_joins`] gathers the distinct join keys per
//! relationship from the handler's JSON, the caller fetches the rows (one round trip per
//! source), and [`project`] builds the result, looking joined rows up by key.

use std::collections::{HashMap, HashSet};

use hge_ir::action::{
    ActionField, ActionOutput, ActionRelTarget, ActionRelationship, ActionSelection, OutputBase,
};
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
            ActionField::Object { name, fields } => {
                if let Some(v) = obj.get(name) {
                    collect_nested(fields, v, joins);
                }
            }
            ActionField::Typename(_) | ActionField::Scalar { .. } => {}
        }
    }
}

fn collect_nested<'a>(fields: &'a [(String, ActionField)], v: &Value, joins: &mut Joins<'a>) {
    match v {
        Value::Object(o) => collect_object(fields, o, joins),
        Value::Array(items) => items.iter().for_each(|i| collect_nested(fields, i, joins)),
        _ => {}
    }
}

/// How a nested object field missing from the response comes out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Missing {
    /// Left out, as in V2's `makeActionResponseNoRelations` (sync actions, async actions
    /// without relationships).
    Omit,
    /// `null`: V2 reads async outputs with relationships through `jsonb_to_record`.
    Null,
}

/// Build the response value of an action.
pub fn project(
    output: &ActionOutput,
    selection: &ActionSelection,
    v: Value,
    joined: &Joined,
    missing: Missing,
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
                    Value::Object(o) => project_object(fields, &o, joined, missing),
                    other => other,
                })
                .collect(),
        ),
        Value::Object(o) => project_object(fields, &o, joined, missing),
        other => other,
    }
}

/// A nested object field's value, as V2's `mkValue`: an object is projected, an array mapped
/// (dropping scalars, keeping nulls), null kept; `None` (key omitted) for a scalar.
fn nested_value(
    fields: &[(String, ActionField)],
    v: &Value,
    joined: &Joined,
    missing: Missing,
) -> Option<Value> {
    match v {
        Value::Object(o) => Some(project_object(fields, o, joined, missing)),
        Value::Array(items) => Some(Value::Array(
            items
                .iter()
                .filter_map(|i| nested_value(fields, i, joined, missing))
                .collect(),
        )),
        Value::Null => Some(Value::Null),
        _ => None,
    }
}

fn project_object(
    fields: &[(String, ActionField)],
    obj: &Map<String, Value>,
    joined: &Joined,
    missing: Missing,
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
            ActionField::Object { name, fields } => {
                let v = match obj.get(name) {
                    Some(v) => nested_value(fields, v, joined, missing),
                    None => (missing == Missing::Null).then_some(Value::Null),
                };
                // A missing key (unless read through SQL), or a scalar where an object is
                // expected: V2 omits the key (quirk Q2).
                if let Some(v) = v {
                    out.insert(alias.clone(), v);
                }
            }
            ActionField::Relationship(rel) => {
                // A null or missing join field gives null whatever the relationship kind, as in V2.
                let value = match join_key(rel, obj) {
                    None => Value::Null,
                    Some(key) => joined.get(&(rel_id(rel), key)).cloned().unwrap_or_else(|| {
                        match rel.target {
                            ActionRelTarget::Array(_) => Value::Array(vec![]),
                            ActionRelTarget::Object(_) | ActionRelTarget::Aggregate(_) => {
                                Value::Null
                            }
                        }
                    }),
                };
                out.insert(alias.clone(), value);
            }
        }
    }
    Value::Object(out)
}
