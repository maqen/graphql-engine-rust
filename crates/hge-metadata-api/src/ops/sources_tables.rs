//! Sources and tracked tables: `pg_add_source`, `pg_drop_source`, `pg_update_source`,
//! `rename_source`, `pg_track_table(s)`, `pg_untrack_table(s)`, table customization and enum
//! flags, and the read-only `pg_get_source_tables`, `pg_get_table_info`,
//! `get_source_kind_capabilities` and `get_table_info`.
//!
//! Also home to the argument and lookup helpers the other table-level modules share, and to
//! the dependency search V2 runs before dropping a table, relationship or source.

use std::collections::{BTreeMap, BTreeSet};

use hge_core::{Code, QualifiedTable, SourceCatalog};
use hge_metadata::{
    ArrayRelUsing, Metadata, ObjectFkOn, ObjectRelUsing, Source, SourceConfiguration,
    SourceCustomization, TableConfig, TableEntry,
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::api::json_kind;
use crate::ctx::{Check, Ctx};
use crate::response::{
    ApiError, ApiResult, aeson_message, already_exists, not_exists, parse_failed, success,
};
use crate::runtime::{Reload, Runtime};

/// Commands this module implements (`pg_` prefix stripped for backend commands).
pub const COMMANDS: &[&str] = &[
    "add_source",
    "drop_source",
    "update_source",
    "rename_source",
    "track_table",
    "track_tables",
    "untrack_table",
    "untrack_tables",
    "set_table_customization",
    // Legacy `/v1/query` command (V2's metadata API rejects the name before it gets here).
    "set_table_custom_fields",
    "set_table_is_enum",
    "get_source_tables",
    "get_table_info",
    "get_source_kind_capabilities",
];

pub async fn run<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    command: &str,
    args: &Value,
) -> ApiResult<Value> {
    match command {
        "add_source" => add_source(ctx, args).await,
        "drop_source" => drop_source(ctx, args).await,
        "update_source" => update_source(ctx, args).await,
        "rename_source" => rename_source(ctx, args).await,
        "track_table" => track_table(ctx, args).await,
        "track_tables" => track_tables(ctx, args).await,
        "untrack_table" => untrack_table(ctx, args).await,
        "untrack_tables" => untrack_tables(ctx, args).await,
        "set_table_customization" => set_table_customization(ctx, args).await,
        "set_table_custom_fields" => set_table_custom_fields(ctx, args).await,
        "set_table_is_enum" => set_table_is_enum(ctx, args).await,
        "get_source_tables" => get_source_tables(ctx, args).await,
        "get_table_info" => get_table_info(ctx, args).await,
        "get_source_kind_capabilities" => get_source_kind_capabilities(args),
        _ => Err(super::not_implemented(command)),
    }
}

// ---------------------------------------------------------------------------------------
// Shared helpers

/// The object arguments of a command, with aeson-style parse errors.
pub(crate) struct Args<'a> {
    obj: &'a Map<String, Value>,
    /// JSON path of the object (`$.args` or below).
    path: String,
}

impl<'a> Args<'a> {
    pub fn new(args: &'a Value) -> ApiResult<Self> {
        Self::at(args, "$.args")
    }

    pub fn at(value: &'a Value, path: &str) -> ApiResult<Self> {
        match value {
            Value::Object(obj) => Ok(Args {
                obj,
                path: path.to_owned(),
            }),
            other => Err(parse_failed(
                path,
                format!(
                    "parsing Object failed, expected Object, but encountered {}",
                    json_kind(other)
                ),
            )),
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.obj.contains_key(key)
    }

    /// A field's value; `null` counts as absent.
    pub fn raw(&self, key: &str) -> Option<&'a Value> {
        self.obj.get(key).filter(|v| !v.is_null())
    }

    pub fn missing(&self, key: &str) -> ApiError {
        parse_failed(&self.path, format!("key \"{key}\" not found"))
    }

    pub fn field_path(&self, key: &str) -> String {
        format!("{}.{key}", self.path)
    }

    /// A required field.
    pub fn req<T: DeserializeOwned>(&self, key: &str) -> ApiResult<T> {
        let v = self.obj.get(key).ok_or_else(|| self.missing(key))?;
        serde_json::from_value(v.clone())
            .map_err(|e| parse_failed(&self.field_path(key), aeson_message(&e)))
    }

    /// An optional field.
    pub fn opt<T: DeserializeOwned>(&self, key: &str) -> ApiResult<Option<T>> {
        match self.raw(key) {
            None => Ok(None),
            Some(v) => serde_json::from_value(v.clone())
                .map(Some)
                .map_err(|e| parse_failed(&self.field_path(key), aeson_message(&e))),
        }
    }

    pub fn string(&self, key: &str) -> ApiResult<String> {
        match self.obj.get(key) {
            None => Err(self.missing(key)),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(other) => Err(parse_failed(
                &self.field_path(key),
                format!(
                    "parsing Text failed, expected String, but encountered {}",
                    json_kind(other)
                ),
            )),
        }
    }

    /// A required `NonEmptyText` field (source and relationship names, roles).
    pub fn non_empty(&self, key: &str) -> ApiResult<String> {
        let s = self.string(key)?;
        self.check_non_empty(key, s)
    }

    /// Reject `""` the way V2's `NonEmptyText` parser does.
    pub fn check_non_empty(&self, key: &str, s: String) -> ApiResult<String> {
        if s.is_empty() {
            return Err(parse_failed(
                &self.field_path(key),
                "empty string not allowed",
            ));
        }
        Ok(s)
    }

    pub fn opt_string(&self, key: &str) -> ApiResult<Option<String>> {
        match self.raw(key) {
            None => Ok(None),
            Some(_) => self.string(key).map(Some),
        }
    }

    pub fn bool_or(&self, key: &str, default: bool) -> ApiResult<bool> {
        match self.raw(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => Err(parse_failed(
                &self.field_path(key),
                format!(
                    "parsing Bool failed, expected Boolean, but encountered {}",
                    json_kind(other)
                ),
            )),
        }
    }

    /// `source`, defaulting to `default`.
    pub fn source(&self) -> ApiResult<String> {
        match self.opt_string("source")? {
            Some(s) => self.check_non_empty("source", s),
            None => Ok("default".to_owned()),
        }
    }

    /// A required qualified table.
    pub fn table(&self, key: &str) -> ApiResult<QualifiedTable> {
        let v = self.obj.get(key).ok_or_else(|| self.missing(key))?;
        parse_table(v, &self.field_path(key))
    }
}

/// A table name: `"name"` or `{"schema": …, "name": …}`.
pub(crate) fn parse_table(v: &Value, path: &str) -> ApiResult<QualifiedTable> {
    let text = |key: &str, v: &Value| match v {
        Value::String(s) => Ok(s.clone()),
        other => Err(parse_failed(
            &format!("{path}.{key}"),
            format!(
                "parsing Text failed, expected String, but encountered {}",
                json_kind(other)
            ),
        )),
    };
    match v {
        Value::String(s) => Ok(QualifiedTable::public(s.clone())),
        Value::Object(obj) => {
            let name = match obj.get("name") {
                Some(v) => text("name", v)?,
                None => return Err(parse_failed(path, "key \"name\" not found")),
            };
            let schema = match obj.get("schema") {
                None | Some(Value::Null) => "public".to_owned(),
                Some(v) => text("schema", v)?,
            };
            Ok(QualifiedTable::new(schema, name))
        }
        other => Err(parse_failed(
            path,
            format!(
                "parsing QualifiedObject failed, expected Object, but encountered {}",
                json_kind(other)
            ),
        )),
    }
}

pub(crate) fn source_not_found(name: &str) -> ApiError {
    not_exists(format!("source with name \"{name}\" does not exist"))
}

pub(crate) fn table_not_found(source: &str, table: &QualifiedTable) -> ApiError {
    not_exists(format!(
        "table \"{}\" does not exist in source: {source}",
        table.v2_text()
    ))
}

pub(crate) fn source_ref<'m>(md: &'m Metadata, name: &str) -> ApiResult<&'m Source> {
    md.source(name).ok_or_else(|| source_not_found(name))
}

pub(crate) fn source_mut<'m>(md: &'m mut Metadata, name: &str) -> ApiResult<&'m mut Source> {
    md.source_mut(name).ok_or_else(|| source_not_found(name))
}

pub(crate) fn table_ref<'m>(
    md: &'m Metadata,
    source: &str,
    table: &QualifiedTable,
) -> ApiResult<&'m TableEntry> {
    source_ref(md, source)?
        .tables
        .iter()
        .find(|t| t.table == *table)
        .ok_or_else(|| table_not_found(source, table))
}

pub(crate) fn table_mut<'m>(
    md: &'m mut Metadata,
    source: &str,
    table: &QualifiedTable,
) -> ApiResult<&'m mut TableEntry> {
    source_mut(md, source)?
        .tables
        .iter_mut()
        .find(|t| t.table == *table)
        .ok_or_else(|| table_not_found(source, table))
}

/// V2's inconsistency name of a table.
pub(crate) fn table_object_name(source: &str, table: &QualifiedTable) -> String {
    format!("table {} in source {source}", table.v2_text())
}

/// V2's inconsistency name of an object attached to a table.
pub(crate) fn table_item_name(
    kind: &str,
    item: &str,
    source: &str,
    table: &QualifiedTable,
) -> String {
    format!(
        "{kind} {item} in table {} in source {source}",
        table.v2_text()
    )
}

/// The introspected catalog of a source in the current working state.
pub(crate) fn catalog<R: Runtime>(
    ctx: &Ctx<'_, R>,
    source: &str,
) -> Option<std::sync::Arc<SourceCatalog>> {
    ctx.rt.catalog(ctx.prepared.as_ref(), source)
}

// ---------------------------------------------------------------------------------------
// Dependencies

/// A metadata object that depends on something being dropped.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Dependent {
    Relationship {
        source: String,
        table: QualifiedTable,
        name: String,
    },
    RemoteRelationship {
        source: String,
        table: QualifiedTable,
        name: String,
    },
    Permission {
        source: String,
        table: QualifiedTable,
        role: String,
        kind: &'static str,
    },
    /// A tracked function returning the table (a function name has a table name's shape).
    Function {
        source: String,
        function: QualifiedTable,
    },
    /// A remote schema's remote relationship to a source.
    RemoteSchemaRelationship {
        remote_schema: String,
        type_name: String,
        name: String,
    },
}

impl Dependent {
    /// V2's `reportSchemaObj`.
    pub fn report(&self) -> String {
        match self {
            Dependent::Relationship {
                source,
                table,
                name,
            } => format!(
                "relationship {}.{name} in source \"{source}\"",
                table.v2_text()
            ),
            Dependent::RemoteRelationship {
                source,
                table,
                name,
            } => format!(
                "remote relationship {}.{name} in source \"{source}\"",
                table.v2_text()
            ),
            Dependent::Permission {
                source,
                table,
                role,
                kind,
            } => format!(
                "permission {}.{role}.{kind} in source \"{source}\"",
                table.v2_text()
            ),
            Dependent::Function { source, function } => {
                format!("function {} in source \"{source}\"", function.v2_text())
            }
            Dependent::RemoteSchemaRelationship {
                remote_schema,
                type_name,
                name,
            } => format!(
                "remote_relationship {name} on type {type_name} in remote schema {remote_schema}"
            ),
        }
    }

    /// The source table the dependent is defined on, if it is a table object.
    fn table(&self) -> Option<(&str, &QualifiedTable)> {
        match self {
            Dependent::Relationship { source, table, .. }
            | Dependent::RemoteRelationship { source, table, .. }
            | Dependent::Permission { source, table, .. } => Some((source, table)),
            Dependent::Function { .. } | Dependent::RemoteSchemaRelationship { .. } => None,
        }
    }
}

/// `cannot drop due to the following dependent objects: …`, sorted like V2.
pub(crate) fn dependency_error(dependents: &[Dependent]) -> ApiError {
    let mut list: Vec<String> = dependents.iter().map(Dependent::report).collect();
    list.sort();
    list.dedup();
    ApiError::bad(
        Code::DependencyError,
        format!(
            "cannot drop due to the following dependent objects: {}",
            list.join(", ")
        ),
    )
}

/// Remove dependents from the metadata.
pub(crate) fn drop_dependents(md: &mut Metadata, dependents: &[Dependent]) {
    for d in dependents {
        match d {
            Dependent::Function { source, function } => {
                if let Some(s) = md.source_mut(source) {
                    s.functions
                        .retain(|f| function_name(f).as_ref() != Some(function));
                }
                continue;
            }
            Dependent::RemoteSchemaRelationship {
                remote_schema,
                type_name,
                name,
            } => {
                // V2 keeps the type's entry, even when its last relationship goes.
                let types = md
                    .remote_schemas
                    .iter_mut()
                    .filter(|rs| rs.get("name").and_then(Value::as_str) == Some(remote_schema))
                    .filter_map(|rs| rs.get_mut("remote_relationships")?.as_array_mut())
                    .flatten()
                    .filter(|t| t.get("type_name").and_then(Value::as_str) == Some(type_name));
                for t in types {
                    if let Some(rels) = t.get_mut("relationships").and_then(Value::as_array_mut) {
                        rels.retain(|r| r.get("name").and_then(Value::as_str) != Some(name));
                    }
                }
                continue;
            }
            _ => {}
        }
        let Some((source, table)) = d.table() else {
            continue;
        };
        let Some(t) = md
            .source_mut(source)
            .and_then(|s| s.tables.iter_mut().find(|t| t.table == *table))
        else {
            continue;
        };
        match d {
            Dependent::Relationship { name, .. } => {
                t.object_relationships.retain(|r| r.name != *name);
                t.array_relationships.retain(|r| r.name != *name);
            }
            Dependent::RemoteRelationship { name, .. } => {
                t.remote_relationships
                    .retain(|r| r.get("name").and_then(Value::as_str) != Some(name));
            }
            Dependent::Permission { role, kind, .. } => match *kind {
                "select" => t.select_permissions.retain(|p| p.role != *role),
                "insert" => t.insert_permissions.retain(|p| p.role != *role),
                "update" => t.update_permissions.retain(|p| p.role != *role),
                _ => t.delete_permissions.retain(|p| p.role != *role),
            },
            Dependent::Function { .. } | Dependent::RemoteSchemaRelationship { .. } => {}
        }
    }
}

/// The name of a tracked function (`functions[].function`).
fn function_name(entry: &Value) -> Option<QualifiedTable> {
    parse_table(entry.get("function")?, "$").ok()
}

/// The function of a computed field (`computed_fields[].definition.function`).
fn computed_field_function(entry: &TableEntry, name: &str) -> Option<QualifiedTable> {
    let cf = entry
        .computed_fields
        .iter()
        .find(|cf| cf.get("name").and_then(Value::as_str) == Some(name))?;
    parse_table(cf.get("definition")?.get("function")?, "$").ok()
}

/// The table a relationship points at. Forward foreign keys are looked up in `catalog`.
pub(crate) fn relationship_target(
    entry: &TableEntry,
    name: &str,
    catalog: Option<&SourceCatalog>,
) -> Option<QualifiedTable> {
    if let Some(r) = entry.object_relationships.iter().find(|r| r.name == name) {
        return match &r.using {
            ObjectRelUsing::ForeignKey {
                foreign_key_constraint_on: fk,
            } => {
                let columns: Vec<String> = match fk {
                    ObjectFkOn::Remote(remote) => return Some(remote.table.clone()),
                    ObjectFkOn::Column(c) => vec![c.clone()],
                    ObjectFkOn::Columns(cs) => cs.clone(),
                };
                let table = catalog?.table(&entry.table)?;
                table
                    .foreign_keys
                    .iter()
                    .find(|fk| {
                        fk.columns.len() == columns.len()
                            && columns
                                .iter()
                                .all(|c| fk.columns.iter().any(|(l, _)| l == c))
                    })
                    .map(|fk| fk.ref_table.clone())
            }
            ObjectRelUsing::Manual {
                manual_configuration,
            } => Some(manual_configuration.remote_table.clone()),
        };
    }
    let r = entry.array_relationships.iter().find(|r| r.name == name)?;
    Some(match &r.using {
        ArrayRelUsing::ForeignKey {
            foreign_key_constraint_on,
        } => foreign_key_constraint_on.table.clone(),
        ArrayRelUsing::Manual {
            manual_configuration,
        } => manual_configuration.remote_table.clone(),
    })
}

/// What the boolean expressions of a permission use.
#[derive(Default)]
pub(crate) struct BoolExpUses {
    /// `(table, relationship)` pairs traversed.
    pub relationships: BTreeSet<(QualifiedTable, String)>,
    /// Tables reached through relationships or `_exists`.
    pub tables: BTreeSet<QualifiedTable>,
    /// Functions of the computed fields compared (their result tables are not looked up).
    pub functions: BTreeSet<QualifiedTable>,
}

fn bool_exp_uses(
    source: &Source,
    catalog: Option<&SourceCatalog>,
    table: &QualifiedTable,
    exp: &Value,
    uses: &mut BoolExpUses,
) {
    let Value::Object(obj) = exp else {
        return;
    };
    let entry = source.tables.iter().find(|t| t.table == *table);
    for (key, value) in obj {
        match key.as_str() {
            "_and" | "$and" | "_or" | "$or" => match value {
                Value::Array(items) => {
                    for i in items {
                        bool_exp_uses(source, catalog, table, i, uses);
                    }
                }
                other => bool_exp_uses(source, catalog, table, other, uses),
            },
            "_not" | "$not" => bool_exp_uses(source, catalog, table, value, uses),
            "_exists" => {
                let Some(t) = value.get("_table").and_then(|t| parse_table(t, "$").ok()) else {
                    continue;
                };
                uses.tables.insert(t.clone());
                if let Some(w) = value.get("_where") {
                    bool_exp_uses(source, catalog, &t, w, uses);
                }
            }
            name => {
                let Some(entry) = entry else {
                    continue;
                };
                if let Some(target) = relationship_target(entry, name, catalog) {
                    uses.relationships.insert((table.clone(), name.to_owned()));
                    uses.tables.insert(target.clone());
                    bool_exp_uses(source, catalog, &target, value, uses);
                } else if let Some(f) = computed_field_function(entry, name) {
                    uses.functions.insert(f);
                }
            }
        }
    }
}

/// Every permission of a source with what its boolean expressions use.
fn permission_uses(
    source: &Source,
    catalog: Option<&SourceCatalog>,
) -> Vec<(Dependent, BoolExpUses)> {
    let mut out = vec![];
    for t in &source.tables {
        let mut push = |role: &str, kind: &'static str, exps: Vec<&Value>| {
            let mut uses = BoolExpUses::default();
            for e in exps {
                bool_exp_uses(source, catalog, &t.table, e, &mut uses);
            }
            out.push((
                Dependent::Permission {
                    source: source.name.clone(),
                    table: t.table.clone(),
                    role: role.to_owned(),
                    kind,
                },
                uses,
            ));
        };
        for p in &t.insert_permissions {
            push(&p.role, "insert", vec![&p.permission.check]);
        }
        for p in &t.select_permissions {
            push(&p.role, "select", vec![&p.permission.filter]);
        }
        for p in &t.update_permissions {
            let mut exps = vec![&p.permission.filter];
            exps.extend(p.permission.check.as_ref());
            push(&p.role, "update", exps);
        }
        for p in &t.delete_permissions {
            push(&p.role, "delete", vec![&p.permission.filter]);
        }
    }
    out
}

/// Does a `to_source` remote relationship definition point at `source` (and `table`)?
fn to_source_hits(def: &Value, source: &str, table: Option<&QualifiedTable>) -> bool {
    let Some(to) = def.get("to_source") else {
        return false;
    };
    if to.get("source").and_then(Value::as_str) != Some(source) {
        return false;
    }
    table.is_none_or(|table| {
        to.get("table")
            .and_then(|v| parse_table(v, "$").ok())
            .is_some_and(|t| t == *table)
    })
}

/// Remote relationships pointing at `table` of `source` (or, without a table, at the
/// source from other sources), from tables and from remote schemas.
fn remote_relationship_dependents(
    md: &Metadata,
    source: &str,
    table: Option<&QualifiedTable>,
) -> Vec<Dependent> {
    let mut out = vec![];
    for s in &md.sources {
        if table.is_none() && s.name == source {
            continue;
        }
        for t in &s.tables {
            for rr in &t.remote_relationships {
                if to_source_hits(rr.get("definition").unwrap_or(rr), source, table) {
                    out.push(Dependent::RemoteRelationship {
                        source: s.name.clone(),
                        table: t.table.clone(),
                        name: str_field(rr, "name"),
                    });
                }
            }
        }
    }
    for (rs, type_name, rel) in remote_schema_relationships(md) {
        if rel
            .get("definition")
            .is_some_and(|d| to_source_hits(d, source, table))
        {
            out.push(Dependent::RemoteSchemaRelationship {
                remote_schema: rs.to_owned(),
                type_name: type_name.to_owned(),
                name: str_field(rel, "name"),
            });
        }
    }
    out
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Every remote schema relationship: `(remote schema, type name, relationship)`.
fn remote_schema_relationships(md: &Metadata) -> Vec<(&str, &str, &Value)> {
    let mut out = vec![];
    for rs in &md.remote_schemas {
        let name = rs.get("name").and_then(Value::as_str).unwrap_or_default();
        let types = rs.get("remote_relationships").and_then(Value::as_array);
        for t in types.into_iter().flatten() {
            let type_name = t
                .get("type_name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let rels = t.get("relationships").and_then(Value::as_array);
            for rel in rels.into_iter().flatten() {
                out.push((name, type_name, rel));
            }
        }
    }
    out
}

/// Objects outside `table` that depend on it. `returning` names the functions of the
/// source's database that return rows of `table`.
pub(crate) fn table_dependents(
    md: &Metadata,
    source: &Source,
    catalog: Option<&SourceCatalog>,
    table: &QualifiedTable,
    returning: &BTreeSet<QualifiedTable>,
) -> Vec<Dependent> {
    let mut out = vec![];
    for t in source.tables.iter().filter(|t| t.table != *table) {
        let names = t
            .object_relationships
            .iter()
            .map(|r| &r.name)
            .chain(t.array_relationships.iter().map(|r| &r.name));
        for name in names {
            if relationship_target(t, name, catalog).as_ref() == Some(table) {
                out.push(Dependent::Relationship {
                    source: source.name.clone(),
                    table: t.table.clone(),
                    name: name.clone(),
                });
            }
        }
    }
    for (perm, uses) in permission_uses(source, catalog) {
        let on_table = perm.table().is_some_and(|(_, t)| t == table);
        if !on_table && (uses.tables.contains(table) || !uses.functions.is_disjoint(returning)) {
            out.push(perm);
        }
    }
    // Computed fields returning the table are no dependents in V2 (a computed field of
    // another table is left to become inconsistent).
    for f in source.functions.iter().filter_map(function_name) {
        if returning.contains(&f) {
            out.push(Dependent::Function {
                source: source.name.clone(),
                function: f,
            });
        }
    }
    out.extend(
        remote_relationship_dependents(md, &source.name, Some(table))
            .into_iter()
            .filter(|d| d.table() != Some((source.name.as_str(), table))),
    );
    out
}

/// Functions of the source database returning rows of a table (`SETOF t` or `t`), as JSON.
const FUNCTIONS_RETURNING_SQL: &str = "\
SELECT coalesce(json_agg(json_build_object('schema', n.nspname, 'name', p.proname)), '[]')::text \
FROM pg_catalog.pg_proc p \
JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
JOIN pg_catalog.pg_type t ON t.oid = p.prorettype \
JOIN pg_catalog.pg_class c ON c.oid = t.typrelid \
JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace \
WHERE cn.nspname = $1 AND c.relname = $2";

/// The functions returning `table`, when the source tracks any functions or computed fields
/// (otherwise none can matter).
async fn functions_returning<R: Runtime>(
    ctx: &Ctx<'_, R>,
    source: &Source,
    table: &QualifiedTable,
) -> ApiResult<BTreeSet<QualifiedTable>> {
    let relevant =
        !source.functions.is_empty() || source.tables.iter().any(|t| !t.computed_fields.is_empty());
    let Some(pg) = relevant
        .then(|| ctx.rt.source(ctx.prepared.as_ref(), &source.name))
        .flatten()
    else {
        return Ok(BTreeSet::new());
    };
    let text = pg
        .query_text(
            FUNCTIONS_RETURNING_SQL,
            &[Some(&table.schema), Some(&table.name)],
        )
        .await?;
    let rows: Vec<QualifiedTable> = text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    Ok(rows.into_iter().collect())
}

/// Permissions that traverse the relationship `name` of `table`.
pub(crate) fn relationship_dependents(
    source: &Source,
    catalog: Option<&SourceCatalog>,
    table: &QualifiedTable,
    name: &str,
) -> Vec<Dependent> {
    let key = (table.clone(), name.to_owned());
    permission_uses(source, catalog)
        .into_iter()
        .filter(|(_, uses)| uses.relationships.contains(&key))
        .map(|(perm, _)| perm)
        .collect()
}

// ---------------------------------------------------------------------------------------
// Sources

fn source_config(args: &Args<'_>) -> ApiResult<SourceConfiguration> {
    let mut config: SourceConfiguration = args.req("configuration")?;
    config.fill_pg_defaults();
    Ok(config)
}

/// Source-level keys `pg_add_source` / `pg_update_source` take besides `configuration` and
/// `customization`, kept as they are given. (`query_tags` belongs to `set_query_tags`; V2's
/// source commands ignore it.)
const SOURCE_EXTRA_KEYS: &[&str] = &["health_check"];

/// A source-level key parsed as V2 does; `None` for `null`, which V2 ignores.
fn source_extra(a: &Args<'_>, key: &str) -> ApiResult<Option<Value>> {
    let Some(value) = a.raw(key) else {
        return Ok(None);
    };
    match key {
        "health_check" => health_check(value, &a.field_path(key)).map(Some),
        _ => Ok(Some(value.clone())),
    }
}

/// V2's `HealthCheckConfig`: `interval` required, `test.sql`, `retries`, `retry_interval` and
/// `timeout` defaulted, unknown keys dropped.
fn health_check(value: &Value, path: &str) -> ApiResult<Value> {
    let h = Args::at(value, path)?;
    let seconds = |key: &str, default: Option<Value>| -> ApiResult<Value> {
        match h.raw(key) {
            Some(v @ Value::Number(_)) => Ok(v.clone()),
            Some(other) => Err(parse_failed(
                &h.field_path(key),
                format!(
                    "parsing DiffTime failed, expected Number, but encountered {}",
                    json_kind(other)
                ),
            )),
            None => default.ok_or_else(|| h.missing(key)),
        }
    };
    let interval = seconds("interval", None)?;
    let retries = match h.raw("retries") {
        None => json!(3),
        Some(Value::Number(n)) if n.is_i64() => json!(n),
        Some(Value::Number(n)) => {
            return Err(parse_failed(
                &h.field_path("retries"),
                format!(
                    "parsing Int failed, value is either floating or will cause over or underflow {n}"
                ),
            ));
        }
        Some(other) => {
            return Err(parse_failed(
                &h.field_path("retries"),
                format!(
                    "parsing Int failed, expected Number, but encountered {}",
                    json_kind(other)
                ),
            ));
        }
    };
    let retry_interval = seconds("retry_interval", Some(json!(10)))?;
    let timeout = seconds("timeout", Some(json!(10)))?;
    let sql = match h.raw("test") {
        None => json!("SELECT 1"),
        Some(test) => {
            let t = Args::at(test, &h.field_path("test"))?;
            match t.raw("sql") {
                None => json!("SELECT 1"),
                Some(v @ Value::String(_)) => v.clone(),
                Some(other) => {
                    return Err(parse_failed(
                        &t.field_path("sql"),
                        format!(
                            "parsing Text failed, expected String, but encountered {}",
                            json_kind(other)
                        ),
                    ));
                }
            }
        }
    };
    Ok(json!({
        "interval": interval,
        "retries": retries,
        "retry_interval": retry_interval,
        "test": {"sql": sql},
        "timeout": timeout,
    }))
}

async fn add_source<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let name = a.non_empty("name")?;
    let configuration = source_config(&a)?;
    let replace = a.bool_or("replace_configuration", false)?;
    let customization: Option<SourceCustomization> = a.opt("customization")?;
    let mut extra = Map::new();
    for key in SOURCE_EXTRA_KEYS {
        if let Some(v) = source_extra(&a, key)? {
            extra.insert((*key).to_owned(), v);
        }
    }
    match ctx.metadata.source_mut(&name) {
        Some(_) if !replace => {
            return Err(already_exists(format!(
                "source with name \"{name}\" already exists"
            )));
        }
        // V2's `replace_configuration` replaces the configuration and customization only:
        // the stored health check (and anything else) stays, whatever the request says.
        Some(existing) => {
            existing.configuration = configuration;
            existing.customization = customization;
        }
        None => ctx.metadata.sources.push(Source {
            name: name.clone(),
            kind: "postgres".to_owned(),
            tables: vec![],
            functions: vec![],
            configuration,
            customization,
            extra,
        }),
    }
    ctx.reload.merge(Reload::Sources([name.clone()].into()));
    ctx.check(Check::object("source", format!("source {name}")))
        .await?;
    Ok(success())
}

async fn drop_source<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let name = a.non_empty("name")?;
    let cascade = a.bool_or("cascade", false)?;
    let dropped = source_ref(&ctx.metadata, &name)?.clone();
    let dependents = remote_relationship_dependents(&ctx.metadata, &name, None);
    if !dependents.is_empty() {
        if !cascade {
            return Err(dependency_error(&dependents));
        }
        drop_dependents(&mut ctx.metadata, &dependents);
    }
    ctx.metadata.sources.retain(|s| s.name != name);
    let pg = ctx.rt.source(ctx.prepared.as_ref(), &name);
    ctx.check(Check::NoNew).await?;
    // V2's `postDropSourceHook`: remove what the engine created in the database.
    if let Some(pg) = pg {
        ctx.rt.post_drop_source(&dropped, pg).await;
    }
    Ok(success())
}

async fn update_source<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let name = a.non_empty("name")?;
    let configuration = match a.raw("configuration") {
        Some(_) => Some(source_config(&a)?),
        None => None,
    };
    let customization: Option<SourceCustomization> = a.opt("customization")?;
    let mut extra = vec![];
    for key in SOURCE_EXTRA_KEYS {
        if let Some(v) = source_extra(&a, key)? {
            extra.push(((*key).to_owned(), v));
        }
    }
    let source = source_mut(&mut ctx.metadata, &name)?;
    if let Some(c) = configuration {
        source.configuration = c;
    }
    if let Some(c) = customization {
        source.customization = Some(c);
    }
    source.extra.extend(extra);
    ctx.reload.merge(Reload::Sources([name.clone()].into()));
    ctx.check(Check::object("source", format!("source {name}")))
        .await?;
    Ok(success())
}

async fn rename_source<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let prefixed = |e: ApiError| {
        let mut e = e;
        e.error.message = format!(
            "parsing Hasura.RQL.DDL.Schema.Source.RenameSource(RenameSource) failed, {}",
            e.error.message
        );
        e
    };
    let name = a.string("name").map_err(prefixed)?;
    let name = a.check_non_empty("name", name)?;
    let new_name = a.string("new_name").map_err(prefixed)?;
    let new_name = a.check_non_empty("new_name", new_name)?;
    if ctx.metadata.source(&name).is_none() {
        return Err(not_exists(format!(
            "Could not find source with name \"{name}\""
        )));
    }
    if ctx.metadata.source(&new_name).is_some() {
        return Err(already_exists(format!(
            "Source with name \"{new_name}\" already exists"
        )));
    }
    // Like V2, only the source is renamed: references to it elsewhere are left as they are.
    if let Some(s) = ctx.metadata.source_mut(&name) {
        s.name = new_name.clone();
    }
    ctx.reload.merge(Reload::Sources([new_name.clone()].into()));
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Tracking

struct TrackArgs {
    source: String,
    table: QualifiedTable,
    is_enum: bool,
    configuration: TableConfig,
    apollo_federation_config: Option<Value>,
}

fn track_args(args: &Value, path: &str) -> ApiResult<TrackArgs> {
    let a = Args::at(args, path)?;
    // V2's `withOptions <|> withoutOptions`: when the options form does not parse, the
    // arguments are taken to be the table name itself (and that parse's error is reported).
    let with_options = || -> ApiResult<TrackArgs> {
        Ok(TrackArgs {
            source: a.source()?,
            table: a.table("table")?,
            is_enum: a.bool_or("is_enum", false)?,
            configuration: a.opt("configuration")?.unwrap_or_default(),
            apollo_federation_config: a.raw("apollo_federation_config").cloned(),
        })
    };
    match with_options() {
        Ok(parsed) => Ok(parsed),
        // The legacy form is the table name alone: no source, options or configuration.
        Err(_) => Ok(TrackArgs {
            source: "default".to_owned(),
            table: parse_table(args, path)?,
            is_enum: false,
            configuration: TableConfig::default(),
            apollo_federation_config: None,
        }),
    }
}

fn add_table(md: &mut Metadata, t: TrackArgs) -> ApiResult<()> {
    let source = source_mut(md, &t.source)?;
    if source.tables.iter().any(|e| e.table == t.table) {
        return Err(ApiError::bad(
            Code::AlreadyTracked,
            format!("view/table already tracked: \"{}\"", t.table.v2_text()),
        ));
    }
    let mut entry = TableEntry::new(t.table);
    entry.is_enum = t.is_enum;
    entry.configuration = t.configuration;
    if let Some(apollo) = t.apollo_federation_config {
        entry
            .extra
            .insert("apollo_federation_config".to_owned(), apollo);
    }
    source.tables.push(entry);
    Ok(())
}

async fn track_table<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let t = track_args(args, "$.args")?;
    let name = table_object_name(&t.source, &t.table);
    add_table(&mut ctx.metadata, t)?;
    ctx.check(Check::object("table", name)).await?;
    Ok(success())
}

/// The `tables` list of `pg_track_tables` / `pg_untrack_tables`.
fn table_list<'a>(a: &Args<'a>) -> ApiResult<&'a Vec<Value>> {
    match a.raw("tables") {
        Some(Value::Array(items)) => Ok(items),
        Some(other) => Err(parse_failed(
            &a.field_path("tables"),
            format!(
                "parsing [] failed, expected Array, but encountered {}",
                json_kind(other)
            ),
        )),
        None => Err(a.missing("tables")),
    }
}

/// The response of `pg_track_tables` / `pg_untrack_tables` given the failed items.
fn bulk_result(
    warnings: Vec<Value>,
    succeeded: usize,
    allow_warnings: bool,
    what: &str,
) -> ApiResult<Value> {
    if warnings.is_empty() {
        return Ok(success());
    }
    if succeeded == 0 {
        return Err(ApiError::bad(
            Code::InvalidConfiguration,
            format!("all tables failed to {what}"),
        )
        .with_internal(Value::Array(warnings)));
    }
    if !allow_warnings {
        return Err(
            ApiError::bad(Code::MetadataWarnings, "failed due to metadata warnings")
                .with_internal(Value::Array(warnings)),
        );
    }
    Ok(json!({"message": "success", "warnings": warnings}))
}

fn bulk_warning(code: &str, name: &str, message: &str) -> Value {
    json!({"code": code, "message": message, "name": name, "type": "table"})
}

async fn track_tables<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let allow_warnings = a.bool_or("allow_warnings", true)?;
    let parsed = table_list(&a)?
        .iter()
        .enumerate()
        .map(|(i, item)| track_args(item, &format!("$.args.tables[{i}]")))
        .collect::<ApiResult<Vec<_>>>()?;
    let mut warnings = vec![];
    let mut succeeded = 0;
    for t in parsed {
        let name = table_object_name(&t.source, &t.table);
        let snapshot = ctx.snapshot();
        let result = match add_table(&mut ctx.metadata, t) {
            Ok(()) => ctx.check(Check::object("table", name.clone())).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => succeeded += 1,
            Err(e) => {
                ctx.restore(snapshot);
                warnings.push(bulk_warning("track-table-failed", &name, &e.error.message));
            }
        }
    }
    bulk_result(warnings, succeeded, allow_warnings, "track")
}

struct UntrackArgs {
    source: String,
    table: QualifiedTable,
    cascade: bool,
}

fn untrack_args(args: &Value, path: &str) -> ApiResult<UntrackArgs> {
    let a = Args::at(args, path)?;
    Ok(UntrackArgs {
        source: a.source()?,
        table: a.table("table")?,
        cascade: a.bool_or("cascade", false)?,
    })
}

/// V2's first untrack phases: the table must be tracked; its dependents, which the
/// caller rejects unless cascading.
async fn untrack_dependents<R: Runtime>(
    ctx: &Ctx<'_, R>,
    u: &UntrackArgs,
) -> ApiResult<Vec<Dependent>> {
    let cat = catalog(ctx, &u.source);
    let source = source_ref(&ctx.metadata, &u.source)?;
    if !source.tables.iter().any(|t| t.table == u.table) {
        return Err(ApiError::bad(
            Code::AlreadyUntracked,
            format!("view/table already untracked: \"{}\"", u.table.v2_text()),
        ));
    }
    let returning = functions_returning(ctx, source, &u.table).await?;
    Ok(table_dependents(
        &ctx.metadata,
        source,
        cat.as_deref(),
        &u.table,
        &returning,
    ))
}

/// Remove the table and its dependents from the metadata.
fn apply_untrack(md: &mut Metadata, u: &UntrackArgs, dependents: &[Dependent]) -> ApiResult<()> {
    drop_dependents(md, dependents);
    source_mut(md, &u.source)?
        .tables
        .retain(|t| t.table != u.table);
    Ok(())
}

async fn untrack_table<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let u = untrack_args(args, "$.args")?;
    let dependents = untrack_dependents(ctx, &u).await?;
    if !dependents.is_empty() && !u.cascade {
        return Err(dependency_error(&dependents));
    }
    apply_untrack(&mut ctx.metadata, &u, &dependents)?;
    ctx.check(Check::NoNew).await.map_err(newly_found)?;
    Ok(success())
}

/// V2's `untrack_table` rebuilds under `withNewInconsistentObjsCheck`, which words the new
/// inconsistencies error differently and answers 500 (bulk untracking does not).
fn newly_found(mut e: ApiError) -> ApiError {
    if e.error.message == "cannot continue due to new inconsistent metadata" {
        e.status = 500;
        e.error.message = "cannot continue due to newly found inconsistent metadata".into();
    }
    e
}

async fn untrack_tables<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let allow_warnings = a.bool_or("allow_warnings", true)?;
    let parsed = table_list(&a)?
        .iter()
        .enumerate()
        .map(|(i, item)| untrack_args(item, &format!("$.args.tables[{i}]")))
        .collect::<ApiResult<Vec<_>>>()?;
    // V2's `runUntrackTablesQ`: per-table checks, then dependents (those on tables untracked
    // in the same request do not count), then one rebuild without the tables that became
    // inconsistent themselves; any other new inconsistency fails the whole request.
    let mut warnings = vec![];
    let warn = |warnings: &mut Vec<Value>, u: &UntrackArgs, message: &str| {
        let name = table_object_name(&u.source, &u.table);
        warnings.push(bulk_warning("untrack-table-failed", &name, message));
    };
    let mut checked = vec![];
    for u in parsed {
        match untrack_dependents(ctx, &u).await {
            Ok(dependents) => checked.push((u, dependents)),
            Err(e) => warn(&mut warnings, &u, &e.error.message),
        }
    }
    let untracking: BTreeSet<(String, QualifiedTable)> = checked
        .iter()
        .map(|(u, _)| (u.source.clone(), u.table.clone()))
        .collect();
    let mut planned = vec![];
    for (u, dependents) in checked {
        let outside = dependents.iter().any(|d| {
            d.table()
                .is_none_or(|(s, t)| !untracking.contains(&(s.to_owned(), t.clone())))
        });
        if outside && !u.cascade {
            warn(
                &mut warnings,
                &u,
                &dependency_error(&dependents).error.message,
            );
        } else {
            planned.push((u, dependents));
        }
    }
    let mut retried = false;
    while !planned.is_empty() {
        let snapshot = ctx.snapshot();
        for (u, dependents) in &planned {
            apply_untrack(&mut ctx.metadata, u, dependents)?;
        }
        let Err(e) = ctx.check(Check::NoNew).await else {
            break;
        };
        ctx.restore(snapshot);
        if e.error.message != "cannot continue due to new inconsistent metadata" {
            return Err(e);
        }
        let objects = e
            .error
            .internal
            .as_deref()
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let reasons = |u: &UntrackArgs| -> Vec<String> {
            let name = table_object_name(&u.source, &u.table);
            objects
                .iter()
                .filter(|o| o["type"] == "table" && o["name"] == name.as_str())
                .map(|o| {
                    let reason = o["reason"].as_str().unwrap_or_default();
                    reason
                        .strip_prefix("Inconsistent object: ")
                        .unwrap_or(reason)
                        .to_owned()
                })
                .collect()
        };
        let failing: Vec<bool> = planned
            .iter()
            .map(|(u, _)| !reasons(u).is_empty())
            .collect();
        if retried || !failing.contains(&true) {
            return Err(ApiError::bad(
                Code::Unexpected,
                "cannot continue due to newly found inconsistent metadata",
            )
            .with_internal(Value::Array(objects)));
        }
        retried = true;
        let mut kept = vec![];
        for ((u, dependents), failed) in planned.into_iter().zip(failing) {
            if failed {
                warn(&mut warnings, &u, &reasons(&u).join(", "));
            } else {
                kept.push((u, dependents));
            }
        }
        planned = kept;
    }
    let succeeded = planned.len();
    // V2 reports warnings newest first.
    warnings.reverse();
    bulk_result(warnings, succeeded, allow_warnings, "untrack")
}

async fn set_table_customization<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let configuration: TableConfig = a.opt("configuration")?.unwrap_or_default();
    table_mut(&mut ctx.metadata, &source, &table)?.configuration = configuration;
    ctx.check(Check::object("table", table_object_name(&source, &table)))
        .await?;
    Ok(success())
}

/// V2's deprecated `set_table_custom_fields` (`/v1/query` version 2): the table's whole
/// configuration becomes the given root fields and column names.
async fn set_table_custom_fields<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let root_fields: Value = a.opt("custom_root_fields")?.unwrap_or_else(|| json!({}));
    let names: BTreeMap<String, String> = a.opt("custom_column_names")?.unwrap_or_default();
    let column_config: Map<String, Value> = names
        .into_iter()
        .map(|(column, name)| (column, json!({"custom_name": name})))
        .collect();
    let configuration: TableConfig = serde_json::from_value(json!({
        "custom_root_fields": root_fields,
        "column_config": column_config,
    }))
    .map_err(|e| parse_failed(&a.field_path("custom_root_fields"), e.to_string()))?;
    table_mut(&mut ctx.metadata, &source, &table)?.configuration = configuration;
    ctx.check(Check::object("table", table_object_name(&source, &table)))
        .await?;
    Ok(success())
}

async fn set_table_is_enum<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let is_enum: bool = a.req("is_enum")?;
    table_mut(&mut ctx.metadata, &source, &table)?.is_enum = is_enum;
    ctx.check(Check::object("table", table_object_name(&source, &table)))
        .await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Read-only

/// Every table, view and foreign table of a source, ordered by schema and name.
const SOURCE_TABLES_SQL: &str = "\
SELECT coalesce(json_agg(json_build_object('name', c.relname, 'schema', n.nspname) \
         ORDER BY n.nspname COLLATE \"C\", c.relname COLLATE \"C\"), '[]')::text \
FROM pg_catalog.pg_class c \
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
WHERE c.relkind IN ('r', 'v', 'm', 'f', 'p') \
  AND n.nspname NOT IN ('information_schema', 'pg_catalog', 'hdb_catalog') \
  AND n.nspname NOT LIKE 'pg\\_%'";

async fn get_source_tables<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.string("source")?;
    source_ref(&ctx.metadata, &source)?;
    let pg = ctx
        .rt
        .source(ctx.prepared.as_ref(), &source)
        .ok_or_else(|| source_not_found(&source))?;
    let text = pg.query_text(SOURCE_TABLES_SQL, &[]).await?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!([])))
}

/// Column comments of one table, as a JSON object (as text).
const COLUMN_COMMENTS_SQL: &str = "\
SELECT coalesce(json_object_agg(a.attname, col_description(a.attrelid, a.attnum)), '{}')::text \
FROM pg_catalog.pg_attribute a \
WHERE a.attrelid = $1::regclass AND a.attnum > 0 AND NOT a.attisdropped";

fn hge_postgres_quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

async fn get_table_info<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.string("source")?;
    let table = a.table("table")?;
    source_ref(&ctx.metadata, &source)?;
    let pg = ctx
        .rt
        .source(ctx.prepared.as_ref(), &source)
        .ok_or_else(|| source_not_found(&source))?;
    let cat = pg
        .introspect(std::slice::from_ref(&table), &[], &[], &[])
        .await?;
    let Some(t) = cat.table(&table) else {
        return Ok(Value::Null);
    };
    let qualified = format!(
        "{}.{}",
        hge_postgres_quote(&table.schema),
        hge_postgres_quote(&table.name)
    );
    let descriptions: Map<String, Value> = pg
        .query_text(COLUMN_COMMENTS_SQL, &[Some(&qualified)])
        .await?
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let columns: Vec<Value> = t
        .columns
        .values()
        .map(|c| {
            let mut obj = Map::new();
            obj.insert("name".into(), json!(c.name));
            obj.insert("type".into(), json!(c.ty.v2_text()));
            obj.insert("nullable".into(), json!(c.nullable));
            if let Some(d) = c
                .description
                .as_ref()
                .map(|d| json!(d))
                .or_else(|| descriptions.get(&c.name).filter(|d| !d.is_null()).cloned())
            {
                obj.insert("description".into(), d);
            }
            let mutable = c.is_mutable();
            obj.insert("insertable".into(), json!(mutable));
            obj.insert("updatable".into(), json!(mutable));
            Value::Object(obj)
        })
        .collect();
    let mut out = Map::new();
    out.insert(
        "name".into(),
        json!({"schema": table.schema, "name": table.name}),
    );
    let kind = match t.kind {
        hge_core::TableKind::View | hge_core::TableKind::MaterializedView => "view",
        _ => "table",
    };
    out.insert("type".into(), json!(kind));
    out.insert("columns".into(), Value::Array(columns));
    if let Some(pk) = &t.primary_key {
        out.insert("primary_key".into(), json!(pk));
    }
    if !t.foreign_keys.is_empty() {
        let mut sorted: Vec<_> = t.foreign_keys.iter().collect();
        sorted.sort_by(|a, b| a.constraint.cmp(&b.constraint));
        let mut fks = Map::new();
        for fk in sorted {
            let mapping: Map<String, Value> = fk
                .columns
                .iter()
                .map(|(l, r)| (l.clone(), json!(r)))
                .collect();
            fks.insert(
                fk.constraint.clone(),
                json!({
                    "foreign_table": {"schema": fk.ref_table.schema, "name": fk.ref_table.name},
                    "column_mapping": mapping,
                }),
            );
        }
        out.insert("foreign_keys".into(), Value::Object(fks));
    }
    out.insert("insertable".into(), json!(t.mutability.insertable));
    out.insert("updatable".into(), json!(t.mutability.updatable));
    out.insert("deletable".into(), json!(t.mutability.deletable));
    Ok(Value::Object(out))
}

/// The backend-agnostic `get_table_info` is for data connector sources only.
pub async fn get_table_info_agnostic<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.string("source")?;
    let kind = ctx
        .metadata
        .source(&source)
        .ok_or_else(|| {
            ApiError::bad(
                Code::DataConnectorError,
                format!("Source '{source}' not found"),
            )
        })?
        .kind
        .clone();
    let kind = if kind == "pg" {
        "postgres".into()
    } else {
        kind
    };
    Err(ApiError::new(
        500,
        Code::Unexpected,
        format!("Schema fetching is not supported for '{kind}'"),
    ))
}

fn get_source_kind_capabilities(args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let name = a.string("name")?;
    Err(ApiError::bad(
        Code::DataConnectorError,
        format!("{name} does not support Capabilities"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(extra: Value) -> Metadata {
        let mut doc = json!({
            "version": 3,
            "sources": [
                {"name": "default", "kind": "postgres",
                 "configuration": {"connection_info": {"database_url": "postgres://x/a"}},
                 "tables": [
                    {"table": {"schema": "public", "name": "author"}},
                    {"table": {"schema": "public", "name": "article"},
                     "computed_fields": [{"name": "auth", "definition": {"function": "article_author"}}],
                     "select_permissions": [
                        {"role": "r", "permission": {"columns": [], "filter": {"auth": {"id": {"_eq": 1}}}}},
                        {"role": "s", "permission": {"columns": [], "filter": {"id": {"_eq": 1}}}}
                     ]}
                 ],
                 "functions": [
                    {"function": "search_authors"},
                    {"function": {"schema": "s", "name": "one_author"}},
                    {"function": "other"}
                 ]},
                {"name": "src2", "kind": "postgres",
                 "configuration": {"connection_info": {"database_url": "postgres://x/b"}},
                 "tables": [
                    {"table": {"schema": "public", "name": "t2"},
                     "remote_relationships": [{"name": "to_author", "definition": {"to_source": {
                        "source": "default", "table": "author", "relationship_type": "object",
                        "field_mapping": {"author_id": "id"}}}}]}
                 ]}
            ]
        });
        if let (Some(doc), Value::Object(extra)) = (doc.as_object_mut(), extra) {
            doc.extend(extra);
        }
        serde_json::from_value(doc).unwrap()
    }

    fn remote_schema() -> Value {
        json!({"remote_schemas": [{
            "name": "rs",
            "definition": {"url": "http://localhost/graphql"},
            "remote_relationships": [{"type_name": "Q", "relationships": [
                {"name": "a", "definition": {"to_source": {"source": "default", "table": "author",
                    "relationship_type": "object", "field_mapping": {"id": "id"}}}},
                {"name": "b", "definition": {"to_source": {"source": "default", "table": "article",
                    "relationship_type": "object", "field_mapping": {"id": "id"}}}}
            ]}]
        }]})
    }

    fn reports(deps: &[Dependent]) -> Vec<String> {
        let mut out: Vec<String> = deps.iter().map(Dependent::report).collect();
        out.sort();
        out
    }

    #[test]
    fn table_dependents_include_functions_permissions_and_remote_relationships() {
        let md = metadata(remote_schema());
        let author = QualifiedTable::public("author");
        let returning: BTreeSet<QualifiedTable> = [
            QualifiedTable::public("search_authors"),
            QualifiedTable::new("s", "one_author"),
            QualifiedTable::public("article_author"),
        ]
        .into();
        let source = md.source("default").unwrap();
        let deps = table_dependents(&md, source, None, &author, &returning);
        assert_eq!(
            reports(&deps),
            [
                "function s.one_author in source \"default\"",
                "function search_authors in source \"default\"",
                "permission article.r.select in source \"default\"",
                "remote relationship t2.to_author in source \"src2\"",
                "remote_relationship a on type Q in remote schema rs",
            ]
        );

        let mut dropped = md.clone();
        drop_dependents(&mut dropped, &deps);
        let functions: Vec<Option<QualifiedTable>> = dropped.sources[0]
            .functions
            .iter()
            .map(function_name)
            .collect();
        assert_eq!(functions, [Some(QualifiedTable::public("other"))]);
        let article = &dropped.sources[0].tables[1];
        assert_eq!(article.select_permissions.len(), 1);
        // The computed field itself stays (V2 leaves it to become inconsistent).
        assert_eq!(article.computed_fields.len(), 1);
        assert!(dropped.sources[1].tables[0].remote_relationships.is_empty());
        // The remote schema keeps the type entry and its other relationships.
        assert_eq!(
            dropped.remote_schemas[0]["remote_relationships"],
            json!([{"type_name": "Q", "relationships": [
                {"name": "b", "definition": {"to_source": {"source": "default", "table": "article",
                    "relationship_type": "object", "field_mapping": {"id": "id"}}}}
            ]}])
        );
    }

    #[test]
    fn source_dependents_come_from_other_sources_and_remote_schemas() {
        let md = metadata(remote_schema());
        let deps = remote_relationship_dependents(&md, "default", None);
        assert_eq!(
            reports(&deps),
            [
                "remote relationship t2.to_author in source \"src2\"",
                "remote_relationship a on type Q in remote schema rs",
                "remote_relationship b on type Q in remote schema rs",
            ]
        );
        assert!(remote_relationship_dependents(&md, "src2", None).is_empty());
    }

    #[test]
    fn empty_names_are_rejected() {
        let args = json!({"name": "", "source": ""});
        let a = Args::new(&args).unwrap();
        let e = a.non_empty("name").unwrap_err();
        assert_eq!(
            (e.error.path.as_str(), e.error.message.as_str()),
            ("$.args.name", "empty string not allowed")
        );
        assert_eq!(a.source().unwrap_err().error.path, "$.args.source");
    }

    #[test]
    fn untrack_reports_new_inconsistencies_as_v2() {
        let e = newly_found(ApiError::bad(
            Code::Unexpected,
            "cannot continue due to new inconsistent metadata",
        ));
        assert_eq!(e.status, 500);
        assert_eq!(
            e.error.message,
            "cannot continue due to newly found inconsistent metadata"
        );
        let other = newly_found(ApiError::bad(Code::NotExists, "no such table"));
        assert_eq!(
            (other.status, other.error.message.as_str()),
            (400, "no such table")
        );
    }

    #[test]
    fn legacy_track_table_forms() {
        let t = track_args(&json!({"name": "article", "table": 5}), "$.args").unwrap();
        assert_eq!(
            (t.source.as_str(), t.table),
            ("default", QualifiedTable::public("article"))
        );
        let t = track_args(&json!({"table": "tag", "name": "x"}), "$.args").unwrap();
        assert_eq!(t.table, QualifiedTable::public("tag"));
        let Err(e) = track_args(&json!({"table": "tag", "source": ""}), "$.args") else {
            panic!("expected the legacy form's error");
        };
        assert_eq!(e.error.message, "key \"name\" not found");
        // A configuration only applies to the options form.
        let t = track_args(
            &json!({"name": "article", "table": 5, "configuration": {"custom_name": "a"}}),
            "$.args",
        )
        .unwrap();
        assert_eq!(t.configuration, TableConfig::default());
    }
}
