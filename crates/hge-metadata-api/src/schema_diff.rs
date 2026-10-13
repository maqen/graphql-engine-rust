//! Apply database schema changes made by `run_sql` to the metadata, like V2: renamed tables
//! and columns are renamed in the metadata, dropped ones are removed, and objects depending
//! on dropped ones block the SQL unless `cascade` is set.

use std::collections::BTreeMap;

use hge_core::{Code, QualifiedTable};
use hge_metadata::{
    ArrayRelUsing, Columns, Metadata, ObjectFkOn, ObjectRelUsing, RemoteFk, TableEntry,
};
use serde_json::Value;

use crate::response::{ApiError, ApiResult};

/// A tracked table as seen in `pg_catalog`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSnapshot {
    pub oid: u32,
    pub table: QualifiedTable,
    /// attnum → column name
    pub columns: BTreeMap<i16, String>,
    /// Foreign keys from this table: (referenced table oid, attnums of the local columns).
    pub foreign_keys: Vec<(u32, Vec<i16>)>,
}

/// Schema of temporary table names while renaming (cannot clash with a real identifier).
const TEMPORARY: &str = "\u{0}rename";

fn temporary(i: usize) -> String {
    format!("\u{0}rename{i}")
}

/// A metadata object that depends on something being dropped.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Dependent {
    Relationship {
        table: QualifiedTable,
        name: String,
    },
    Permission {
        table: QualifiedTable,
        role: String,
        kind: &'static str,
    },
}

impl Dependent {
    fn report(&self, source: &str) -> String {
        match self {
            Dependent::Relationship { table, name } => format!(
                "relationship {}.{name} in source \"{source}\"",
                table.v2_text()
            ),
            Dependent::Permission { table, role, kind } => format!(
                "permission {}.{role}.{kind} in source \"{source}\"",
                table.v2_text()
            ),
        }
    }
}

/// Update `metadata` for the changes between `before` and `after` (matched by oid).
pub fn apply(
    metadata: &mut Metadata,
    source: &str,
    before: &[TableSnapshot],
    after: &[TableSnapshot],
    cascade: bool,
) -> ApiResult<()> {
    let Some(src) = metadata.source_mut(source) else {
        return Ok(());
    };
    let mut dependents = vec![];
    let mut dropped_tables = vec![];
    for b in before {
        let a = after.iter().find(|a| a.oid == b.oid);
        // Relationships defined through a foreign key that is gone (dropped with its own
        // table, the table it references, or by itself).
        for (ref_oid, attnums) in &b.foreign_keys {
            let kept = a.is_some_and(|a| {
                a.foreign_keys
                    .iter()
                    .any(|(r, n)| r == ref_oid && n == attnums)
            });
            if kept {
                continue;
            }
            let Some(referenced) = before.iter().find(|t| t.oid == *ref_oid) else {
                continue;
            };
            let columns: Vec<&str> = attnums
                .iter()
                .filter_map(|n| b.columns.get(n).map(String::as_str))
                .collect();
            dependents.extend(fk_dependents(
                &src.tables,
                &b.table,
                &referenced.table,
                &columns,
            ));
        }
        match a {
            None => {
                dependents.extend(table_dependents(&src.tables, &b.table));
                dropped_tables.push(b.table.clone());
            }
            Some(a) => {
                for (attnum, old) in &b.columns {
                    if !a.columns.contains_key(attnum) {
                        dependents.extend(column_dependents(&src.tables, &b.table, old));
                    }
                }
            }
        }
    }
    // Objects of dropped tables go with them.
    dependents.retain(|d| match d {
        Dependent::Relationship { table, .. } | Dependent::Permission { table, .. } => {
            !dropped_tables.contains(table)
        }
    });
    dependents.sort();
    dependents.dedup();
    if !dependents.is_empty() && !cascade {
        let list: Vec<String> = dependents.iter().map(|d| d.report(source)).collect();
        return Err(ApiError::bad(
            Code::DependencyError,
            format!(
                "cannot drop due to the following dependent objects: {}",
                list.join(", ")
            ),
        ));
    }
    for d in &dependents {
        drop_dependent(&mut src.tables, d);
    }
    src.tables.retain(|t| !dropped_tables.contains(&t.table));

    // Everything above used the old names; now rename. Columns first (they are looked up
    // by their table's old name), then tables. Each goes through a unique temporary name so
    // that swaps (`a → tmp, b → a, tmp → b`) don't merge two objects.
    let renamed: Vec<(&TableSnapshot, &TableSnapshot)> = before
        .iter()
        .filter_map(|b| after.iter().find(|a| a.oid == b.oid).map(|a| (b, a)))
        .collect();
    for (b, a) in &renamed {
        let columns: Vec<(&String, &String)> = b
            .columns
            .iter()
            .filter_map(|(n, old)| {
                a.columns
                    .get(n)
                    .filter(|new| *new != old)
                    .map(|new| (old, new))
            })
            .collect();
        for (i, (old, _)) in columns.iter().enumerate() {
            rename_column(&mut src.tables, &b.table, old, &temporary(i));
        }
        for (i, (_, new)) in columns.iter().enumerate() {
            rename_column(&mut src.tables, &b.table, &temporary(i), new);
        }
    }
    let tables: Vec<_> = renamed.iter().filter(|(b, a)| a.table != b.table).collect();
    for (i, (b, _)) in tables.iter().enumerate() {
        rename_table(
            &mut src.tables,
            &b.table,
            &QualifiedTable::new(TEMPORARY, temporary(i)),
        );
    }
    for (i, (_, a)) in tables.iter().enumerate() {
        rename_table(
            &mut src.tables,
            &QualifiedTable::new(TEMPORARY, temporary(i)),
            &a.table,
        );
    }
    Ok(())
}

fn obj_rel_target(using: &ObjectRelUsing) -> Option<&QualifiedTable> {
    match using {
        ObjectRelUsing::ForeignKey {
            foreign_key_constraint_on: ObjectFkOn::Remote(r),
        } => Some(&r.table),
        ObjectRelUsing::ForeignKey { .. } => None,
        ObjectRelUsing::Manual {
            manual_configuration,
        } => Some(&manual_configuration.remote_table),
    }
}

fn arr_rel_target(using: &ArrayRelUsing) -> &QualifiedTable {
    match using {
        ArrayRelUsing::ForeignKey {
            foreign_key_constraint_on,
        } => &foreign_key_constraint_on.table,
        ArrayRelUsing::Manual {
            manual_configuration,
        } => &manual_configuration.remote_table,
    }
}

/// Relationships of other tables that point at `dropped`. (Forward fk object relationships
/// lose their constraint with the table, so they are dependents too, but V2 finds those
/// through the constraint; we look at columns, which covers the manual and reverse forms.)
fn table_dependents(tables: &[TableEntry], dropped: &QualifiedTable) -> Vec<Dependent> {
    let mut out = vec![];
    for t in tables.iter().filter(|t| t.table != *dropped) {
        for r in &t.object_relationships {
            if obj_rel_target(&r.using) == Some(dropped) {
                out.push(Dependent::Relationship {
                    table: t.table.clone(),
                    name: r.name.clone(),
                });
            }
        }
        for r in &t.array_relationships {
            if arr_rel_target(&r.using) == dropped {
                out.push(Dependent::Relationship {
                    table: t.table.clone(),
                    name: r.name.clone(),
                });
            }
        }
    }
    out
}

/// Does a boolean expression mention `column` at this table's level?
fn bool_exp_mentions(exp: &Value, column: &str) -> bool {
    let Value::Object(obj) = exp else {
        return false;
    };
    obj.iter().any(|(k, v)| match k.as_str() {
        "_and" | "_or" => v
            .as_array()
            .is_some_and(|items| items.iter().any(|i| bool_exp_mentions(i, column))),
        "_not" => bool_exp_mentions(v, column),
        "_exists" => false,
        key => key == column,
    })
}

fn rename_in_bool_exp(exp: &mut Value, old: &str, new: &str) {
    let Value::Object(obj) = exp else {
        return;
    };
    let entries: Vec<(String, Value)> = std::mem::take(obj).into_iter().collect();
    for (k, mut v) in entries {
        match k.as_str() {
            "_and" | "_or" => {
                if let Value::Array(items) = &mut v {
                    for i in items {
                        rename_in_bool_exp(i, old, new);
                    }
                }
            }
            "_not" => rename_in_bool_exp(&mut v, old, new),
            _ => {}
        }
        let k = if k == old { new.to_owned() } else { k };
        obj.insert(k, v);
    }
}

fn columns_mention(cols: &Columns, column: &str) -> bool {
    matches!(cols, Columns::List(list) if list.iter().any(|c| c == column))
}

fn rename_columns(cols: &mut Columns, old: &str, new: &str) {
    if let Columns::List(list) = cols {
        for c in list.iter_mut().filter(|c| *c == old) {
            *c = new.to_owned();
        }
    }
}

/// Permissions and relationships that use `column` of `table`.
fn column_dependents(
    tables: &[TableEntry],
    table: &QualifiedTable,
    column: &str,
) -> Vec<Dependent> {
    let mut out = vec![];
    for t in tables {
        let own = t.table == *table;
        if own {
            let perm = |role: &str, kind: &'static str| Dependent::Permission {
                table: t.table.clone(),
                role: role.to_owned(),
                kind,
            };
            for p in &t.select_permissions {
                if columns_mention(&p.permission.columns, column)
                    || bool_exp_mentions(&p.permission.filter, column)
                {
                    out.push(perm(&p.role, "select"));
                }
            }
            for p in &t.insert_permissions {
                if p.permission
                    .columns
                    .as_ref()
                    .is_some_and(|c| columns_mention(c, column))
                    || bool_exp_mentions(&p.permission.check, column)
                    || p.permission.set.contains_key(column)
                {
                    out.push(perm(&p.role, "insert"));
                }
            }
            for p in &t.update_permissions {
                if columns_mention(&p.permission.columns, column)
                    || bool_exp_mentions(&p.permission.filter, column)
                    || p.permission
                        .check
                        .as_ref()
                        .is_some_and(|c| bool_exp_mentions(c, column))
                    || p.permission.set.contains_key(column)
                {
                    out.push(perm(&p.role, "update"));
                }
            }
            for p in &t.delete_permissions {
                if bool_exp_mentions(&p.permission.filter, column) {
                    out.push(perm(&p.role, "delete"));
                }
            }
        }
        for r in &t.object_relationships {
            let uses = match &r.using {
                ObjectRelUsing::ForeignKey {
                    foreign_key_constraint_on: fk,
                } => match fk {
                    ObjectFkOn::Column(c) => own && c == column,
                    ObjectFkOn::Columns(cs) => own && cs.iter().any(|c| c == column),
                    ObjectFkOn::Remote(r) => {
                        r.table == *table && r.columns().iter().any(|c| c == column)
                    }
                },
                ObjectRelUsing::Manual {
                    manual_configuration: m,
                } => {
                    (own && m.column_mapping.contains_key(column))
                        || (m.remote_table == *table
                            && m.column_mapping.values().any(|c| c == column))
                }
            };
            if uses {
                out.push(Dependent::Relationship {
                    table: t.table.clone(),
                    name: r.name.clone(),
                });
            }
        }
        for r in &t.array_relationships {
            let uses = match &r.using {
                ArrayRelUsing::ForeignKey {
                    foreign_key_constraint_on: fk,
                } => fk.table == *table && fk.columns().iter().any(|c| c == column),
                ArrayRelUsing::Manual {
                    manual_configuration: m,
                } => {
                    (own && m.column_mapping.contains_key(column))
                        || (m.remote_table == *table
                            && m.column_mapping.values().any(|c| c == column))
                }
            };
            if uses {
                out.push(Dependent::Relationship {
                    table: t.table.clone(),
                    name: r.name.clone(),
                });
            }
        }
    }
    out
}

/// Foreign-key relationships that use the constraint on `columns` of `table` referencing
/// `referenced`: the object relationship on `table` and the array relationship on
/// `referenced`.
fn fk_dependents(
    tables: &[TableEntry],
    table: &QualifiedTable,
    referenced: &QualifiedTable,
    columns: &[&str],
) -> Vec<Dependent> {
    let same = |cs: &[String]| {
        cs.len() == columns.len() && cs.iter().all(|c| columns.contains(&c.as_str()))
    };
    let mut out = vec![];
    for t in tables {
        if t.table == *table {
            for r in &t.object_relationships {
                let uses = match &r.using {
                    ObjectRelUsing::ForeignKey {
                        foreign_key_constraint_on: ObjectFkOn::Column(c),
                    } => same(std::slice::from_ref(c)),
                    ObjectRelUsing::ForeignKey {
                        foreign_key_constraint_on: ObjectFkOn::Columns(cs),
                    } => same(cs),
                    _ => false,
                };
                if uses {
                    out.push(Dependent::Relationship {
                        table: t.table.clone(),
                        name: r.name.clone(),
                    });
                }
            }
        }
        if t.table == *referenced {
            for r in &t.array_relationships {
                if let ArrayRelUsing::ForeignKey {
                    foreign_key_constraint_on: fk,
                } = &r.using
                    && fk.table == *table
                    && same(&fk.columns())
                {
                    out.push(Dependent::Relationship {
                        table: t.table.clone(),
                        name: r.name.clone(),
                    });
                }
            }
            for r in &t.object_relationships {
                if let ObjectRelUsing::ForeignKey {
                    foreign_key_constraint_on: ObjectFkOn::Remote(fk),
                } = &r.using
                    && fk.table == *table
                    && same(&fk.columns())
                {
                    out.push(Dependent::Relationship {
                        table: t.table.clone(),
                        name: r.name.clone(),
                    });
                }
            }
        }
    }
    out
}

fn rename_remote_fk(fk: &mut RemoteFk, table: &QualifiedTable, old: &str, new: &str) {
    if fk.table != *table {
        return;
    }
    if fk.column.as_deref() == Some(old) {
        fk.column = Some(new.to_owned());
    }
    if let Some(cols) = &mut fk.columns {
        for c in cols.iter_mut().filter(|c| *c == old) {
            *c = new.to_owned();
        }
    }
}

fn rename_mapping(
    mapping: &mut indexmap::IndexMap<String, String>,
    local: bool,
    remote: bool,
    old: &str,
    new: &str,
) {
    let entries: Vec<(String, String)> = std::mem::take(mapping).into_iter().collect();
    for (mut l, mut r) in entries {
        if local && l == old {
            l = new.to_owned();
        }
        if remote && r == old {
            r = new.to_owned();
        }
        mapping.insert(l, r);
    }
}

fn rename_column(tables: &mut [TableEntry], table: &QualifiedTable, old: &str, new: &str) {
    for t in tables.iter_mut() {
        let own = t.table == *table;
        if own {
            let cfg = &mut t.configuration;
            if let Some(i) = cfg.column_config.get_index_of(old) {
                let (_, v) = cfg
                    .column_config
                    .shift_remove_index(i)
                    .expect("index exists");
                cfg.column_config.shift_insert(i, new.to_owned(), v);
            }
            if let Some(i) = cfg.custom_column_names.get_index_of(old) {
                let (_, v) = cfg
                    .custom_column_names
                    .shift_remove_index(i)
                    .expect("index exists");
                cfg.custom_column_names.shift_insert(i, new.to_owned(), v);
            }
            for p in &mut t.select_permissions {
                rename_columns(&mut p.permission.columns, old, new);
                rename_in_bool_exp(&mut p.permission.filter, old, new);
            }
            for p in &mut t.insert_permissions {
                if let Some(columns) = &mut p.permission.columns {
                    rename_columns(columns, old, new);
                }
                rename_in_bool_exp(&mut p.permission.check, old, new);
                rename_key(&mut p.permission.set, old, new);
            }
            for p in &mut t.update_permissions {
                rename_columns(&mut p.permission.columns, old, new);
                rename_in_bool_exp(&mut p.permission.filter, old, new);
                if let Some(c) = &mut p.permission.check {
                    rename_in_bool_exp(c, old, new);
                }
                rename_key(&mut p.permission.set, old, new);
            }
            for p in &mut t.delete_permissions {
                rename_in_bool_exp(&mut p.permission.filter, old, new);
            }
        }
        for r in &mut t.object_relationships {
            match &mut r.using {
                ObjectRelUsing::ForeignKey {
                    foreign_key_constraint_on: fk,
                } => match fk {
                    ObjectFkOn::Column(c) if own && c == old => *c = new.to_owned(),
                    ObjectFkOn::Columns(cs) if own => {
                        for c in cs.iter_mut().filter(|c| *c == old) {
                            *c = new.to_owned();
                        }
                    }
                    ObjectFkOn::Remote(r) => rename_remote_fk(r, table, old, new),
                    _ => {}
                },
                ObjectRelUsing::Manual {
                    manual_configuration: m,
                } => {
                    let remote = m.remote_table == *table;
                    rename_mapping(&mut m.column_mapping, own, remote, old, new);
                }
            }
        }
        for r in &mut t.array_relationships {
            match &mut r.using {
                ArrayRelUsing::ForeignKey {
                    foreign_key_constraint_on: fk,
                } => rename_remote_fk(fk, table, old, new),
                ArrayRelUsing::Manual {
                    manual_configuration: m,
                } => {
                    let remote = m.remote_table == *table;
                    rename_mapping(&mut m.column_mapping, own, remote, old, new);
                }
            }
        }
    }
}

fn rename_key(map: &mut indexmap::IndexMap<String, Value>, old: &str, new: &str) {
    if let Some(i) = map.get_index_of(old) {
        let (_, v) = map.shift_remove_index(i).expect("index exists");
        map.shift_insert(i, new.to_owned(), v);
    }
}

fn rename_table(tables: &mut [TableEntry], old: &QualifiedTable, new: &QualifiedTable) {
    for t in tables.iter_mut() {
        if t.table == *old {
            t.table = new.clone();
        }
        for r in &mut t.object_relationships {
            match &mut r.using {
                ObjectRelUsing::ForeignKey {
                    foreign_key_constraint_on: ObjectFkOn::Remote(fk),
                } if fk.table == *old => fk.table = new.clone(),
                ObjectRelUsing::Manual {
                    manual_configuration: m,
                } if m.remote_table == *old => m.remote_table = new.clone(),
                _ => {}
            }
        }
        for r in &mut t.array_relationships {
            match &mut r.using {
                ArrayRelUsing::ForeignKey {
                    foreign_key_constraint_on: fk,
                } if fk.table == *old => fk.table = new.clone(),
                ArrayRelUsing::Manual {
                    manual_configuration: m,
                } if m.remote_table == *old => m.remote_table = new.clone(),
                _ => {}
            }
        }
    }
}

fn drop_dependent(tables: &mut [TableEntry], d: &Dependent) {
    match d {
        Dependent::Relationship { table, name } => {
            if let Some(t) = tables.iter_mut().find(|t| t.table == *table) {
                t.object_relationships.retain(|r| r.name != *name);
                t.array_relationships.retain(|r| r.name != *name);
            }
        }
        Dependent::Permission { table, role, kind } => {
            if let Some(t) = tables.iter_mut().find(|t| t.table == *table) {
                match *kind {
                    "select" => t.select_permissions.retain(|p| p.role != *role),
                    "insert" => t.insert_permissions.retain(|p| p.role != *role),
                    "update" => t.update_permissions.retain(|p| p.role != *role),
                    _ => t.delete_permissions.retain(|p| p.role != *role),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn md() -> Metadata {
        hge_metadata::from_json(json!({
            "version": 3,
            "sources": [{"name": "default", "configuration": {}, "tables": [
                {"table": "author",
                 "array_relationships": [{"name": "articles", "using": {"foreign_key_constraint_on": {"table": "article", "column": "author_id"}}}],
                 "select_permissions": [{"role": "user", "permission": {"columns": ["id", "name"], "filter": {"_and": [{"id": {"_eq": "X-Hasura-User-Id"}}]}}}]},
                {"table": "article",
                 "object_relationships": [{"name": "author", "using": {"foreign_key_constraint_on": "author_id"}}]}
            ]}]
        }))
        .unwrap()
    }

    fn snap(oid: u32, name: &str, cols: &[(i16, &str)]) -> TableSnapshot {
        TableSnapshot {
            oid,
            table: QualifiedTable::public(name),
            columns: cols.iter().map(|(n, c)| (*n, (*c).to_owned())).collect(),
            foreign_keys: vec![],
        }
    }

    #[test]
    fn dropping_a_referenced_table_drops_forward_relationships() {
        let mut article = snap(2, "article", &[(1, "id"), (2, "author_id")]);
        article.foreign_keys = vec![(1, vec![2])];
        let before = [snap(1, "author", &[(1, "id"), (2, "name")]), article];
        // `DROP TABLE author CASCADE` also drops the constraint on article.
        let after = [snap(2, "article", &[(1, "id"), (2, "author_id")])];
        let err = apply(&mut md(), "default", &before, &after, false).unwrap_err();
        assert_eq!(
            err.error.message,
            "cannot drop due to the following dependent objects: relationship article.author in source \"default\""
        );
        let mut m = md();
        apply(&mut m, "default", &before, &after, true).unwrap();
        assert_eq!(m.sources[0].tables.len(), 1);
        assert!(m.sources[0].tables[0].object_relationships.is_empty());
    }

    #[test]
    fn renames_tables_and_columns() {
        let mut m = md();
        let before = [
            snap(1, "author", &[(1, "id"), (2, "name")]),
            snap(2, "article", &[(1, "id"), (2, "author_id")]),
        ];
        let after = [
            snap(1, "writer", &[(1, "uid"), (2, "name")]),
            snap(2, "article", &[(1, "id"), (2, "writer_id")]),
        ];
        apply(&mut m, "default", &before, &after, false).unwrap();
        let tables = &m.sources[0].tables;
        assert_eq!(tables[0].table.name, "writer");
        let perm = &tables[0].select_permissions[0].permission;
        assert_eq!(
            perm.columns,
            Columns::List(vec!["uid".into(), "name".into()])
        );
        assert_eq!(
            perm.filter,
            json!({"_and": [{"uid": {"_eq": "X-Hasura-User-Id"}}]})
        );
        let ArrayRelUsing::ForeignKey {
            foreign_key_constraint_on,
        } = &tables[0].array_relationships[0].using
        else {
            panic!()
        };
        assert_eq!(
            foreign_key_constraint_on.column.as_deref(),
            Some("writer_id")
        );
    }

    #[test]
    fn swaps_and_rename_then_drop() {
        let mut m = md();
        let before = [
            snap(1, "author", &[(1, "id"), (2, "name")]),
            snap(2, "article", &[(1, "id"), (2, "author_id")]),
        ];
        let after = [
            snap(1, "article", &[(1, "name"), (2, "id")]),
            snap(2, "author", &[(1, "id"), (2, "author_id")]),
        ];
        apply(&mut m, "default", &before, &after, false).unwrap();
        let names: Vec<_> = m.sources[0]
            .tables
            .iter()
            .map(|t| t.table.name.clone())
            .collect();
        assert_eq!(names, ["article", "author"]);
        let perm = &m.sources[0].tables[0].select_permissions[0].permission;
        assert_eq!(
            perm.columns,
            Columns::List(vec!["name".into(), "id".into()])
        );

        // Rename a table and drop a column its permission uses, with cascade.
        let mut m = md();
        let after = [
            snap(1, "writer", &[(1, "id")]),
            snap(2, "article", &[(1, "id"), (2, "author_id")]),
        ];
        apply(&mut m, "default", &before, &after, true).unwrap();
        let writer = &m.sources[0].tables[0];
        assert_eq!(writer.table.name, "writer");
        assert!(writer.select_permissions.is_empty());
    }

    #[test]
    fn dropping_requires_cascade() {
        let before = [
            snap(1, "author", &[(1, "id"), (2, "name")]),
            snap(2, "article", &[(1, "id"), (2, "author_id")]),
        ];
        let after = [snap(1, "author", &[(1, "id"), (2, "name")])];
        let err = apply(&mut md(), "default", &before, &after, false).unwrap_err();
        assert_eq!(
            err.error.message,
            "cannot drop due to the following dependent objects: relationship author.articles in source \"default\""
        );
        let mut m = md();
        apply(&mut m, "default", &before, &after, true).unwrap();
        assert_eq!(m.sources[0].tables.len(), 1);
        assert!(m.sources[0].tables[0].array_relationships.is_empty());

        // Dropping a column used by a permission.
        let after = [
            snap(1, "author", &[(1, "id")]),
            snap(2, "article", &[(1, "id"), (2, "author_id")]),
        ];
        let err = apply(&mut md(), "default", &before, &after, false).unwrap_err();
        assert!(
            err.error
                .message
                .ends_with("permission author.user.select in source \"default\"")
        );
    }
}
