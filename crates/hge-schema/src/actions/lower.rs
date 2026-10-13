//! Lowering action root fields to IR.

use hge_core::Error;
use hge_ir::action::{ActionCall, ActionField, ActionRelationship, ActionSelection, AsyncField};
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
    let id = field
        .arg("id")
        .and_then(Json::as_str)
        .ok_or_else(|| {
            Error::validation("missing required field 'id'").with_path(field.arg_path("id"))
        })?
        .to_owned();
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
                FieldKind::ActionRelationship(rel) => {
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
                    ActionField::Relationship(Box::new(ActionRelationship {
                        source: rel.source.clone(),
                        array: rel.array,
                        mapping,
                        select: crate::lower::table_select(rs, table, sel, rel.array)?,
                    }))
                }
                other => return Err(unexpected_kind(other)),
            },
        };
        out.push((sel.alias.clone(), f));
    }
    Ok(out)
}
