//! `mutation_root` fields and the input / response types they use (V2 names and
//! descriptions, see spec 02 §4.1, §6.7, §6.8, §6.10).

use std::collections::HashMap;
use std::sync::Arc;

use hge_core::{ColumnType, QualifiedTable};
use indexmap::IndexMap;

use super::access::{InsertAccess, MutationAccess, UpdateAccess};
use super::{MutationField, RootNames};
use crate::build::Builder;
use crate::model::*;
use crate::scalars;
use crate::tables::{ColumnInfo, TableInfo};

pub(crate) type AccessMap = HashMap<(Arc<str>, QualifiedTable), MutationAccess>;

/// Update operators: (argument name, description, type name suffix).
pub(crate) const UPDATE_OPERATORS: &[(&str, &str, &str)] = &[
    (
        "_append",
        "append existing jsonb value of filtered columns with new jsonb value",
        "_append_input",
    ),
    (
        "_delete_at_path",
        "delete the field or element with specified path (for JSON arrays, negative integers count from the end)",
        "_delete_at_path_input",
    ),
    (
        "_delete_elem",
        "delete the array element with specified index (negative integers count from the end). throws an error if top level container is not an array",
        "_delete_elem_input",
    ),
    (
        "_delete_key",
        "delete key/value pair or string element. key/value pairs are matched based on their key value",
        "_delete_key_input",
    ),
    (
        "_inc",
        "increments the numeric columns with given value of the filtered values",
        "_inc_input",
    ),
    (
        "_prepend",
        "prepend existing jsonb value of filtered columns with new jsonb value",
        "_prepend_input",
    ),
    (
        "_set",
        "sets the columns of the filtered rows to the given values",
        "_set_input",
    ),
];

/// `isNumType`: columns `_inc` applies to.
pub(crate) fn is_numeric(ty: &ColumnType) -> bool {
    !ty.is_array()
        && matches!(
            ty.name.as_str(),
            "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "money"
        )
}

fn input_object(
    name: String,
    description: Option<String>,
    mut fields: Vec<InputValueDef>,
) -> TypeDef {
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    TypeDef::InputObject(InputObjectDef {
        name,
        description,
        fields: fields.into_iter().map(|f| (f.name.clone(), f)).collect(),
    })
}

fn column_enum_value(name: &str, description: &str) -> (String, EnumValueDef) {
    (
        name.to_owned(),
        EnumValueDef {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            deprecation_reason: None,
        },
    )
}

impl Builder<'_> {
    /// Register the mutation root fields of `table` for the role.
    pub(crate) fn add_table_mutation_root_fields(
        &mut self,
        table: &Arc<TableInfo>,
        access: &AccessMap,
        root: &mut ObjectDef,
    ) {
        let Some(ma) = access.get(&(table.source.clone(), table.name.clone())) else {
            return;
        };
        let names = RootNames::of(table);
        // Fields returning a row need the role's select permission on the table.
        let selectable = self.access(table).is_some_and(|a| !a.columns.is_empty());
        let pk_selectable = selectable
            && table.primary_key.as_ref().is_some_and(|pk| {
                let a = self.access(table).expect("selectable");
                pk.iter().all(|c| a.allows_column(c))
            });
        // `where` arguments use the table's bool_exp, built with the select types. Without
        // a select permission, V2 still offers update and delete with a bool_exp that has
        // only `_and`, `_not` and `_or`.
        if selectable {
            self.ensure_table_types(table);
        } else if ma
            .update
            .as_ref()
            .is_some_and(|_| table.mutability.updatable)
            || ma
                .delete
                .as_ref()
                .is_some_and(|_| table.mutability.insertable)
        {
            self.ensure_bare_bool_exp(table);
        }

        if let Some(ia) = ma.insert.as_ref()
            && table.mutability.insertable
        {
            let input = self.ensure_insert_input(table, ia, access);
            let on_conflict = self.ensure_on_conflict(table, ma);
            let response = self.ensure_mutation_response(table, selectable);
            let mut f = FieldDef::new(
                names.insert.0.clone(),
                TypeRef::named(response),
                FieldKind::Mutation(MutationField::Insert(table.clone())),
            )
            .describe(names.insert.1.clone())
            .arg(
                InputValueDef::new("objects", TypeRef::nn_list_nn(input.clone()))
                    .describe("the rows to be inserted"),
            );
            if let Some(oc) = &on_conflict {
                f = f.arg(
                    InputValueDef::new("on_conflict", TypeRef::named(oc))
                        .describe("upsert condition"),
                );
            }
            root.fields.insert(f.name.clone(), f);

            if selectable {
                let mut f = FieldDef::new(
                    names.insert_one.0.clone(),
                    TypeRef::named(table.type_name()),
                    FieldKind::Mutation(MutationField::InsertOne(table.clone())),
                )
                .describe(names.insert_one.1.clone())
                .arg(
                    InputValueDef::new("object", TypeRef::nn(input))
                        .describe("the row to be inserted"),
                );
                if let Some(oc) = &on_conflict {
                    f = f.arg(
                        InputValueDef::new("on_conflict", TypeRef::named(oc))
                            .describe("upsert condition"),
                    );
                }
                root.fields.insert(f.name.clone(), f);
            }
        }

        if let Some(ua) = ma.update.as_ref()
            && table.mutability.updatable
        {
            let response = self.ensure_mutation_response(table, selectable);
            let ops = self.ensure_update_operators(table, ua);
            let with_ops = |mut f: FieldDef| {
                for (arg, desc, ty) in &ops {
                    f = f.arg(InputValueDef::new(*arg, TypeRef::named(ty.clone())).describe(*desc));
                }
                f
            };
            let where_arg = || {
                InputValueDef::new("where", TypeRef::nn(table.bool_exp_name()))
                    .describe("filter the rows which have to be updated")
            };
            let f = FieldDef::new(
                names.update.0.clone(),
                TypeRef::named(response.clone()),
                FieldKind::Mutation(MutationField::Update(table.clone())),
            )
            .describe(names.update.1.clone())
            .arg(where_arg());
            let f = with_ops(f);
            root.fields.insert(f.name.clone(), f);

            if pk_selectable {
                let pk_input = self.ensure_pk_columns_input(table);
                let f = FieldDef::new(
                    names.update_by_pk.0.clone(),
                    TypeRef::named(table.type_name()),
                    FieldKind::Mutation(MutationField::UpdateByPk(table.clone())),
                )
                .describe(names.update_by_pk.1.clone())
                .arg(InputValueDef::new("pk_columns", TypeRef::nn(pk_input)));
                let f = with_ops(f);
                root.fields.insert(f.name.clone(), f);
            }

            let updates = format!("{}_updates", table.gql_name);
            if !self.schema.types.contains_key(&updates) {
                let mut fields: Vec<InputValueDef> = ops
                    .iter()
                    .map(|(arg, desc, ty)| {
                        InputValueDef::new(*arg, TypeRef::named(ty.clone())).describe(*desc)
                    })
                    .collect();
                fields.push(where_arg());
                self.schema.add(input_object(updates.clone(), None, fields));
            }
            let f = FieldDef::new(
                names.update_many.0.clone(),
                TypeRef::named(response).list(),
                FieldKind::Mutation(MutationField::UpdateMany(table.clone())),
            )
            .describe(names.update_many.1.clone())
            .arg(
                InputValueDef::new("updates", TypeRef::nn_list_nn(updates))
                    .describe("updates to execute, in order"),
            );
            root.fields.insert(f.name.clone(), f);
        }

        // V2 quirk: deletes from views are gated on the view being insertable.
        if ma.delete.is_some() && table.mutability.insertable {
            let response = self.ensure_mutation_response(table, selectable);
            let f = FieldDef::new(
                names.delete.0.clone(),
                TypeRef::named(response),
                FieldKind::Mutation(MutationField::Delete(table.clone())),
            )
            .describe(names.delete.1.clone())
            .arg(
                InputValueDef::new("where", TypeRef::nn(table.bool_exp_name()))
                    .describe("filter the rows which have to be deleted"),
            );
            root.fields.insert(f.name.clone(), f);

            if pk_selectable {
                let mut f = FieldDef::new(
                    names.delete_by_pk.0.clone(),
                    TypeRef::named(table.type_name()),
                    FieldKind::Mutation(MutationField::DeleteByPk(table.clone())),
                )
                .describe(names.delete_by_pk.1.clone());
                for col in table.primary_key.iter().flatten() {
                    let col = &table.columns[col];
                    let ty = self.column_value_type(col, false);
                    f = f.arg(InputValueDef::new(col.gql_name.clone(), ty));
                }
                root.fields.insert(f.name.clone(), f);
            }
        }
    }

    /// `t_bool_exp` with only the logical operators, for a role that cannot select `table`.
    fn ensure_bare_bool_exp(&mut self, table: &TableInfo) {
        let name = table.bool_exp_name();
        if self.schema.types.contains_key(&name) {
            return;
        }
        let fields = vec![
            InputValueDef::new("_and", TypeRef::nn(name.clone()).list()),
            InputValueDef::new("_not", TypeRef::named(name.clone())),
            InputValueDef::new("_or", TypeRef::nn(name.clone()).list()),
        ];
        let description = format!(
            "Boolean expression to filter rows from the table \"{}\". All fields are combined with a logical 'AND'.",
            table.display_name()
        );
        self.schema
            .add(input_object(name, Some(description), fields));
    }

    /// `t_mutation_response`; returns its name.
    fn ensure_mutation_response(&mut self, table: &Arc<TableInfo>, selectable: bool) -> String {
        let name = format!("{}_mutation_response", table.gql_name);
        if self.schema.types.contains_key(&name) {
            return name;
        }
        let mut obj = ObjectDef::new(
            name.clone(),
            Some(format!(
                "response of any mutation on the table \"{}\"",
                table.display_name()
            )),
        )
        .field(
            FieldDef::new(
                "affected_rows",
                TypeRef::nn("Int"),
                FieldKind::Mutation(MutationField::AffectedRows),
            )
            .describe("number of rows affected by the mutation"),
        );
        if selectable {
            obj = obj.field(
                FieldDef::new(
                    "returning",
                    TypeRef::nn_list_nn(table.type_name()),
                    FieldKind::Mutation(MutationField::Returning(table.clone())),
                )
                .describe("data from the rows affected by the mutation"),
            );
        }
        self.schema.add(TypeDef::Object(obj));
        name
    }

    /// `t_insert_input` (and, through relationships, the insert inputs of related tables);
    /// returns its name.
    fn ensure_insert_input(
        &mut self,
        table: &Arc<TableInfo>,
        ia: &InsertAccess,
        access: &AccessMap,
    ) -> String {
        let name = format!("{}_insert_input", table.gql_name);
        if self.schema.types.contains_key(&name) {
            return name;
        }
        // Reserve the name first so recursive relationships terminate.
        self.schema.add(input_object(name.clone(), None, vec![]));

        let mut fields = vec![];
        for col in ia.input_columns(table) {
            let ty = self.column_value_type(col, true);
            fields.push(
                InputValueDef::new(col.gql_name.clone(), ty).describe_opt(col.description.clone()),
            );
        }
        let rels: Vec<_> = table
            .object_relationships
            .iter()
            .chain(table.array_relationships.iter())
            .cloned()
            .collect();
        for rel in rels {
            let Some(target) = self.tables.get(&rel.source, &rel.target).cloned() else {
                continue;
            };
            let Some(target_ia) = access
                .get(&(target.source.clone(), target.name.clone()))
                .and_then(|a| a.insert.as_ref())
            // V2 offers nested inserts into views whether or not Postgres can insert into
            // them (the statement then fails).
            else {
                continue;
            };
            let data = self.ensure_insert_input(&target, target_ia, access);
            let on_conflict = access
                .get(&(target.source.clone(), target.name.clone()))
                .and_then(|ma| self.ensure_on_conflict(&target, ma));
            let (suffix, kind, data_ty) = match rel.kind {
                crate::tables::RelKind::Object => {
                    ("obj_rel_insert_input", "object", TypeRef::nn(data))
                }
                crate::tables::RelKind::Array => {
                    ("arr_rel_insert_input", "array", TypeRef::nn_list_nn(data))
                }
            };
            let rel_input = format!("{}_{suffix}", target.gql_name);
            if !self.schema.types.contains_key(&rel_input) {
                let mut rf = vec![InputValueDef::new("data", data_ty)];
                if let Some(oc) = on_conflict {
                    rf.push(
                        InputValueDef::new("on_conflict", TypeRef::named(oc))
                            .describe("upsert condition"),
                    );
                }
                self.schema.add(input_object(
                    rel_input.clone(),
                    Some(format!(
                        "input type for inserting {kind} relation for remote table \"{}\"",
                        target.display_name()
                    )),
                    rf,
                ));
            }
            fields.push(InputValueDef::new(
                rel.name.clone(),
                TypeRef::named(rel_input),
            ));
        }
        let ty = input_object(
            name.clone(),
            Some(format!(
                "input type for inserting data into table \"{}\"",
                table.display_name()
            )),
            fields,
        );
        self.schema.types.insert(name.clone(), ty);
        name
    }

    /// `t_on_conflict`, `t_constraint` and `t_update_column` when the table has unique
    /// constraints; returns the on_conflict type name.
    fn ensure_on_conflict(
        &mut self,
        table: &Arc<TableInfo>,
        ma: &MutationAccess,
    ) -> Option<String> {
        if table.constraints.is_empty() {
            return None;
        }
        let name = format!("{}_on_conflict", table.gql_name);
        if self.schema.types.contains_key(&name) {
            return Some(name);
        }
        let display = table.display_name();
        let constraint = format!("{}_constraint", table.gql_name);
        let mut values: IndexMap<String, EnumValueDef> = table
            .constraints
            .iter()
            .filter(|c| crate::tables::is_valid_name(&c.name))
            .map(|c| {
                let cols = c
                    .columns
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                column_enum_value(
                    &c.name,
                    &format!("unique or primary key constraint on columns {cols}"),
                )
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        values.sort_keys();
        self.schema.add(TypeDef::Enum(EnumDef {
            name: constraint.clone(),
            description: Some(format!(
                "unique or primary key constraints on table \"{display}\""
            )),
            values,
        }));

        let update_column = self.ensure_update_column_enum(table, ma.update.as_ref());
        let mut fields = vec![
            InputValueDef::new("constraint", TypeRef::nn(constraint)),
            InputValueDef {
                default_value: Some("[]".to_owned()),
                ..InputValueDef::new("update_columns", TypeRef::nn_list_nn(update_column))
            },
        ];
        if self.access(table).is_some() {
            self.ensure_table_types(table);
            fields.push(InputValueDef::new(
                "where",
                TypeRef::named(table.bool_exp_name()),
            ));
        }
        self.schema.add(input_object(
            name.clone(),
            Some(format!(
                "on_conflict condition type for table \"{display}\""
            )),
            fields,
        ));
        Some(name)
    }

    /// `t_update_column`, or its placeholder variant when the role cannot update anything.
    fn ensure_update_column_enum(
        &mut self,
        table: &TableInfo,
        ua: Option<&UpdateAccess>,
    ) -> String {
        let name = format!("{}_update_column", table.gql_name);
        if self.schema.types.contains_key(&name) {
            return name;
        }
        let display = table.display_name();
        let columns: Vec<&ColumnInfo> = ua
            .map(|a| a.input_columns(table).collect())
            .unwrap_or_default();
        let def = if columns.is_empty() {
            EnumDef {
                name: name.clone(),
                description: Some(format!(
                    "placeholder for update columns of table \"{display}\" (current role has no relevant permissions)"
                )),
                values: [column_enum_value(
                    "_PLACEHOLDER",
                    "placeholder (do not use)",
                )]
                .into_iter()
                .collect(),
            }
        } else {
            let mut values: IndexMap<String, EnumValueDef> = columns
                .iter()
                .map(|c| column_enum_value(&c.gql_name, "column name"))
                .collect();
            values.sort_keys();
            EnumDef {
                name: name.clone(),
                description: Some(format!("update columns of table \"{display}\"")),
                values,
            }
        };
        self.schema.add(TypeDef::Enum(def));
        name
    }

    /// The operator input types the role can use; returns `(argument, description, type)`.
    fn ensure_update_operators(
        &mut self,
        table: &TableInfo,
        ua: &UpdateAccess,
    ) -> Vec<(&'static str, &'static str, String)> {
        let columns: Vec<ColumnInfo> = ua.input_columns(table).cloned().collect();
        let display = table.display_name();
        let mut out = vec![];
        for (arg, desc, suffix) in UPDATE_OPERATORS {
            let applicable: Vec<&ColumnInfo> = columns
                .iter()
                .filter(|c| match *arg {
                    "_set" => true,
                    "_inc" => is_numeric(&c.ty),
                    _ => scalars::is_jsonb(&c.ty),
                })
                .collect();
            if applicable.is_empty() {
                continue;
            }
            let name = format!("{}{suffix}", table.gql_name);
            if !self.schema.types.contains_key(&name) {
                let fields = applicable
                    .iter()
                    .map(|c| {
                        let ty = match *arg {
                            "_delete_key" => TypeRef::named("String"),
                            "_delete_elem" => TypeRef::named("Int"),
                            "_delete_at_path" => TypeRef::nn("String").list(),
                            _ => self.column_value_type(c, true),
                        };
                        InputValueDef::new(c.gql_name.clone(), ty)
                            .describe_opt(c.description.clone())
                    })
                    .collect();
                let description = match *arg {
                    "_set" => format!("input type for updating data in table \"{display}\""),
                    "_inc" => format!(
                        "input type for incrementing numeric columns in table \"{display}\""
                    ),
                    _ => (*desc).to_owned(),
                };
                self.schema
                    .add(input_object(name.clone(), Some(description), fields));
            }
            out.push((*arg, *desc, name));
        }
        out
    }

    /// `t_pk_columns_input`; returns its name.
    fn ensure_pk_columns_input(&mut self, table: &TableInfo) -> String {
        let name = format!("{}_pk_columns_input", table.gql_name);
        if self.schema.types.contains_key(&name) {
            return name;
        }
        let mut fields = vec![];
        for col in table.primary_key.iter().flatten() {
            let col = &table.columns[col];
            let ty = self.column_value_type(col, false);
            fields.push(
                InputValueDef::new(col.gql_name.clone(), ty).describe_opt(col.description.clone()),
            );
        }
        self.schema.add(input_object(
            name.clone(),
            Some(format!(
                "primary key columns input for table: {}",
                table.display_name()
            )),
            fields,
        ));
        name
    }
}
