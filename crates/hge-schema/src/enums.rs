//! Enum tables (`is_enum: true`).
//!
//! An enum table has a single-column `text` primary key and at most one other (`text`)
//! column holding descriptions; its rows become the values of a GraphQL enum type
//! `<table>_enum`, which replaces `String` for every column with a foreign key to the table.
//! The table itself stays an ordinary table. Validation follows V2's
//! `fetchAndValidateEnumValues` (`Backends/Postgres/DDL/Table.hs`), the type V2's `enumParser`.

use std::collections::HashMap;
use std::sync::Arc;

use hge_core::{EnumTableRow, InconsistentObject, QualifiedTable, SourceCatalog, TableCatalog};
use indexmap::IndexMap;

use crate::build::Builder;
use crate::model::{EnumDef, EnumValueDef, InputObjectDef, InputValueDef, TypeDef, TypeRef};
use crate::scalars;
use crate::tables::{ColumnInfo, TableInfo};

/// The enum type a column takes because it references an enum table.
#[derive(Debug)]
pub struct EnumReference {
    pub table: QualifiedTable,
    /// `<table GraphQL name>_enum`.
    pub type_name: String,
    /// The table's rows sorted by value.
    pub values: Vec<EnumTableRow>,
}

/// The shape of a table marked `is_enum`, checked before its rows are read.
#[derive(Debug, Default)]
pub struct EnumColumns {
    /// The primary key column, when it is a single `text` column.
    pub key: Option<String>,
    /// The comment column, when there is exactly one other column and it is `text`.
    pub comment: Option<String>,
    errors: Vec<String>,
}

fn is_text(col: &hge_core::ColumnInfo) -> bool {
    col.ty.kind == hge_core::TypeKind::Base && !col.ty.is_array() && col.ty.name == "text"
}

fn type_mismatch(what: &str, col: &hge_core::ColumnInfo) -> String {
    let ty = if col.ty.is_array() {
        format!("{}[]", scalars::canonical(&col.ty.name))
    } else {
        scalars::canonical(&col.ty.name).to_owned()
    };
    format!(
        "the table’s {what} (\"{}\") must have type \"text\", not type \"{ty}\"",
        col.name
    )
}

fn sorted_list(names: impl IntoIterator<Item = String>) -> String {
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    names.join(", ")
}

/// Check the columns of an enum table.
pub fn enum_table_columns(cat: &TableCatalog) -> EnumColumns {
    let mut out = EnumColumns::default();
    match cat.primary_key.as_deref() {
        None | Some([]) => out
            .errors
            .push("the table must have a primary key".to_owned()),
        Some([key]) => match cat.column(key) {
            Some(col) if is_text(col) => out.key = Some(key.clone()),
            Some(col) => out.errors.push(type_mismatch("primary key", col)),
            None => out
                .errors
                .push("the table must have a primary key".to_owned()),
        },
        Some(keys) => out.errors.push(format!(
            "the table’s primary key must not span multiple columns ({})",
            sorted_list(keys.iter().cloned())
        )),
    }
    // Without a usable key every column counts as an "other" column, as in V2.
    let others: Vec<_> = cat
        .columns
        .values()
        .filter(|c| out.key.as_deref() != Some(c.name.as_str()))
        .collect();
    match others.as_slice() {
        [] => {}
        [col] if is_text(col) => out.comment = Some(col.name.clone()),
        [col] => out.errors.push(type_mismatch("comment column", col)),
        cols => out.errors.push(format!(
            "the table must have exactly one primary key and optionally one comment column, not {} columns ({})",
            cols.len(),
            sorted_list(cols.iter().map(|c| c.name.clone()))
        )),
    }
    out
}

/// V2's `mkValidEnumValueName`.
fn is_valid_value(v: &str) -> bool {
    crate::tables::is_valid_name(v) && !matches!(v, "true" | "false" | "null")
}

fn invalid_values_reason(values: &[&str]) -> String {
    let mut sorted: Vec<&str> = values.to_vec();
    sorted.sort();
    let plural = " are not valid GraphQL enum value names";
    match sorted.as_slice() {
        [one] => format!("the value \"{one}\" is not a valid GraphQL enum value name"),
        [a, b] => format!("the values \"{a}\" and \"{b}\"{plural}"),
        [rest @ .., last] => format!("the values {}, and \"{last}\"{plural}", rest.join(", ")),
        [] => String::new(),
    }
}

/// The values of a table tracked with `is_enum`, sorted, or why it cannot be an enum table.
pub(crate) fn resolve_enum_values(
    table: &QualifiedTable,
    cat: &TableCatalog,
    catalog: &SourceCatalog,
) -> Result<Vec<EnumTableRow>, String> {
    let shape = enum_table_columns(cat);
    let mut errors = shape.errors;
    let mut values = vec![];
    if shape.key.is_some() {
        match catalog.enum_tables.get(table) {
            Some(Ok(rows)) => {
                if rows.is_empty() {
                    errors.push("the table must have at least one row".to_owned());
                }
                let invalid: Vec<&str> = rows
                    .iter()
                    .map(|r| r.value.as_str())
                    .filter(|v| !is_valid_value(v))
                    .collect();
                if !invalid.is_empty() {
                    errors.push(invalid_values_reason(&invalid));
                }
                values = rows.clone();
            }
            Some(Err(e)) => errors.push(format!("postgres error: {e}")),
            None => errors.push("postgres error: the table's rows were not read".to_owned()),
        }
    }
    match errors.as_slice() {
        [] => {
            values.sort_by(|a, b| a.value.cmp(&b.value));
            values.dedup_by(|a, b| a.value == b.value);
            Ok(values)
        }
        [one] => Err(format!(
            "the table \"{}\" cannot be used as an enum because {one}",
            table.v2_text()
        )),
        many => Err(format!(
            "the table \"{}\" cannot be used as an enum for the following reasons:\n{}",
            table.v2_text(),
            many.iter()
                .map(|e| format!("  • {e}\n"))
                .collect::<String>()
        )),
    }
}

/// Give columns with a foreign key to an enum table of the same source that table's enum
/// type. A column referencing several enum tables makes its table inconsistent (as in V2).
pub(crate) fn attach_enum_references(
    source: &str,
    infos: &mut Vec<TableInfo>,
    catalog: &SourceCatalog,
    inconsistencies: &mut Vec<InconsistentObject>,
) {
    let enums: HashMap<QualifiedTable, Arc<EnumReference>> = infos
        .iter()
        .filter_map(|t| {
            let values = t.enum_values.clone()?;
            Some((
                t.name.clone(),
                Arc::new(EnumReference {
                    table: t.name.clone(),
                    type_name: format!("{}_enum", t.gql_name),
                    values,
                }),
            ))
        })
        .collect();
    if enums.is_empty() {
        return;
    }
    infos.retain_mut(|info| {
        let Some(cat) = catalog.table(&info.name) else {
            return true;
        };
        for col in info.columns.values_mut() {
            let mut refs: Vec<&Arc<EnumReference>> = cat
                .foreign_keys
                .iter()
                .filter(|fk| matches!(fk.columns.as_slice(), [(c, _)] if *c == col.name))
                .filter_map(|fk| enums.get(&fk.ref_table))
                .collect();
            refs.dedup_by(|a, b| a.table == b.table);
            match refs.as_slice() {
                [] => {}
                [one] => col.enum_ref = Some((*one).clone()),
                many => {
                    inconsistencies.push(InconsistentObject::table(
                        source,
                        &info.name,
                        format!(
                            "column \"{}\" in table \"{}\" references multiple enum tables ({})",
                            col.name,
                            info.name.v2_text(),
                            many.iter()
                                .map(|r| format!("\"{}\"", r.table.v2_text()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        serde_json::to_value(&info.name).unwrap_or_default(),
                    ));
                    return false;
                }
            }
        }
        true
    });
}

impl Builder<'_> {
    /// The GraphQL type of a column's values: its enum table's enum type, or its scalar.
    pub(crate) fn column_value_type(&mut self, col: &ColumnInfo, nullable: bool) -> TypeRef {
        match &col.enum_ref {
            Some(r) => {
                self.ensure_enum_type(r);
                let ty = TypeRef::named(r.type_name.clone());
                if nullable { ty } else { ty.non_null() }
            }
            None => {
                self.note_scalar(&col.ty);
                scalars::column_type_ref(&col.ty, nullable)
            }
        }
    }

    /// The comparison expression type of a column; returns its name.
    pub(crate) fn column_comparison_exp(&mut self, col: &ColumnInfo) -> String {
        let Some(r) = &col.enum_ref else {
            return self.ensure_comparison_exp(&col.ty);
        };
        let name = format!("{}_comparison_exp", r.type_name);
        if self.schema.types.contains_key(&name) {
            return name;
        }
        self.ensure_enum_type(r);
        // V2 offers only the equality operators on enum values.
        let value = TypeRef::named(r.type_name.clone());
        let list = TypeRef::nn(r.type_name.clone()).list();
        let fields = [
            InputValueDef::new("_eq", value.clone()),
            InputValueDef::new("_in", list.clone()),
            InputValueDef::new("_is_null", TypeRef::named("Boolean")),
            InputValueDef::new("_neq", value),
            InputValueDef::new("_nin", list),
        ];
        self.schema.add(TypeDef::InputObject(InputObjectDef {
            name: name.clone(),
            description: Some(format!(
                "Boolean expression to compare columns of type \"{}\". All fields are combined with logical 'AND'.",
                r.type_name
            )),
            fields: fields.into_iter().map(|f| (f.name.clone(), f)).collect(),
        }));
        name
    }

    fn ensure_enum_type(&mut self, r: &EnumReference) {
        if self.schema.types.contains_key(&r.type_name) {
            return;
        }
        let values: IndexMap<String, EnumValueDef> = r
            .values
            .iter()
            .map(|v| {
                (
                    v.value.clone(),
                    EnumValueDef {
                        name: v.value.clone(),
                        description: v.comment.clone(),
                        deprecation_reason: None,
                    },
                )
            })
            .collect();
        self.schema.add(TypeDef::Enum(EnumDef {
            name: r.type_name.clone(),
            description: None,
            values,
        }));
    }
}

#[cfg(test)]
mod tests {
    use hge_core::{ColumnType, TableKind, TableMutability};

    use super::*;

    fn catalog(columns: &[(&str, &str)], key: &[&str]) -> TableCatalog {
        let columns = columns
            .iter()
            .map(|(name, ty)| {
                let col = hge_core::ColumnInfo {
                    name: (*name).to_owned(),
                    ty: ColumnType::builtin(ty),
                    nullable: false,
                    has_default: false,
                    is_identity: false,
                    is_identity_always: false,
                    is_generated: false,
                    description: None,
                };
                ((*name).to_owned(), col)
            })
            .collect();
        TableCatalog {
            name: QualifiedTable::new("public", "t"),
            kind: TableKind::Table,
            description: None,
            columns,
            primary_key: Some(key.iter().map(|k| (*k).to_owned()).collect()),
            unique_constraints: vec![],
            constraints: vec![],
            mutability: TableMutability::ALL,
            foreign_keys: vec![],
        }
    }

    fn resolve(cat: &TableCatalog, rows: &[&str]) -> Result<Vec<EnumTableRow>, String> {
        let mut source = SourceCatalog::default();
        let rows = rows
            .iter()
            .map(|v| EnumTableRow {
                value: (*v).to_owned(),
                comment: None,
            })
            .collect();
        source.enum_tables.insert(cat.name.clone(), Ok(rows));
        resolve_enum_values(&cat.name, cat, &source)
    }

    #[test]
    fn shape_errors_like_v2() {
        let employees = catalog(
            &[
                ("id", "int4"),
                ("name", "text"),
                ("favorite_color", "text"),
                ("gets_paid_on", "text"),
            ],
            &["id"],
        );
        assert_eq!(
            resolve(&employees, &[]).unwrap_err(),
            "the table \"t\" cannot be used as an enum for the following reasons:\n  \
             • the table’s primary key (\"id\") must have type \"text\", not type \"integer\"\n  \
             • the table must have exactly one primary key and optionally one comment column, \
             not 4 columns (favorite_color, gets_paid_on, id, name)\n"
        );
        let colors = catalog(&[("value", "text"), ("comment", "text")], &["value"]);
        assert_eq!(
            resolve(&colors, &["red", "a+b"]).unwrap_err(),
            "the table \"t\" cannot be used as an enum because the value \"a+b\" is not a valid GraphQL enum value name"
        );
        assert_eq!(
            resolve(&colors, &[]).unwrap_err(),
            "the table \"t\" cannot be used as an enum because the table must have at least one row"
        );
        let values = resolve(&colors, &["red", "blue"]).unwrap();
        assert_eq!(values[0].value, "blue");
        let shape = enum_table_columns(&colors);
        assert_eq!(shape.comment.as_deref(), Some("comment"));
    }

    #[test]
    fn invalid_value_messages_like_v2() {
        assert_eq!(
            invalid_values_reason(&["a+b"]),
            "the value \"a+b\" is not a valid GraphQL enum value name"
        );
        assert_eq!(
            invalid_values_reason(&["z z", "a-a"]),
            "the values \"a-a\" and \"z z\" are not valid GraphQL enum value names"
        );
        assert_eq!(
            invalid_values_reason(&["c-", "a-", "b-"]),
            "the values a-, b-, and \"c-\" are not valid GraphQL enum value names"
        );
        assert!(!is_valid_value("true") && !is_valid_value("1a") && is_valid_value("red"));
    }
}
