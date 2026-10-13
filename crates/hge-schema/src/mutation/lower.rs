//! Lowering mutation root fields to IR.

use std::sync::Arc;

use hge_core::{ColumnType, Error};
use hge_ir::{
    BoolExp, CompareOp, Delete, Insert, InsertCheck, InsertInto, InsertObject, Mutation,
    MutationOutput, MutationResponseField, MutationRootField, MutationTable, OnConflict,
    RelationshipInsert, RootField, Update, UpdateExp, Value,
};
use hge_metadata::{ObjectFkOn, ObjectRelUsing};
use serde_json::{Map, Value as Json};

use super::MutationField;
use super::access::{InsertAccess, MutationAccess, UpdateAccess};
use crate::lower::{Lowerer, to_value};
use crate::model::FieldKind;
use crate::normalize::{Selected, SelectedField};
use crate::tables::{ColumnInfo, RelationshipInfo, TableInfo};

pub(crate) fn root_field(
    lw: &Lowerer<'_>,
    kind: &MutationField,
    field: &SelectedField<'_>,
) -> Result<RootField, Error> {
    let ml = MutationLowerer { lw };
    let (table, mutation) = match kind {
        MutationField::Insert(t) => (t, ml.insert(t, field, false)?),
        MutationField::InsertOne(t) => (t, ml.insert(t, field, true)?),
        MutationField::Update(t) => (t, Mutation::Update(Box::new(ml.update(t, field, false)?))),
        MutationField::UpdateByPk(t) => (t, Mutation::Update(Box::new(ml.update(t, field, true)?))),
        MutationField::UpdateMany(t) => (t, ml.update_many(t, field)?),
        MutationField::Delete(t) => (t, ml.delete(t, field, false)?),
        MutationField::DeleteByPk(t) => (t, ml.delete(t, field, true)?),
        MutationField::AffectedRows | MutationField::Returning(_) => {
            return Err(Error::unexpected("mutation response field at the root"));
        }
    };
    Ok(RootField::Mutation(Box::new(MutationRootField {
        source: table.source.clone(),
        mutation,
    })))
}

struct MutationLowerer<'a, 'b> {
    lw: &'b Lowerer<'a>,
}

fn mutation_table(table: &TableInfo) -> MutationTable {
    MutationTable {
        name: table.name.clone(),
        columns: table
            .columns
            .values()
            .map(|c| (c.name.clone(), c.ty.clone()))
            .collect(),
    }
}

/// V2 reports mutation errors under the field's *name*, not its alias
/// (`$.selectionSet.insert_author.args.objects` for `x: insert_author(…)`).
fn arg_path(field: &SelectedField<'_>, arg: &str) -> String {
    format!("$.selectionSet.{}.args.{arg}", field.name)
}

/// `unexpected null value for type 'T'` unless `value` is null for a non-nullable column.
fn check_null(value: &Json, col: &ColumnInfo, path: &str) -> Result<(), Error> {
    if value.is_null() && !col.nullable {
        return Err(Error::validation(format!(
            "unexpected null value for type '{}'",
            crate::scalars::column_type_ref(&col.ty, true)
        ))
        .with_path(path.to_owned()));
    }
    Ok(())
}

fn quoted_list<'a>(items: impl IntoIterator<Item = &'a String>) -> String {
    items
        .into_iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether an object relationship is inserted before its parent row (the parent references
/// it) or after (it references the parent).
fn inserted_before_parent(table: &TableInfo, rel: &RelationshipInfo) -> bool {
    let Some(meta) = table
        .entry
        .object_relationships
        .iter()
        .find(|r| r.name == rel.name)
    else {
        return true;
    };
    match &meta.using {
        ObjectRelUsing::ForeignKey {
            foreign_key_constraint_on: ObjectFkOn::Remote(_),
        } => false,
        ObjectRelUsing::ForeignKey { .. } => true,
        ObjectRelUsing::Manual {
            manual_configuration,
        } => manual_configuration.insertion_order.as_deref() != Some("after_parent"),
    }
}

impl MutationLowerer<'_, '_> {
    fn access(&self, table: &TableInfo) -> Result<&MutationAccess, Error> {
        self.lw
            .rs
            .mutation_access
            .get(&(table.source.clone(), table.name.clone()))
            .ok_or_else(|| Error::unexpected(format!("no mutation access to {}", table.name)))
    }

    fn insert_access(&self, table: &TableInfo) -> Result<&InsertAccess, Error> {
        self.access(table)?
            .insert
            .as_ref()
            .ok_or_else(|| Error::unexpected(format!("no insert access to {}", table.name)))
    }

    fn update_access(&self, table: &TableInfo) -> Result<&UpdateAccess, Error> {
        self.access(table)?
            .update
            .as_ref()
            .ok_or_else(|| Error::unexpected(format!("no update access to {}", table.name)))
    }

    // --- output ----------------------------------------------------------------------------

    fn output(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
        single: bool,
    ) -> Result<MutationOutput, Error> {
        if single {
            return Ok(MutationOutput::Single(
                self.lw.fields(table, &field.selection)?,
            ));
        }
        let mut out = Vec::with_capacity(field.selection.len());
        for sel in &field.selection {
            let f = match sel.kind {
                Selected::Typename(t) => MutationResponseField::Typename(t.to_owned()),
                Selected::Field(def) => match &def.kind {
                    FieldKind::Mutation(MutationField::AffectedRows) => {
                        MutationResponseField::AffectedRows
                    }
                    FieldKind::Mutation(MutationField::Returning(t)) => {
                        MutationResponseField::Returning(self.lw.fields(t, &sel.selection)?)
                    }
                    other => {
                        return Err(Error::unexpected(format!(
                            "unexpected field kind {other:?} in a mutation response"
                        )));
                    }
                },
            };
            out.push((sel.alias.clone(), f));
        }
        Ok(MutationOutput::Multi(out))
    }

    fn where_arg(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
        name: &str,
    ) -> Result<Option<BoolExp>, Error> {
        match field.arg(name) {
            Some(Json::Object(obj)) => self.lw.bool_exp(table, obj, &arg_path(field, name)),
            _ => Ok(None),
        }
    }

    /// Equality on every primary key column, with values from `obj`.
    fn pk_filter(
        &self,
        table: &TableInfo,
        obj: &Map<String, Json>,
        path: &str,
    ) -> Result<BoolExp, Error> {
        let pk = table
            .primary_key
            .as_ref()
            .ok_or_else(|| Error::unexpected("by_pk field on table without primary key"))?;
        let mut conds = Vec::with_capacity(pk.len());
        for col in pk {
            let col = &table.columns[col];
            let col_path = format!("{path}.{}", col.gql_name);
            let value = obj.get(&col.gql_name).ok_or_else(|| {
                Error::validation(format!("missing required field '{}'", col.gql_name))
                    .with_path(col_path.clone())
            })?;
            conds.push(BoolExp::Compare {
                column: col.name.clone(),
                ty: col.ty.clone(),
                op: CompareOp::Eq(to_value(value, &col.ty, &col_path)?),
            });
        }
        Ok(BoolExp::And(conds))
    }

    // --- insert ----------------------------------------------------------------------------

    fn insert(
        &self,
        table: &Arc<TableInfo>,
        field: &SelectedField<'_>,
        one: bool,
    ) -> Result<Mutation, Error> {
        let output = self.output(table, field, one)?;
        let (arg, objects): (&str, Vec<&Json>) = if one {
            ("object", field.arg("object").into_iter().collect())
        } else {
            match field.arg("objects") {
                Some(Json::Array(items)) => ("objects", items.iter().collect()),
                _ => ("objects", vec![]),
            }
        };
        let path = arg_path(field, arg);
        let on_conflict = field.arg("on_conflict").filter(|v| !v.is_null());
        let into = self.insert_into(
            table,
            &objects,
            on_conflict,
            &path,
            &arg_path(field, "on_conflict"),
            !one,
            &[],
        )?;
        Ok(Mutation::Insert(Box::new(Insert { into, output })))
    }

    /// Rows for `table`. `indexed`: object paths get `[i]`. `parent_columns` are set by the
    /// parent row and may not appear in the input.
    #[allow(clippy::too_many_arguments)]
    fn insert_into(
        &self,
        table: &Arc<TableInfo>,
        objects: &[&Json],
        on_conflict: Option<&Json>,
        path: &str,
        on_conflict_path: &str,
        indexed: bool,
        parent_columns: &[String],
    ) -> Result<InsertInto, Error> {
        let ia = self.insert_access(table)?;
        let mut rows = Vec::with_capacity(objects.len());
        for (i, obj) in objects.iter().enumerate() {
            let obj_path = if indexed {
                format!("{path}[{i}]")
            } else {
                path.to_owned()
            };
            let Json::Object(obj) = obj else {
                return Err(Error::validation("expected an object").with_path(obj_path));
            };
            rows.push(self.insert_object(table, ia, obj, obj_path, parent_columns)?);
        }
        let on_conflict = match on_conflict {
            Some(Json::Object(oc)) => Some(self.on_conflict(table, oc, on_conflict_path)?),
            _ => None,
        };
        let update_check = match &on_conflict {
            Some(_) => self
                .access(table)?
                .update
                .as_ref()
                .and_then(|u| u.check.clone()),
            None => None,
        };
        let check = match (&ia.check, update_check) {
            (None, None) => None,
            (insert, update) => Some(InsertCheck {
                insert: insert.clone().unwrap_or_else(BoolExp::always_true),
                update,
            }),
        };
        Ok(InsertInto {
            table: mutation_table(table),
            objects: rows,
            on_conflict,
            check,
            path: path.to_owned(),
        })
    }

    fn insert_object(
        &self,
        table: &Arc<TableInfo>,
        ia: &InsertAccess,
        obj: &Map<String, Json>,
        path: String,
        parent_columns: &[String],
    ) -> Result<InsertObject, Error> {
        let mut columns: Vec<(String, Value)> = vec![];
        let mut before = vec![];
        let mut after = vec![];
        let mut determined_by_parent = vec![];
        for (key, value) in obj {
            let key_path = format!("{path}.{key}");
            if let Some(col) = table.column_by_gql_name(key) {
                if parent_columns.contains(&col.name) {
                    determined_by_parent.push(col.name.clone());
                    continue;
                }
                check_null(value, col, &key_path)?;
                columns.push((col.name.clone(), to_value(value, &col.ty, &key_path)?));
                continue;
            }
            let rel = table.relationship_by_name(key).ok_or_else(|| {
                Error::validation(format!(
                    "field '{key}' not found in type: '{}_insert_input'",
                    table.gql_name
                ))
                .with_path(key_path.clone())
            })?;
            let Json::Object(rel_obj) = value else {
                continue; // null: nothing to insert
            };
            let target = self.lw.target(rel)?;
            let data_path = format!("{key_path}.data");
            let oc = rel_obj.get("on_conflict").filter(|v| !v.is_null());
            let oc_path = format!("{key_path}.on_conflict");
            let is_object = matches!(rel.kind, crate::tables::RelKind::Object);
            if is_object && inserted_before_parent(table, rel) {
                let data: Vec<&Json> = rel_obj.get("data").into_iter().collect();
                let insert =
                    self.insert_into(target, &data, oc, &data_path, &oc_path, false, &[])?;
                before.push(RelationshipInsert {
                    name: rel.name.clone(),
                    mapping: rel.mapping.clone(),
                    insert,
                    path: key_path,
                });
            } else {
                let (data, indexed): (Vec<&Json>, bool) = match rel_obj.get("data") {
                    Some(Json::Array(items)) => (items.iter().collect(), true),
                    Some(other) => (vec![other], false),
                    None => (vec![], true),
                };
                let child_columns: Vec<String> = rel
                    .mapping
                    .iter()
                    .map(|(_, remote)| remote.clone())
                    .collect();
                let insert = self.insert_into(
                    target,
                    &data,
                    oc,
                    &data_path,
                    &oc_path,
                    indexed,
                    &child_columns,
                )?;
                after.push(RelationshipInsert {
                    name: rel.name.clone(),
                    mapping: rel.mapping.clone(),
                    insert,
                    path: key_path,
                });
            }
        }
        if !determined_by_parent.is_empty() {
            return Err(Error::validation(format!(
                "cannot insert {} columns as their values are already being determined by parent insert",
                quoted_list(&determined_by_parent)
            ))
            .with_path(path));
        }
        let mut before_inserts = Vec::with_capacity(before.len());
        for rel in before {
            let given: Vec<String> = rel
                .mapping
                .iter()
                .map(|(local, _)| local.clone())
                .filter(|local| columns.iter().any(|(c, _)| c == local))
                .collect();
            if !given.is_empty() {
                return Err(Error::validation(format!(
                    "cannot insert object relationship \"{}\" as {} column values are already determined",
                    rel.name,
                    quoted_list(&given)
                ))
                .with_path(rel.path));
            }
            before_inserts.push(rel);
        }
        for (c, v) in &ia.presets {
            columns.retain(|(name, _)| name != c);
            columns.push((c.clone(), v.clone()));
        }
        Ok(InsertObject {
            columns,
            before: before_inserts,
            after,
            path,
        })
    }

    fn on_conflict(
        &self,
        table: &TableInfo,
        oc: &Map<String, Json>,
        path: &str,
    ) -> Result<OnConflict, Error> {
        let constraint = oc
            .get("constraint")
            .and_then(Json::as_str)
            .ok_or_else(|| {
                Error::validation("missing required field 'constraint'").with_path(path.to_owned())
            })?
            .to_owned();
        let mut update_columns = vec![];
        if let Some(Json::Array(cols)) = oc.get("update_columns") {
            for c in cols {
                let name = c.as_str().unwrap_or_default();
                let col = table.column_by_gql_name(name).ok_or_else(|| {
                    Error::validation("erroneous column name")
                        .with_path(format!("{path}.update_columns"))
                })?;
                update_columns.push(col.name.clone());
            }
        }
        let user_where = match oc.get("where") {
            Some(Json::Object(w)) => self.lw.bool_exp(table, w, &format!("{path}.where"))?,
            _ => None,
        };
        let (presets, filter) = match (
            update_columns.is_empty(),
            self.access(table)?.update.as_ref(),
        ) {
            (false, Some(ua)) => (
                ua.presets.clone(),
                BoolExp::and(ua.filter.clone(), user_where),
            ),
            _ => (vec![], user_where),
        };
        Ok(OnConflict {
            constraint,
            update_columns,
            presets,
            filter,
        })
    }

    // --- update ----------------------------------------------------------------------------

    fn update(
        &self,
        table: &Arc<TableInfo>,
        field: &SelectedField<'_>,
        by_pk: bool,
    ) -> Result<Update, Error> {
        let args = &field.args;
        let user_filter = if by_pk {
            match field.arg("pk_columns") {
                Some(Json::Object(pk)) => {
                    Some(self.pk_filter(table, pk, &arg_path(field, "pk_columns"))?)
                }
                _ => None,
            }
        } else {
            self.where_arg(table, field, "where")?
        };
        let output = self.output(table, field, by_pk)?;
        let args_path = format!("$.selectionSet.{}.args", field.name);
        self.update_from(table, args, user_filter, output, &args_path)
    }

    fn update_many(
        &self,
        table: &Arc<TableInfo>,
        field: &SelectedField<'_>,
    ) -> Result<Mutation, Error> {
        let output = self.output(table, field, false)?;
        let mut batches = vec![];
        if let Some(Json::Array(items)) = field.arg("updates") {
            let path = arg_path(field, "updates");
            for (i, item) in items.iter().enumerate() {
                let Json::Object(obj) = item else { continue };
                let item_path = format!("{path}[{i}]");
                let user_filter = match obj.get("where") {
                    Some(Json::Object(w)) => {
                        self.lw.bool_exp(table, w, &format!("{item_path}.where"))?
                    }
                    _ => None,
                };
                let args: indexmap::IndexMap<String, Json> =
                    obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                batches.push(self.update_from(
                    table,
                    &args,
                    user_filter,
                    output.clone(),
                    &item_path,
                )?);
            }
        }
        Ok(Mutation::UpdateMany(batches))
    }

    /// An update from the operator arguments in `args` (found at `path`).
    fn update_from(
        &self,
        table: &TableInfo,
        args: &indexmap::IndexMap<String, Json>,
        user_filter: Option<BoolExp>,
        output: MutationOutput,
        path: &str,
    ) -> Result<Update, Error> {
        let ua = self.update_access(table)?;
        let mut set: Vec<(String, UpdateExp)> = vec![];
        let mut duplicates: Vec<String> = vec![];
        for (op, _, _) in super::schema::UPDATE_OPERATORS {
            let Some(Json::Object(cols)) = args.get(*op) else {
                continue;
            };
            for (gql, v) in cols {
                let vpath = format!("{path}.{op}.{gql}");
                let col = table.column_by_gql_name(gql).ok_or_else(|| {
                    Error::validation(format!("field '{gql}' not found")).with_path(vpath.clone())
                })?;
                let text = ColumnType::builtin("text");
                match *op {
                    "_set" | "_inc" | "_append" | "_prepend" => check_null(v, col, &vpath)?,
                    "_delete_elem" if v.is_null() => {
                        return Err(Error::validation("unexpected null value for type 'Int'")
                            .with_path(vpath));
                    }
                    _ => {}
                }
                let exp = match *op {
                    "_set" => UpdateExp::Set(to_value(v, &col.ty, &vpath)?),
                    "_inc" => UpdateExp::Inc(to_value(v, &col.ty, &vpath)?),
                    "_append" => UpdateExp::Append(to_value(v, &col.ty, &vpath)?),
                    "_prepend" => UpdateExp::Prepend(to_value(v, &col.ty, &vpath)?),
                    "_delete_key" => UpdateExp::DeleteKey(to_value(v, &text, &vpath)?),
                    "_delete_elem" => {
                        UpdateExp::DeleteElem(to_value(v, &ColumnType::builtin("int4"), &vpath)?)
                    }
                    "_delete_at_path" => {
                        UpdateExp::DeleteAtPath(to_value(v, &text.array_of(), &vpath)?)
                    }
                    _ => unreachable!("listed operators"),
                };
                if set.iter().any(|(c, _)| c == &col.name) {
                    duplicates.push(col.gql_name.clone());
                }
                set.push((col.name.clone(), exp));
            }
        }
        for (c, _) in &ua.presets {
            if set.iter().any(|(name, _)| name == c) {
                duplicates.push(table.columns.get(c).map_or(c, |col| &col.gql_name).clone());
            }
        }
        if !duplicates.is_empty() {
            duplicates.sort();
            duplicates.dedup();
            let list: Vec<String> = duplicates.iter().map(|c| format!("'{c}'")).collect();
            return Err(Error::validation(format!(
                "Column found in multiple operators: [{}].",
                list.join(", ")
            ))
            .with_path(path.to_owned()));
        }
        for (c, v) in &ua.presets {
            set.push((c.clone(), UpdateExp::Set(v.clone())));
        }
        Ok(Update {
            table: mutation_table(table),
            set,
            filter: BoolExp::and(ua.filter.clone(), user_filter)
                .unwrap_or_else(BoolExp::always_true),
            check: ua.check.clone(),
            output,
        })
    }

    // --- delete ----------------------------------------------------------------------------

    fn delete(
        &self,
        table: &Arc<TableInfo>,
        field: &SelectedField<'_>,
        by_pk: bool,
    ) -> Result<Mutation, Error> {
        let da = self
            .access(table)?
            .delete
            .as_ref()
            .ok_or_else(|| Error::unexpected(format!("no delete access to {}", table.name)))?;
        let user_filter = if by_pk {
            let args: Map<String, Json> = field
                .args
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            Some(self.pk_filter(table, &args, &format!("$.selectionSet.{}.args", field.name))?)
        } else {
            self.where_arg(table, field, "where")?
        };
        Ok(Mutation::Delete(Box::new(Delete {
            table: mutation_table(table),
            filter: BoolExp::and(da.filter.clone(), user_filter)
                .unwrap_or_else(BoolExp::always_true),
            output: self.output(table, field, by_pk)?,
        })))
    }
}
