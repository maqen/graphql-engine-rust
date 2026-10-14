//! Lowering action root fields to IR.

use hge_core::{Code, Error};
use hge_ir::action::{
    ActionCall, ActionField, ActionRelTarget, ActionRelationship, ActionSelection, AsyncField,
};
use hge_ir::{ActionRootField, RootField};
use serde_json::{Map, Value as Json};

use super::AsyncResultField;
use super::resolve::{ActionInfo, ActionRoot};
use crate::build::RoleSchema;
use crate::model::{FieldKind, TypeDef};
use crate::normalize::{Selected, SelectedField};

pub fn lower_action(
    rs: &RoleSchema,
    action: &ActionInfo,
    field: &SelectedField<'_>,
) -> Result<RootField, Error> {
    let input = Json::Object(
        field
            .args
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Map<_, _>>(),
    );
    let call = match action.root {
        ActionRoot::AsyncMutation => ActionCall::AsyncMutation { input },
        ActionRoot::Query | ActionRoot::SyncMutation => ActionCall::Sync {
            input,
            output: action.output.clone(),
            selection: selection(rs, &field.selection)?,
        },
    };
    Ok(RootField::Action(Box::new(ActionRootField {
        action: action.name.as_str().into(),
        call,
    })))
}

pub fn lower_async_query(
    rs: &RoleSchema,
    action: &ActionInfo,
    field: &SelectedField<'_>,
) -> Result<RootField, Error> {
    let id = action_id(field)?;
    let Some(object) = &action.output_object else {
        // Scalar outputs resolve to an empty object in V2 (quirk Q7).
        return Ok(RootField::Static(Json::Object(Map::new())));
    };
    let mut fields = Vec::with_capacity(field.selection.len());
    for sel in &field.selection {
        let f = match sel.kind {
            Selected::Typename(t) => AsyncField::Typename(t.to_owned()),
            Selected::Field(def) => match &def.kind {
                FieldKind::AsyncActionResult(AsyncResultField::Id) => AsyncField::Id,
                FieldKind::AsyncActionResult(AsyncResultField::CreatedAt) => AsyncField::CreatedAt,
                FieldKind::AsyncActionResult(AsyncResultField::Errors) => AsyncField::Errors,
                FieldKind::AsyncActionResult(AsyncResultField::Output) => {
                    AsyncField::Output(selection(rs, &sel.selection)?)
                }
                other => return Err(unexpected_kind(other)),
            },
        };
        fields.push((sel.alias.clone(), f));
    }
    Ok(RootField::Action(Box::new(ActionRootField {
        action: action.name.as_str().into(),
        call: ActionCall::AsyncQuery {
            id,
            output: action.output.clone(),
            fields,
            has_relationships: !object.relationships.is_empty(),
        },
    })))
}

/// The `id: uuid!` argument, parsed like V2's `uuid` scalar parser: the aeson `UUID` instance
/// (`Data.UUID.fromText`, the 36-character hyphenated form only), before any database access.
fn action_id(field: &SelectedField<'_>) -> Result<String, Error> {
    let path = field.arg_path("id");
    match field.arg("id") {
        Some(Json::String(s)) if is_uuid(s) => Ok(s.to_ascii_lowercase()),
        Some(Json::String(_)) => Err(Error::new(Code::ParseFailed, "invalid UUID").with_path(path)),
        Some(other) => Err(Error::validation(format!(
            "expected a UUID for type 'uuid', but found {}",
            match other {
                Json::Null => "null",
                Json::Bool(_) => "a boolean",
                Json::Number(_) => "a number",
                Json::Array(_) => "a list",
                _ => "an object",
            }
        ))
        .with_path(path)),
        None => Err(Error::validation("missing required field 'id'").with_path(path)),
    }
}

/// `Data.UUID.fromText`: 8-4-4-4-12 hex digits (either case).
fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn unexpected_kind(kind: &FieldKind) -> Error {
    Error::unexpected(format!("unexpected field kind {kind:?} in action output"))
}

fn selection(rs: &RoleSchema, selection: &[SelectedField<'_>]) -> Result<ActionSelection, Error> {
    if selection.is_empty() {
        return Ok(ActionSelection::Leaf);
    }
    Ok(ActionSelection::Object(object_fields(rs, selection)?))
}

fn object_fields(
    rs: &RoleSchema,
    selection: &[SelectedField<'_>],
) -> Result<Vec<(String, ActionField)>, Error> {
    let mut out = Vec::with_capacity(selection.len());
    for sel in selection {
        let f = match sel.kind {
            Selected::Typename(t) => ActionField::Typename(t.to_owned()),
            Selected::Field(def) => match &def.kind {
                FieldKind::CustomObjectField => {
                    let nested = matches!(rs.schema.get(def.ty.base()), Some(TypeDef::Object(_)));
                    if nested {
                        ActionField::Object {
                            name: sel.name.clone(),
                            fields: object_fields(rs, &sel.selection)?,
                        }
                    } else {
                        ActionField::Scalar {
                            name: sel.name.clone(),
                        }
                    }
                }
                FieldKind::ActionRelationship(rel)
                | FieldKind::ActionRelationshipAggregate(rel) => {
                    let table = rs
                        .tables
                        .get(&rel.source, &rel.table)
                        .ok_or_else(|| Error::unexpected(format!("unknown table {}", rel.table)))?;
                    let mut mapping = Vec::with_capacity(rel.mapping.len());
                    for (field, column) in &rel.mapping {
                        let col = table
                            .columns
                            .get(column)
                            .ok_or_else(|| Error::unexpected(format!("unknown column {column}")))?;
                        mapping.push((field.clone(), column.clone(), col.ty.clone()));
                    }
                    let target = if matches!(def.kind, FieldKind::ActionRelationshipAggregate(_)) {
                        ActionRelTarget::Aggregate(crate::lower::aggregate_select(rs, table, sel)?)
                    } else if rel.array {
                        ActionRelTarget::Array(crate::lower::table_select(rs, table, sel, true)?)
                    } else {
                        ActionRelTarget::Object(crate::lower::table_select(rs, table, sel, false)?)
                    };
                    ActionField::Relationship(Box::new(ActionRelationship {
                        source: rel.source.clone(),
                        mapping,
                        target,
                    }))
                }
                other => return Err(unexpected_kind(other)),
            },
        };
        out.push((sel.alias.clone(), f));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::is_uuid;

    #[test]
    fn uuids_are_hyphenated() {
        assert!(is_uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"));
        assert!(is_uuid("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11"));
        assert!(!is_uuid("{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}"));
        assert!(!is_uuid("a0eebc999c0b4ef8bb6d6bb9bd380a11"));
        assert!(!is_uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1"));
        assert!(!is_uuid("a0eebc99x9c0b-4ef8-bb6d-6bb9bd380a11"));
    }
}
