//! Apply database schema changes made by `run_sql` to the metadata, like V2: renamed tables
//! and columns are renamed in the metadata, dropped ones are removed, and objects depending
//! on dropped ones block the SQL unless `cascade` is set.

use std::collections::{BTreeMap, HashMap};

use hge_core::{Code, QualifiedTable, SourceCatalog};
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
    /// attnum → column type (`format_type`)
    pub column_types: BTreeMap<i16, String>,
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
    EventTrigger {
        table: QualifiedTable,
        name: String,
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
            Dependent::EventTrigger { table, name } => format!(
                "event-trigger {}.{name} in source \"{source}\"",
                table.v2_text()
            ),
        }
    }
}

/// The dependents as V2 lists them: reports sorted as text (`reportSchemaObjs`).
fn report_list(deps: &[Dependent], source: &str) -> Vec<String> {
    let mut list: Vec<String> = deps.iter().map(|d| d.report(source)).collect();
    list.sort();
    list.dedup();
    list
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
    let rels = Rels::new(&src.tables, before);
    let mut dependents = vec![];
    let mut dropped_tables = vec![];
    // Columns whose type changed: what uses them blocks the change, like a drop.
    let mut retyped = vec![];
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
                        dependents.extend(column_dependents(&src.tables, &rels, &b.table, old));
                    } else if a.column_types.get(attnum) != b.column_types.get(attnum) {
                        let deps = typed_column_dependents(&src.tables, &rels, &b.table, old);
                        if !deps.is_empty() {
                            retyped.push((b.table.clone(), old.clone(), deps));
                        }
                    }
                }
            }
        }
    }
    if !cascade && let Some((table, column, deps)) = retyped.first() {
        let list = report_list(deps, source);
        return Err(ApiError::bad(
            Code::DependencyError,
            format!(
                "cannot change type of column \"{column}\" in table \"{}\" because of the following dependencies: {}",
                table.v2_text(),
                list.join(", ")
            ),
        ));
    }
    dependents.extend(retyped.into_iter().flat_map(|(_, _, deps)| deps));
    // Objects of dropped tables go with them.
    dependents.retain(|d| match d {
        Dependent::Relationship { table, .. }
        | Dependent::Permission { table, .. }
        | Dependent::EventTrigger { table, .. } => !dropped_tables.contains(table),
    });
    dependents.sort();
    dependents.dedup();
    if !dependents.is_empty() && !cascade {
        let list = report_list(&dependents, source);
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
            rename_column(&mut src.tables, &rels, &b.table, old, &temporary(i));
        }
        for (i, (_, new)) in columns.iter().enumerate() {
            rename_column(&mut src.tables, &rels, &b.table, &temporary(i), new);
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
        } => manual_configuration.remote_table.as_ref(),
    }
}

fn arr_rel_target(using: &ArrayRelUsing) -> Option<&QualifiedTable> {
    match using {
        ArrayRelUsing::ForeignKey {
            foreign_key_constraint_on,
        } => Some(&foreign_key_constraint_on.table),
        ArrayRelUsing::Manual {
            manual_configuration,
        } => manual_configuration.remote_table.as_ref(),
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
            if arr_rel_target(&r.using) == Some(dropped) {
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
/// Relationship targets, `(table, relationship) → remote table`, resolved before anything is
/// renamed (forward foreign keys through the snapshot, the other forms from metadata).
pub(crate) struct Rels(HashMap<(QualifiedTable, String), QualifiedTable>);

impl Rels {
    fn new(tables: &[TableEntry], before: &[TableSnapshot]) -> Self {
        let forward = |table: &QualifiedTable, columns: &[String]| -> Option<QualifiedTable> {
            let snap = before.iter().find(|s| s.table == *table)?;
            let (ref_oid, _) = snap.foreign_keys.iter().find(|(_, attnums)| {
                attnums.len() == columns.len()
                    && attnums
                        .iter()
                        .all(|n| snap.columns.get(n).is_some_and(|c| columns.contains(c)))
            })?;
            before
                .iter()
                .find(|s| s.oid == *ref_oid)
                .map(|s| s.table.clone())
        };
        Self::with(tables, forward)
    }

    /// Forward foreign keys resolved through an introspected catalog.
    pub(crate) fn from_catalog(tables: &[TableEntry], catalog: Option<&SourceCatalog>) -> Self {
        Self::with(tables, |table, columns| {
            let fks = &catalog?.table(table)?.foreign_keys;
            fks.iter()
                .find(|fk| {
                    fk.columns.len() == columns.len()
                        && fk.columns.iter().all(|(c, _)| columns.contains(c))
                })
                .map(|fk| fk.ref_table.clone())
        })
    }

    fn with(
        tables: &[TableEntry],
        forward: impl Fn(&QualifiedTable, &[String]) -> Option<QualifiedTable>,
    ) -> Self {
        let mut map = HashMap::new();
        for t in tables {
            for r in &t.object_relationships {
                let target = match &r.using {
                    ObjectRelUsing::ForeignKey {
                        foreign_key_constraint_on: ObjectFkOn::Column(c),
                    } => forward(&t.table, std::slice::from_ref(c)),
                    ObjectRelUsing::ForeignKey {
                        foreign_key_constraint_on: ObjectFkOn::Columns(cs),
                    } => forward(&t.table, cs),
                    ObjectRelUsing::ForeignKey {
                        foreign_key_constraint_on: ObjectFkOn::Remote(fk),
                    } => Some(fk.table.clone()),
                    ObjectRelUsing::Manual {
                        manual_configuration: m,
                    } => m.remote_table.clone(),
                };
                if let Some(target) = target {
                    map.insert((t.table.clone(), r.name.clone()), target);
                }
            }
            for r in &t.array_relationships {
                if let Some(target) = arr_rel_target(&r.using) {
                    map.insert((t.table.clone(), r.name.clone()), target.clone());
                }
            }
        }
        Rels(map)
    }
}

/// Visit every column key of a boolean expression on `table`, following relationships and
/// `_exists` into other tables. `f(table, column, operators)` returns a new name to rename
/// the key.
fn visit_bool_exp(
    exp: &mut Value,
    table: &QualifiedTable,
    rels: &Rels,
    f: &mut dyn FnMut(&QualifiedTable, &str, &Value) -> Option<String>,
) {
    let Value::Object(obj) = exp else {
        return;
    };
    let entries: Vec<(String, Value)> = std::mem::take(obj).into_iter().collect();
    for (k, mut v) in entries {
        let key = match k.as_str() {
            "_and" | "_or" => {
                if let Value::Array(items) = &mut v {
                    for i in items {
                        visit_bool_exp(i, table, rels, f);
                    }
                }
                k
            }
            "_not" => {
                visit_bool_exp(&mut v, table, rels, f);
                k
            }
            "_exists" => {
                let remote = v
                    .get("_table")
                    .and_then(|t| serde_json::from_value::<QualifiedTable>(t.clone()).ok());
                if let (Some(remote), Some(w)) = (remote, v.get_mut("_where")) {
                    visit_bool_exp(w, &remote, rels, f);
                }
                k
            }
            _ => match rels.0.get(&(table.clone(), k.clone())) {
                Some(remote) => {
                    visit_bool_exp(&mut v, remote, rels, f);
                    k
                }
                None => f(table, &k, &v).unwrap_or(k),
            },
        };
        obj.insert(key, v);
    }
}

/// Does a boolean expression on `context` use `column` of `table`?
pub(crate) fn bool_exp_mentions(
    exp: &Value,
    context: &QualifiedTable,
    rels: &Rels,
    table: &QualifiedTable,
    column: &str,
) -> bool {
    let mut found = false;
    visit_bool_exp(&mut exp.clone(), context, rels, &mut |t, c, _| {
        found |= t == table && c == column;
        None
    });
    found
}

/// Does a boolean expression on `context` depend on the type of `column` of `table`? Like
/// V2's `DROnType`: the column is compared with a static value (not a session variable, not
/// `_is_null`), or is the right-hand side of a column comparison (`_ceq` etc.).
fn bool_exp_types(
    exp: &Value,
    context: &QualifiedTable,
    rels: &Rels,
    table: &QualifiedTable,
    column: &str,
) -> bool {
    let mut found = false;
    visit_bool_exp(&mut exp.clone(), context, rels, &mut |t, c, ops| {
        if t != table {
            return None;
        }
        let Value::Object(ops) = ops else {
            found |= c == column && is_static(ops);
            return None;
        };
        for (op, v) in ops {
            if is_column_op(op) {
                // `"other"` or `["$", "other"]` (the root table, which we treat alike).
                let rhs = match v {
                    Value::Array(path) => path.last(),
                    v => Some(v),
                };
                found |= rhs.and_then(Value::as_str) == Some(column);
            } else if c == column && op != "_is_null" && is_static(v) {
                found = true;
            }
        }
        None
    });
    found
}

/// `_ceq`, `_cgt`, … compare with another column.
fn is_column_op(op: &str) -> bool {
    matches!(
        op,
        "_ceq"
            | "_cne"
            | "_cneq"
            | "_cgt"
            | "_clt"
            | "_cgte"
            | "_clte"
            | "$ceq"
            | "$cne"
            | "$cneq"
            | "$cgt"
            | "$clt"
            | "$cgte"
            | "$clte"
    )
}

/// A literal rather than a session variable reference.
fn is_static(v: &Value) -> bool {
    !v.as_str()
        .is_some_and(hge_core::session::is_session_variable)
}

fn rename_in_bool_exp(
    exp: &mut Value,
    context: &QualifiedTable,
    rels: &Rels,
    table: &QualifiedTable,
    old: &str,
    new: &str,
) {
    visit_bool_exp(exp, context, rels, &mut |t, c, _| {
        (t == table && c == old).then(|| new.to_owned())
    });
}

/// Point `_exists` clauses at a renamed table.
fn rename_exists_table(exp: &mut Value, old: &QualifiedTable, new: &QualifiedTable) {
    match exp {
        Value::Object(obj) => {
            if let Some(Value::Object(ex)) = obj.get_mut("_exists")
                && let Some(t) = ex.get_mut("_table")
                && serde_json::from_value::<QualifiedTable>(t.clone())
                    .ok()
                    .as_ref()
                    == Some(old)
            {
                *t = serde_json::to_value(new).unwrap_or_default();
            }
            for v in obj.values_mut() {
                rename_exists_table(v, old, new);
            }
        }
        Value::Array(items) => {
            for v in items {
                rename_exists_table(v, old, new);
            }
        }
        _ => {}
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

/// Permissions that depend on the type of `column` of `table`: boolean expressions that
/// compare it with a static value, and static presets. Column lists, session variables and
/// relationships don't care about the type (V2's `DROnType` dependencies).
fn typed_column_dependents(
    tables: &[TableEntry],
    rels: &Rels,
    table: &QualifiedTable,
    column: &str,
) -> Vec<Dependent> {
    let mut out = vec![];
    for t in tables {
        let own = t.table == *table;
        let exp = |e: &Value| bool_exp_types(e, &t.table, rels, table, column);
        let preset =
            |set: &indexmap::IndexMap<String, Value>| own && set.get(column).is_some_and(is_static);
        let perm = |role: &str, kind: &'static str| Dependent::Permission {
            table: t.table.clone(),
            role: role.to_owned(),
            kind,
        };
        for p in &t.select_permissions {
            if exp(&p.permission.filter) {
                out.push(perm(&p.role, "select"));
            }
        }
        for p in &t.insert_permissions {
            if preset(&p.permission.set) || exp(&p.permission.check) {
                out.push(perm(&p.role, "insert"));
            }
        }
        for p in &t.update_permissions {
            if preset(&p.permission.set)
                || exp(&p.permission.filter)
                || p.permission.check.as_ref().is_some_and(exp)
            {
                out.push(perm(&p.role, "update"));
            }
        }
        for p in &t.delete_permissions {
            if exp(&p.permission.filter) {
                out.push(perm(&p.role, "delete"));
            }
        }
    }
    out
}

/// Permissions and relationships that use `column` of `table`.
fn column_dependents(
    tables: &[TableEntry],
    rels: &Rels,
    table: &QualifiedTable,
    column: &str,
) -> Vec<Dependent> {
    let mut out = vec![];
    for t in tables {
        let own = t.table == *table;
        // Columns and presets name the table's own columns; boolean expressions can reach
        // `table` from any table through relationships and `_exists`.
        let exp = |e: &Value| bool_exp_mentions(e, &t.table, rels, table, column);
        let perm = |role: &str, kind: &'static str| Dependent::Permission {
            table: t.table.clone(),
            role: role.to_owned(),
            kind,
        };
        for p in &t.select_permissions {
            if (own && columns_mention(&p.permission.columns, column)) || exp(&p.permission.filter)
            {
                out.push(perm(&p.role, "select"));
            }
        }
        for p in &t.insert_permissions {
            let cols = p
                .permission
                .columns
                .as_ref()
                .is_some_and(|c| columns_mention(c, column));
            if (own && (cols || p.permission.set.contains_key(column))) || exp(&p.permission.check)
            {
                out.push(perm(&p.role, "insert"));
            }
        }
        for p in &t.update_permissions {
            if (own
                && (columns_mention(&p.permission.columns, column)
                    || p.permission.set.contains_key(column)))
                || exp(&p.permission.filter)
                || p.permission.check.as_ref().is_some_and(exp)
            {
                out.push(perm(&p.role, "update"));
            }
        }
        for p in &t.delete_permissions {
            if exp(&p.permission.filter) {
                out.push(perm(&p.role, "delete"));
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
                        || (m.remote_table.as_ref() == Some(table)
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
                        || (m.remote_table.as_ref() == Some(table)
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
        if own {
            for trigger in &t.event_triggers {
                if trigger_mentions(trigger, column)
                    && let Some(name) = trigger.get("name").and_then(Value::as_str)
                {
                    out.push(Dependent::EventTrigger {
                        table: t.table.clone(),
                        name: name.to_owned(),
                    });
                }
            }
        }
    }
    out
}

/// The column lists of an event trigger's operations: `columns` and `payload`, when they
/// are lists rather than `"*"`.
fn trigger_column_lists(trigger: &mut Value) -> impl Iterator<Item = &mut Vec<Value>> {
    trigger
        .get_mut("definition")
        .and_then(Value::as_object_mut)
        .into_iter()
        .flat_map(|d| d.iter_mut())
        .filter_map(|(op, spec)| {
            matches!(op.as_str(), "insert" | "update" | "delete").then_some(spec)
        })
        .filter_map(Value::as_object_mut)
        .flat_map(|spec| spec.iter_mut())
        .filter_map(|(key, v)| matches!(key.as_str(), "columns" | "payload").then_some(v))
        .filter_map(Value::as_array_mut)
}

/// Whether an event trigger lists `column` in an operation's `columns` or `payload`.
fn trigger_mentions(trigger: &Value, column: &str) -> bool {
    let mut copy = trigger.clone();
    trigger_column_lists(&mut copy).any(|cols| cols.iter().any(|c| c == column))
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

fn rename_column(
    tables: &mut [TableEntry],
    rels: &Rels,
    table: &QualifiedTable,
    old: &str,
    new: &str,
) {
    for t in tables.iter_mut() {
        let own = t.table == *table;
        let context = t.table.clone();
        let exp = |e: &mut Value| rename_in_bool_exp(e, &context, rels, table, old, new);
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
        }
        if own {
            for trigger in &mut t.event_triggers {
                for cols in trigger_column_lists(trigger) {
                    for c in cols.iter_mut().filter(|c| *c == old) {
                        *c = Value::String(new.to_owned());
                    }
                }
            }
        }
        for p in &mut t.select_permissions {
            if own {
                rename_columns(&mut p.permission.columns, old, new);
            }
            exp(&mut p.permission.filter);
        }
        for p in &mut t.insert_permissions {
            if own {
                if let Some(columns) = &mut p.permission.columns {
                    rename_columns(columns, old, new);
                }
                rename_key(&mut p.permission.set, old, new);
            }
            exp(&mut p.permission.check);
        }
        for p in &mut t.update_permissions {
            if own {
                rename_columns(&mut p.permission.columns, old, new);
                rename_key(&mut p.permission.set, old, new);
            }
            exp(&mut p.permission.filter);
            if let Some(c) = &mut p.permission.check {
                exp(c);
            }
        }
        for p in &mut t.delete_permissions {
            exp(&mut p.permission.filter);
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
                    let remote = m.remote_table.as_ref() == Some(table);
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
                    let remote = m.remote_table.as_ref() == Some(table);
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
        for e in t
            .select_permissions
            .iter_mut()
            .map(|p| &mut p.permission.filter)
            .chain(
                t.insert_permissions
                    .iter_mut()
                    .map(|p| &mut p.permission.check),
            )
            .chain(t.update_permissions.iter_mut().flat_map(|p| {
                std::iter::once(&mut p.permission.filter).chain(p.permission.check.as_mut())
            }))
            .chain(
                t.delete_permissions
                    .iter_mut()
                    .map(|p| &mut p.permission.filter),
            )
        {
            rename_exists_table(e, old, new);
        }
        for r in &mut t.object_relationships {
            match &mut r.using {
                ObjectRelUsing::ForeignKey {
                    foreign_key_constraint_on: ObjectFkOn::Remote(fk),
                } if fk.table == *old => fk.table = new.clone(),
                ObjectRelUsing::Manual {
                    manual_configuration: m,
                } if m.remote_table.as_ref() == Some(old) => m.remote_table = Some(new.clone()),
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
                } if m.remote_table.as_ref() == Some(old) => m.remote_table = Some(new.clone()),
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
        Dependent::EventTrigger { table, name } => {
            if let Some(t) = tables.iter_mut().find(|t| t.table == *table) {
                t.event_triggers
                    .retain(|e| e.get("name").and_then(Value::as_str) != Some(name));
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
                 "select_permissions": [{"role": "user", "permission": {"columns": ["id", "name"], "filter": {"_and": [{"id": {"_eq": "X-Hasura-User-Id"}}, {"name": {"_ne": "x"}}]}}}]},
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
            column_types: cols
                .iter()
                .map(|(n, _)| (*n, "integer".to_owned()))
                .collect(),
            foreign_keys: vec![],
        }
    }

    #[test]
    fn renames_follow_relationships() {
        // article.user filter goes through `author` (forward fk) to author.id.
        let mut m = hge_metadata::from_json(json!({
            "version": 3,
            "sources": [{"name": "default", "configuration": {}, "tables": [
                {"table": "author"},
                {"table": "article",
                 "object_relationships": [{"name": "author", "using": {"foreign_key_constraint_on": "author_id"}}],
                 "select_permissions": [{"role": "user", "permission": {"columns": ["id"],
                    "filter": {"author": {"id": "X-Hasura-User-Id"}, "id": {"_gt": 0},
                               "_exists": {"_table": "author", "_where": {"id": {"_eq": 1}}}}}}]}
            ]}]
        }))
        .unwrap();
        let mut article = snap(2, "article", &[(1, "id"), (2, "author_id")]);
        article.foreign_keys = vec![(1, vec![2])];
        let before = [snap(1, "author", &[(1, "id")]), article.clone()];
        let after = [snap(1, "writer", &[(1, "author_id")]), article];
        apply(&mut m, "default", &before, &after, false).unwrap();
        let filter = &m.sources[0].tables[1].select_permissions[0]
            .permission
            .filter;
        assert_eq!(
            filter,
            &json!({"author": {"author_id": "X-Hasura-User-Id"}, "id": {"_gt": 0},
                    "_exists": {"_table": {"schema": "public", "name": "writer"}, "_where": {"author_id": {"_eq": 1}}}})
        );
    }

    #[test]
    fn type_changes_of_used_columns_need_cascade() {
        let before = [
            snap(1, "author", &[(1, "id"), (2, "name")]),
            snap(2, "article", &[(1, "id"), (2, "author_id")]),
        ];
        let retype = |attnum: i16| {
            let mut author = snap(1, "author", &[(1, "id"), (2, "name")]);
            author.column_types.insert(attnum, "text".into());
            [author, before[1].clone()]
        };
        // `name` is compared with a static value.
        let err = apply(&mut md(), "default", &before, &retype(2), false).unwrap_err();
        assert_eq!(
            err.error.message,
            "cannot change type of column \"name\" in table \"author\" because of the following dependencies: permission author.user.select in source \"default\""
        );
        // `id` is only compared with a session variable, listed in `columns` and used by
        // relationships: none of these depend on its type.
        apply(&mut md(), "default", &before, &retype(1), false).unwrap();
    }

    #[test]
    fn type_dependencies_follow_v2() {
        let m = hge_metadata::from_json(json!({
            "version": 3,
            "sources": [{"name": "default", "configuration": {}, "tables": [
                {"table": "p"},
                {"table": "t",
                 "object_relationships": [{"name": "par", "using": {"foreign_key_constraint_on": "p"}}],
                 "select_permissions": [{"role": "u", "permission": {"columns": ["id"],
                    "filter": {"_and": [{"c": {"_in": [1, 2]}}, {"d": {"_ceq": "e"}},
                                        {"n": {"_is_null": false}}, {"par": {"s": {"_eq": 5}}}]}}}],
                 "insert_permissions": [{"role": "u", "permission": {"columns": ["id"], "check": {},
                    "set": {"f": "x-hasura-user-id", "g": "5"}}}]}
            ]}]
        }))
        .unwrap();
        let tables = &m.sources[0].tables;
        let before = [
            snap(1, "p", &[(1, "id"), (2, "s")]),
            snap(2, "t", &[(1, "p")]),
        ];
        let mut before = before;
        before[1].foreign_keys = vec![(1, vec![1])];
        let rels = Rels::new(tables, &before);
        let deps = |table: &str, column: &str| {
            typed_column_dependents(tables, &rels, &QualifiedTable::public(table), column)
                .iter()
                .map(|d| d.report("default"))
                .collect::<Vec<_>>()
        };
        let select = vec!["permission t.u.select in source \"default\"".to_owned()];
        let insert = vec!["permission t.u.insert in source \"default\"".to_owned()];
        // Checked against v2.51.0-ce.
        assert_eq!(deps("t", "c"), select);
        assert_eq!(deps("t", "d"), Vec::<String>::new());
        assert_eq!(deps("t", "e"), select);
        assert_eq!(deps("t", "n"), Vec::<String>::new());
        assert_eq!(deps("p", "s"), select);
        assert_eq!(deps("t", "f"), Vec::<String>::new());
        assert_eq!(deps("t", "g"), insert);
        assert_eq!(deps("t", "id"), Vec::<String>::new());
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
            json!({"_and": [{"uid": {"_eq": "X-Hasura-User-Id"}}, {"name": {"_ne": "x"}}]})
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
    fn event_trigger_columns() {
        let md = || {
            hge_metadata::from_json(json!({
                "version": 3,
                "sources": [{"name": "default", "configuration": {}, "tables": [
                    {"table": "t", "event_triggers": [
                        {"name": "cols", "webhook": "http://h", "definition": {
                            "insert": {"columns": "*"}, "update": {"columns": ["a"]}}},
                        {"name": "pay", "webhook": "http://h", "definition": {
                            "delete": {"columns": "*", "payload": ["b"]}}}
                    ]}
                ]}]
            }))
            .unwrap()
        };
        let before = [snap(1, "t", &[(1, "a"), (2, "b"), (3, "c")])];
        let drop = |n: i16| {
            let cols: Vec<(i16, &str)> = [(1, "a"), (2, "b"), (3, "c")]
                .into_iter()
                .filter(|(i, _)| *i != n)
                .collect();
            [snap(1, "t", &cols)]
        };
        let err = apply(&mut md(), "default", &before, &drop(1), false).unwrap_err();
        assert_eq!(
            err.error.message,
            "cannot drop due to the following dependent objects: event-trigger t.cols in source \"default\""
        );
        let err = apply(&mut md(), "default", &before, &drop(2), false).unwrap_err();
        assert!(
            err.error
                .message
                .ends_with("event-trigger t.pay in source \"default\"")
        );
        apply(&mut md(), "default", &before, &drop(3), false).unwrap();
        // Listed as V2 sorts the reports: the event trigger before the permission.
        let mut m = md();
        m.sources[0].tables[0].select_permissions = hge_metadata::from_json(json!({
            "version": 3, "sources": [{"name": "default", "configuration": {}, "tables": [
                {"table": "t", "select_permissions": [
                    {"role": "user", "permission": {"columns": ["a"], "filter": {}}}]}]}]
        }))
        .unwrap()
        .sources[0]
            .tables[0]
            .select_permissions
            .clone();
        let err = apply(&mut m, "default", &before, &drop(1), false).unwrap_err();
        assert_eq!(
            err.error.message,
            "cannot drop due to the following dependent objects: event-trigger t.cols in source \"default\", permission t.user.select in source \"default\""
        );

        let mut m = md();
        apply(&mut m, "default", &before, &drop(1), true).unwrap();
        let names: Vec<_> = m.sources[0].tables[0]
            .event_triggers
            .iter()
            .map(|t| t["name"].clone())
            .collect();
        assert_eq!(names, [json!("pay")]);

        let mut m = md();
        let renamed = [snap(1, "t", &[(1, "a"), (2, "bb"), (3, "c")])];
        apply(&mut m, "default", &before, &renamed, false).unwrap();
        assert_eq!(
            m.sources[0].tables[0].event_triggers[1]["definition"]["delete"]["payload"],
            json!(["bb"])
        );
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
