//! Enum tables (`is_enum: true`).
//!
//! An enum table has a single-column `text` primary key and at most one other (`text`)
//! column holding descriptions; its rows become the values of a GraphQL enum type
//! `<table>_enum`, which replaces `String` for every column with a foreign key to the table.
//! The table itself stays an ordinary table. Validation follows V2's
//! `fetchAndValidateEnumValues` (`Backends/Postgres/DDL/Table.hs`), the type V2's `enumParser`.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use hge_core::{EnumTableRow, InconsistentObject, QualifiedTable, SourceCatalog, TableCatalog};
use indexmap::IndexMap;

use crate::build::Builder;
use crate::model::{EnumDef, EnumValueDef, InputObjectDef, InputValueDef, TypeDef, TypeRef};
use crate::scalars;
use crate::tables::{ColumnInfo, TableInfo, Tables};

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

/// Check the columns of an enum table. The columns to read agree with
/// [`TableCatalog::enum_columns`], which this explains.
pub fn enum_table_columns(cat: &TableCatalog) -> EnumColumns {
    let mut out = EnumColumns::default();
    if let Some((key, comment)) = cat.enum_columns() {
        out.key = Some(key.to_owned());
        out.comment = comment.map(str::to_owned);
    }
    match cat.primary_key.as_deref() {
        None | Some([]) => out
            .errors
            .push("the table must have a primary key".to_owned()),
        Some([key]) => match cat.column(key) {
            Some(col) if is_text(col) => {}
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
        [col] if is_text(col) => {}
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

/// Enum tables by source and name, with their primary key column.
type EnumTables = HashMap<(Arc<str>, QualifiedTable), (String, Arc<EnumReference>)>;

/// Give columns with a foreign key to the primary key of an enum table of the same source
/// that table's enum type (V2's `resolveEnumReferences`). A column referencing several enum
/// tables makes its table inconsistent (as in V2). Runs after tables were dropped for other
/// reasons, and again whenever it drops an enum table itself, so no column keeps the type
/// of a table that is not in the schema.
pub(crate) fn attach_enum_references<C: Borrow<SourceCatalog>>(
    tables: &mut Tables,
    catalogs: &HashMap<String, C>,
    inconsistencies: &mut Vec<InconsistentObject>,
) {
    loop {
        let enums: EnumTables = tables
            .ordered
            .iter()
            .filter_map(|t| {
                let values = t.enum_values.clone()?;
                let [key] = t.primary_key.as_deref()? else {
                    return None;
                };
                let reference = Arc::new(EnumReference {
                    table: t.name.clone(),
                    type_name: format!("{}_enum", t.gql_name),
                    values,
                });
                Some(((t.source.clone(), t.name.clone()), (key.clone(), reference)))
            })
            .collect();
        let mut dropped_enum_table = false;
        let mut kept = Vec::with_capacity(tables.ordered.len());
        for table in std::mem::take(&mut tables.ordered) {
            let cat = catalogs
                .get(&*table.source)
                .and_then(|c| c.borrow().table(&table.name));
            match column_references(&table, cat, &enums) {
                Ok(refs) => {
                    let changed = table.columns.values().any(|c| {
                        let new = refs.get(&c.name);
                        match (&c.enum_ref, new) {
                            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                            (a, b) => a.is_some() != b.is_some(),
                        }
                    });
                    if !changed {
                        kept.push(table);
                        continue;
                    }
                    let mut info = (*table).clone();
                    for col in info.columns.values_mut() {
                        col.enum_ref = refs.get(&col.name).cloned();
                    }
                    let info = Arc::new(info);
                    tables
                        .by_name
                        .insert((info.source.clone(), info.name.clone()), info.clone());
                    kept.push(info);
                }
                Err(reason) => {
                    dropped_enum_table |= table.enum_values.is_some();
                    inconsistencies.push(InconsistentObject::table(
                        &table.source,
                        &table.name,
                        reason,
                        serde_json::to_value(&table.name).unwrap_or_default(),
                    ));
                    tables
                        .by_name
                        .remove(&(table.source.clone(), table.name.clone()));
                }
            }
        }
        tables.ordered = kept;
        if !dropped_enum_table {
            break;
        }
    }
}

/// The enum table each column of `table` references, by column name.
fn column_references(
    table: &TableInfo,
    cat: Option<&TableCatalog>,
    enums: &EnumTables,
) -> Result<HashMap<String, Arc<EnumReference>>, String> {
    let mut out = HashMap::new();
    let Some(cat) = cat else {
        return Ok(out);
    };
    for col in table.columns.values() {
        let mut refs: Vec<&Arc<EnumReference>> = cat
            .foreign_keys
            .iter()
            .filter_map(|fk| match fk.columns.as_slice() {
                [(local, remote)] if *local == col.name => {
                    let (key, reference) =
                        enums.get(&(table.source.clone(), fk.ref_table.clone()))?;
                    (key == remote).then_some(reference)
                }
                _ => None,
            })
            .collect();
        refs.sort_by(|a, b| a.table.cmp(&b.table));
        refs.dedup_by(|a, b| a.table == b.table);
        match refs.as_slice() {
            [] => {}
            [one] => {
                out.insert(col.name.clone(), (*one).clone());
            }
            many => {
                return Err(format!(
                    "column \"{}\" in table \"{}\" references multiple enum tables ({})",
                    col.name,
                    table.name.v2_text(),
                    many.iter()
                        .map(|r| format!("\"{}\"", r.table.v2_text()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    Ok(out)
}

impl Builder<'_> {
    /// The GraphQL type of a column's values: its enum table's enum type, or its scalar.
    pub(crate) fn column_value_type(&mut self, col: &ColumnInfo, nullable: bool) -> TypeRef {
        match &col.enum_ref {
            Some(r) if self.ensure_enum_type(r) => {
                let ty = TypeRef::named(r.type_name.clone());
                if nullable { ty } else { ty.non_null() }
            }
            _ => {
                self.note_scalar(&col.ty);
                scalars::column_type_ref(&col.ty, nullable)
            }
        }
    }

    /// The comparison expression type of a column; returns its name.
    pub(crate) fn column_comparison_exp(&mut self, col: &ColumnInfo) -> String {
        let Some(r) = col.enum_ref.as_ref().filter(|r| self.ensure_enum_type(r)) else {
            return self.ensure_comparison_exp(&col.ty);
        };
        let name = format!("{}_comparison_exp", r.type_name);
        if self.schema.types.contains_key(&name) {
            return name;
        }
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

    /// Add `r`'s enum type; whether the schema's type of that name is it. Resolution drops
    /// tables whose names clash with an enum type, so another type is never expected here,
    /// but it must not lend its values to `r`'s columns.
    fn ensure_enum_type(&mut self, r: &EnumReference) -> bool {
        match self.schema.types.get(&r.type_name) {
            Some(TypeDef::Enum(e)) => {
                return e.values.len() == r.values.len()
                    && e.values.keys().zip(&r.values).all(|(k, v)| *k == v.value);
            }
            Some(_) => return false,
            None => {}
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
        }))
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

    /// Resolve `colors` (an enum table), `post` (referencing it) and the tables `extra`, in
    /// that metadata order (`extra` first when `extra_first`); `post`'s enum references.
    fn post_refs(extra: &[&str], extra_first: bool) -> HashMap<String, String> {
        use hge_core::ForeignKey;
        let named = |name: &str, cat: TableCatalog| TableCatalog {
            name: QualifiedTable::new("public", name),
            ..cat
        };
        let colors = named(
            "colors",
            catalog(&[("value", "text"), ("comment", "text")], &["value"]),
        );
        let mut post = named(
            "post",
            catalog(
                &[("id", "int4"), ("color", "text"), ("note", "text")],
                &["id"],
            ),
        );
        let fk = |local: &str, remote: &str| ForeignKey {
            constraint: format!("post_{local}_fkey"),
            ref_table: colors.name.clone(),
            columns: vec![(local.to_owned(), remote.to_owned())],
        };
        // `note` references the comment column, not the key: no enum type.
        post.foreign_keys = vec![fk("color", "value"), fk("note", "comment")];
        let mut source = SourceCatalog::default();
        source.enum_tables.insert(
            colors.name.clone(),
            Ok(vec![EnumTableRow {
                value: "red".into(),
                comment: None,
            }]),
        );
        let mut names = vec!["colors", "post"];
        for e in extra {
            let cat = named(e, catalog(&[("id", "int4")], &["id"]));
            source.tables.insert(cat.name.clone(), cat);
            if extra_first {
                names.insert(0, e);
            } else {
                names.push(e);
            }
        }
        source.tables.insert(colors.name.clone(), colors);
        source.tables.insert(post.name.clone(), post);
        let tables: Vec<_> = names
            .iter()
            .map(|n| {
                let mut t = serde_json::json!({"table": {"schema": "public", "name": n}});
                if *n == "colors" {
                    t["is_enum"] = true.into();
                }
                t
            })
            .collect();
        let metadata = hge_metadata::from_json(serde_json::json!({
            "version": 3,
            "sources": [{
                "name": "default",
                "kind": "postgres",
                "configuration": {"connection_info": {"database_url": "postgres://x"}},
                "tables": tables
            }]
        }))
        .unwrap();
        let catalogs = HashMap::from([("default".to_owned(), source)]);
        let (tables, _) = crate::resolve_tables(&metadata, &catalogs);
        let post = tables
            .get("default", &QualifiedTable::new("public", "post"))
            .unwrap();
        post.columns
            .values()
            .filter_map(|c| Some((c.name.clone(), c.enum_ref.as_ref()?.type_name.clone())))
            .collect()
    }

    #[test]
    fn references_only_through_the_primary_key() {
        let refs = post_refs(&[], false);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs["color"], "colors_enum");
    }

    #[test]
    fn no_references_to_a_dropped_enum_table() {
        // A table named like the enum type, tracked first, wins the name.
        assert!(post_refs(&["colors_enum"], true).is_empty());
        // Tracked after the enum table, it is the one dropped.
        assert_eq!(post_refs(&["colors_enum"], false).len(), 1);
    }
}
