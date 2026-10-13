//! Resolved information about tracked tables: metadata joined with the database catalog,
//! independent of any role.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hge_core::{
    ColumnType, InconsistentObject, QualifiedTable, RoleName, SourceCatalog, TableCatalog,
    TableMutability, UniqueConstraint,
};
use hge_ir::TablePermissions;
use hge_metadata::{
    ArrayRelUsing, ManualRelConfig, ObjectFkOn, ObjectRelUsing, Source, TableEntry,
};
use indexmap::IndexMap;

use crate::functions::{ComputedFieldInfo, computed};

/// A column as exposed in GraphQL.
#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub gql_name: String,
    pub ty: ColumnType,
    pub nullable: bool,
    pub description: Option<String>,
    /// Can be inserted / updated (not generated, not `GENERATED ALWAYS AS IDENTITY`).
    pub mutable: bool,
    /// The enum table this column references, whose enum type it takes in GraphQL.
    pub enum_ref: Option<Arc<crate::enums::EnumReference>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelKind {
    Object,
    Array,
}

#[derive(Debug, Clone)]
pub struct RelationshipInfo {
    pub name: String,
    pub kind: RelKind,
    pub source: Arc<str>,
    pub target: QualifiedTable,
    /// `(column on this table, column on target table)`.
    pub mapping: Vec<(String, String)>,
    /// Whether the GraphQL field is nullable (object relationships only).
    pub nullable: bool,
    pub description: Option<String>,
}

/// Root field names of a table (after `custom_root_fields`).
#[derive(Debug, Clone)]
pub struct RootFieldNames {
    pub select: String,
    pub select_by_pk: String,
    pub select_aggregate: String,
    pub select_comment: Option<String>,
    pub select_by_pk_comment: Option<String>,
    pub select_aggregate_comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub source: Arc<str>,
    pub name: QualifiedTable,
    /// The GraphQL identifier of the table (`author`, `myschema_author`, or `custom_name`);
    /// type names are derived from it.
    pub gql_name: String,
    pub description: Option<String>,
    /// Columns in table order, keyed by database name.
    pub columns: IndexMap<String, ColumnInfo>,
    pub primary_key: Option<Vec<String>>,
    /// Primary key and unique indexes (`on_conflict` targets).
    pub constraints: Vec<UniqueConstraint>,
    pub mutability: TableMutability,
    pub object_relationships: Vec<Arc<RelationshipInfo>>,
    pub array_relationships: Vec<Arc<RelationshipInfo>>,
    pub computed_fields: Vec<Arc<ComputedFieldInfo>>,
    pub root_fields: RootFieldNames,
    pub is_enum: bool,
    /// A logical model (see [`crate::native`]): no table behind it, only the rows of the
    /// native queries returning it.
    pub logical_model: bool,
    /// The rows of an enum table, sorted by value (`None` unless `is_enum`).
    pub enum_values: Option<Vec<hge_core::EnumTableRow>>,
    /// The metadata entry, for permission lookups.
    pub entry: TableEntry,
    /// Resolved permissions by role (filled by [`crate::permissions::resolve_permissions`]).
    pub permissions: HashMap<RoleName, Arc<TablePermissions>>,
}

impl TableInfo {
    pub fn column_by_gql_name(&self, gql_name: &str) -> Option<&ColumnInfo> {
        self.columns.values().find(|c| c.gql_name == gql_name)
    }

    pub fn relationship_by_name(&self, name: &str) -> Option<&Arc<RelationshipInfo>> {
        self.object_relationships
            .iter()
            .chain(self.array_relationships.iter())
            .find(|r| r.name == name)
    }

    pub fn computed_field(&self, name: &str) -> Option<&Arc<ComputedFieldInfo>> {
        self.computed_fields.iter().find(|c| c.name == name)
    }

    pub fn type_name(&self) -> &str {
        &self.gql_name
    }

    pub fn bool_exp_name(&self) -> String {
        if self.logical_model {
            // Sic: V2 names it so.
            format!("{}_bool_exp_bool_exp", self.gql_name)
        } else {
            format!("{}_bool_exp", self.gql_name)
        }
    }

    pub fn order_by_name(&self) -> String {
        format!("{}_order_by", self.gql_name)
    }

    pub fn select_column_name(&self) -> String {
        if self.logical_model {
            format!("{}_enum_name", self.gql_name)
        } else {
            format!("{}_select_column", self.gql_name)
        }
    }

    /// The table name as V2 prints it in descriptions (schema omitted when `public`).
    pub fn display_name(&self) -> String {
        if self.logical_model {
            self.name.name.clone()
        } else {
            display_name(&self.name)
        }
    }
}

/// A description from a metadata comment, as V2 resolves its `Comment`: an explicit comment
/// wins, and an empty one means no description; without one, `default`.
pub(crate) fn comment_or(
    comment: Option<&str>,
    default: impl FnOnce() -> Option<String>,
) -> Option<String> {
    match comment {
        Some("") => None,
        Some(c) => Some(c.to_owned()),
        None => default(),
    }
}

/// How V2 names a table in descriptions: unqualified in `public`, else `schema.name`.
fn display_name(table: &QualifiedTable) -> String {
    if table.schema == "public" {
        table.name.clone()
    } else {
        format!("{}.{}", table.schema, table.name)
    }
}

/// All tracked tables of all sources.
#[derive(Debug, Clone, Default)]
pub struct Tables {
    pub by_name: HashMap<(Arc<str>, QualifiedTable), Arc<TableInfo>>,
    /// Tables in metadata order.
    pub ordered: Vec<Arc<TableInfo>>,
    /// Tracked functions of all sources, in metadata order.
    pub functions: Vec<Arc<crate::functions::FunctionInfo>>,
    /// Logical models of all sources (also in `by_name`, but not in `ordered`: they have no
    /// root fields of their own).
    pub logical_models: Vec<Arc<TableInfo>>,
    /// Native queries of all sources, in metadata order.
    pub native_queries: Vec<Arc<crate::native::NativeQueryInfo>>,
}

impl Tables {
    /// A tracked table (not a logical model).
    pub fn get(&self, source: &str, name: &QualifiedTable) -> Option<&Arc<TableInfo>> {
        self.by_name
            .get(&(Arc::from(source), name.clone()))
            .filter(|t| !t.logical_model)
    }

    /// A logical model, by its pseudo-table name ([`crate::native::logical_model_name`]).
    pub fn logical_model(&self, source: &str, name: &QualifiedTable) -> Option<&Arc<TableInfo>> {
        self.by_name
            .get(&(Arc::from(source), name.clone()))
            .filter(|t| t.logical_model)
    }
}

/// Is `s` a valid GraphQL name (`/[_A-Za-z][_0-9A-Za-z]*/`)?
pub fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Resolve the tables of one source. Tables missing from the catalog, or whose names are not
/// valid GraphQL names, are reported as inconsistencies and skipped; so are relationships
/// that cannot be resolved (the table itself is kept).
pub fn resolve_source(
    source: &Source,
    catalog: &SourceCatalog,
    tables: &mut Tables,
    inconsistencies: &mut Vec<InconsistentObject>,
) {
    let source_name: Arc<str> = Arc::from(source.name.as_str());
    let tracked: HashSet<QualifiedTable> = source
        .tables
        .iter()
        .filter(|t| catalog.table(&t.table).is_some())
        .map(|t| t.table.clone())
        .collect();
    for entry in &source.tables {
        let table_def = || serde_json::to_value(&entry.table).unwrap_or_default();
        let Some(cat) = catalog.table(&entry.table) else {
            inconsistencies.push(InconsistentObject::table(
                &source.name,
                &entry.table,
                format!(
                    "no such table/view exists in source: \"{}\"",
                    entry.table.v2_text()
                ),
                table_def(),
            ));
            continue;
        };
        match resolve_table(&source_name, entry, cat, catalog, &tracked, inconsistencies) {
            Ok(info) => {
                let info = Arc::new(info);
                tables
                    .by_name
                    .insert((source_name.clone(), entry.table.clone()), info.clone());
                tables.ordered.push(info);
            }
            Err(e) => inconsistencies.push(InconsistentObject::table(
                &source.name,
                &entry.table,
                e,
                table_def(),
            )),
        }
    }
}

fn relationship_issue(
    kind: &str,
    rel: &impl serde::Serialize,
    name: &str,
    source: &str,
    table: &QualifiedTable,
    reason: String,
) -> InconsistentObject {
    InconsistentObject::table_object(
        kind,
        name,
        source,
        table,
        reason,
        with_comment(serde_json::to_value(rel).unwrap_or_default()),
    )
}

/// V2's relationship definitions always carry `comment`.
fn with_comment(mut def: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = def.as_object_mut() {
        obj.entry("comment").or_insert(serde_json::Value::Null);
    }
    def
}

fn resolve_table(
    source: &Arc<str>,
    entry: &TableEntry,
    cat: &TableCatalog,
    catalog: &SourceCatalog,
    tracked: &HashSet<QualifiedTable>,
    issues: &mut Vec<InconsistentObject>,
) -> Result<TableInfo, String> {
    let config = &entry.configuration;
    let gql_name = match &config.custom_name {
        Some(n) => n.clone(),
        None if entry.table.schema == "public" => entry.table.name.clone(),
        None => format!("{}_{}", entry.table.schema, entry.table.name),
    };
    if !is_valid_name(&gql_name) {
        return Err(format!(
            "cannot use table \"{}\" in the GraphQL schema because its name is not a valid GraphQL identifier; set a custom_name",
            entry.table
        ));
    }

    let enum_values = if entry.is_enum {
        Some(crate::enums::resolve_enum_values(
            &entry.table,
            cat,
            catalog,
        )?)
    } else {
        None
    };

    let mut columns = IndexMap::new();
    for col in cat.columns.values() {
        let gql = config.column_name(&col.name).to_owned();
        if !is_valid_name(&gql) {
            continue;
        }
        let description = comment_or(
            config
                .column_config
                .get(&col.name)
                .and_then(|c| c.comment.as_deref()),
            || col.description.clone(),
        );
        columns.insert(
            col.name.clone(),
            ColumnInfo {
                name: col.name.clone(),
                gql_name: gql,
                ty: col.ty.clone(),
                nullable: col.nullable,
                description,
                mutable: col.is_mutable(),
                enum_ref: None,
            },
        );
    }

    let mut object_relationships = vec![];
    for rel in &entry.object_relationships {
        let resolved = match &rel.using {
            ObjectRelUsing::ForeignKey {
                foreign_key_constraint_on,
            } => match foreign_key_constraint_on {
                ObjectFkOn::Column(c) => {
                    forward_fk(cat, std::slice::from_ref(c), &rel.name).map(|(t, m)| (t, m, true))
                }
                ObjectFkOn::Columns(cs) => {
                    forward_fk(cat, cs, &rel.name).map(|(t, m)| (t, m, true))
                }
                ObjectFkOn::Remote(remote) => {
                    reverse_fk(cat, catalog, source, remote, &rel.name).map(|(t, m)| (t, m, false))
                }
            },
            ObjectRelUsing::Manual {
                manual_configuration,
            } => manual(cat, catalog, tracked, manual_configuration).map(|(t, m)| (t, m, false)),
        };
        let (target, mapping, forward) = match resolved {
            Ok(r) => r,
            Err(e) => {
                issues.push(relationship_issue(
                    "object_relation",
                    rel,
                    &rel.name,
                    source,
                    &entry.table,
                    e,
                ));
                continue;
            }
        };
        if let Some(e) = field_conflict(&columns, &rel.name, std::slice::from_ref(&rel.name)) {
            issues.push(relationship_issue(
                "object_relation",
                rel,
                &rel.name,
                source,
                &entry.table,
                e,
            ));
            continue;
        }
        let nullable = !forward
            || mapping
                .iter()
                .any(|(c, _)| cat.column(c).is_none_or(|c| c.nullable));
        object_relationships.push(Arc::new(RelationshipInfo {
            name: rel.name.clone(),
            kind: RelKind::Object,
            source: source.clone(),
            target,
            mapping,
            nullable,
            description: rel.comment.clone(),
        }));
    }

    let mut array_relationships = vec![];
    for rel in &entry.array_relationships {
        let resolved = match &rel.using {
            ArrayRelUsing::ForeignKey {
                foreign_key_constraint_on,
            } => reverse_fk(cat, catalog, source, foreign_key_constraint_on, &rel.name),
            ArrayRelUsing::Manual {
                manual_configuration,
            } => manual(cat, catalog, tracked, manual_configuration),
        };
        let (target, mapping) = match resolved {
            Ok(r) => r,
            Err(e) => {
                issues.push(relationship_issue(
                    "array_relation",
                    rel,
                    &rel.name,
                    source,
                    &entry.table,
                    e,
                ));
                continue;
            }
        };
        let names = [rel.name.clone(), format!("{}_aggregate", rel.name)];
        if let Some(e) = field_conflict(&columns, &rel.name, &names) {
            issues.push(relationship_issue(
                "array_relation",
                rel,
                &rel.name,
                source,
                &entry.table,
                e,
            ));
            continue;
        }
        array_relationships.push(Arc::new(RelationshipInfo {
            name: rel.name.clone(),
            kind: RelKind::Array,
            source: source.clone(),
            target,
            mapping,
            nullable: true,
            description: rel.comment.clone(),
        }));
    }

    let computed_fields = computed::resolve(source, entry, cat, catalog, tracked, issues)
        .into_iter()
        .map(Arc::new)
        .collect();

    let crf = config.custom_root_fields.clone().unwrap_or_default();
    let pick = |f: &Option<hge_metadata::CustomRootField>, default: String| {
        f.as_ref()
            .and_then(|f| f.name())
            .map(str::to_owned)
            .unwrap_or(default)
    };
    let comment = |f: &Option<hge_metadata::CustomRootField>| {
        f.as_ref().and_then(|f| f.comment()).map(str::to_owned)
    };
    let root_fields = RootFieldNames {
        select: pick(&crf.select, gql_name.clone()),
        select_by_pk: pick(&crf.select_by_pk, format!("{gql_name}_by_pk")),
        select_aggregate: pick(&crf.select_aggregate, format!("{gql_name}_aggregate")),
        select_comment: comment(&crf.select),
        select_by_pk_comment: comment(&crf.select_by_pk),
        select_aggregate_comment: comment(&crf.select_aggregate),
    };

    Ok(TableInfo {
        source: source.clone(),
        name: entry.table.clone(),
        gql_name,
        // The metadata comment, else the database comment, else V2's default.
        description: comment_or(config.comment.as_deref(), || {
            Some(cat.description.clone().unwrap_or_else(|| {
                format!(
                    "columns and relationships of \"{}\"",
                    display_name(&entry.table)
                )
            }))
        }),
        columns,
        primary_key: cat.primary_key.clone(),
        constraints: cat.constraints.clone(),
        mutability: cat.mutability,
        object_relationships,
        array_relationships,
        computed_fields,
        root_fields,
        is_enum: entry.is_enum,
        logical_model: false,
        enum_values,
        entry: entry.clone(),
        permissions: HashMap::new(),
    })
}

/// V2's dependency pass for relationships: one whose target table is not among the resolved
/// tables (untracked, or itself inconsistent) is reported and dropped.
pub fn drop_dangling_relationships(tables: &mut Tables, issues: &mut Vec<InconsistentObject>) {
    let present: HashSet<(Arc<str>, QualifiedTable)> = tables.by_name.keys().cloned().collect();
    let resolved = |r: &RelationshipInfo| present.contains(&(r.source.clone(), r.target.clone()));
    for slot in &mut tables.ordered {
        let table = slot.as_ref();
        let dangling = |rels: &[Arc<RelationshipInfo>]| rels.iter().any(|r| !resolved(r));
        if !dangling(&table.object_relationships) && !dangling(&table.array_relationships) {
            continue;
        }
        let mut info = table.clone();
        let entry = &table.entry;
        let mut report = |r: &RelationshipInfo| {
            let reason = format!("table \"{}\" is not tracked", r.target.v2_text());
            let issue = match r.kind {
                RelKind::Object => entry
                    .object_relationships
                    .iter()
                    .find(|m| m.name == r.name)
                    .map(|m| {
                        relationship_issue(
                            "object_relation",
                            m,
                            &r.name,
                            &info.source,
                            &info.name,
                            reason,
                        )
                    }),
                RelKind::Array => entry
                    .array_relationships
                    .iter()
                    .find(|m| m.name == r.name)
                    .map(|m| {
                        relationship_issue(
                            "array_relation",
                            m,
                            &r.name,
                            &info.source,
                            &info.name,
                            reason,
                        )
                    }),
            };
            issues.extend(issue);
        };
        for r in table
            .object_relationships
            .iter()
            .chain(&table.array_relationships)
        {
            if !resolved(r) {
                report(r);
            }
        }
        info.object_relationships.retain(|r| resolved(r));
        info.array_relationships.retain(|r| resolved(r));
        let info = Arc::new(info);
        tables
            .by_name
            .insert((info.source.clone(), info.name.clone()), info.clone());
        *slot = info;
    }
}

/// V2's checks of a relationship against the table's columns: one of its GraphQL names
/// (`gql_names`) taken by a column's custom name, else its `name` equal to a column's name.
fn field_conflict(
    columns: &IndexMap<String, ColumnInfo>,
    name: &str,
    gql_names: &[String],
) -> Option<String> {
    for n in gql_names {
        if let Some(col) = columns.values().find(|c| &c.gql_name == n && &c.name != n) {
            return Some(format!(
                "field definition conflicts with custom field name for postgres column \"{}\"",
                col.name
            ));
        }
    }
    columns
        .contains_key(name)
        .then(|| "field definition conflicts with postgres column".to_owned())
}

/// Object relationship through a foreign key on this table.
fn forward_fk(
    cat: &TableCatalog,
    columns: &[String],
    rel: &str,
) -> Result<(QualifiedTable, Vec<(String, String)>), String> {
    let fk = cat
        .foreign_keys
        .iter()
        .find(|fk| {
            fk.columns.len() == columns.len()
                && columns
                    .iter()
                    .all(|c| fk.columns.iter().any(|(l, _)| l == c))
        })
        .ok_or_else(|| no_constraint(cat, rel))?;
    Ok((fk.ref_table.clone(), fk.columns.clone()))
}

/// Relationship through a foreign key on the remote table pointing at this table.
fn reverse_fk(
    cat: &TableCatalog,
    catalog: &SourceCatalog,
    source: &str,
    remote: &hge_metadata::RemoteFk,
    rel: &str,
) -> Result<(QualifiedTable, Vec<(String, String)>), String> {
    let columns = remote.columns();
    let remote_cat = catalog.table(&remote.table).ok_or_else(|| {
        format!(
            "in table \"{}\": in relationship \"{rel}\": table \"{}\" does not exist in source: {}",
            cat.name.v2_text(),
            remote.table.v2_text(),
            source
        )
    })?;
    let fk = remote_cat
        .foreign_keys
        .iter()
        .find(|fk| {
            fk.ref_table == cat.name
                && fk.columns.len() == columns.len()
                && columns
                    .iter()
                    .all(|c| fk.columns.iter().any(|(l, _)| l == c))
        })
        .ok_or_else(|| no_constraint(cat, rel))?;
    let mapping = fk
        .columns
        .iter()
        .map(|(remote_col, local_col)| (local_col.clone(), remote_col.clone()))
        .collect();
    Ok((remote.table.clone(), mapping))
}

fn no_constraint(cat: &TableCatalog, rel: &str) -> String {
    format!(
        "in table \"{}\": in relationship \"{rel}\": no foreign constraint exists on the given column(s)",
        cat.name.v2_text()
    )
}

fn manual(
    cat: &TableCatalog,
    catalog: &SourceCatalog,
    tracked: &HashSet<QualifiedTable>,
    cfg: &ManualRelConfig,
) -> Result<(QualifiedTable, Vec<(String, String)>), String> {
    let remote = tracked
        .contains(&cfg.remote_table)
        .then(|| catalog.table(&cfg.remote_table))
        .flatten()
        .ok_or_else(|| format!("table \"{}\" is not tracked", cfg.remote_table.v2_text()))?;
    for (local, remote_col) in &cfg.column_mapping {
        if cat.column(local).is_none() {
            return Err(format!(
                "table \"{}\" has no field named \"{local}\"",
                cat.name.v2_text()
            ));
        }
        if remote.column(remote_col).is_none() {
            return Err(format!(
                "table \"{}\" has no field named \"{remote_col}\"",
                remote.name.v2_text()
            ));
        }
    }
    Ok((
        cfg.remote_table.clone(),
        cfg.column_mapping
            .iter()
            .map(|(l, r)| (l.clone(), r.clone()))
            .collect(),
    ))
}

/// Names the schema itself defines; a table may not generate any of them.
const RESERVED_TYPE_NAMES: &[&str] = &[
    "Boolean",
    "Float",
    "Int",
    "String",
    "ID",
    "order_by",
    "query_root",
    "mutation_root",
    "subscription_root",
    "cursor_ordering",
];

/// Suffixes of the per-table types V2 generates (the bare table type has no suffix).
const TYPE_SUFFIXES: &[&str] = &[
    "",
    "_bool_exp",
    "_order_by",
    "_select_column",
    "_aggregate",
    "_aggregate_fields",
    "_aggregate_order_by",
    "_aggregate_bool_exp",
    "_avg_fields",
    "_avg_order_by",
    "_max_fields",
    "_max_order_by",
    "_min_fields",
    "_min_order_by",
    "_stddev_fields",
    "_stddev_order_by",
    "_stddev_pop_fields",
    "_stddev_pop_order_by",
    "_stddev_samp_fields",
    "_stddev_samp_order_by",
    "_sum_fields",
    "_sum_order_by",
    "_var_pop_fields",
    "_var_pop_order_by",
    "_var_samp_fields",
    "_var_samp_order_by",
    "_variance_fields",
    "_variance_order_by",
    "_mutation_response",
    "_insert_input",
    "_set_input",
    "_pk_columns_input",
    "_on_conflict",
    "_constraint",
    "_update_column",
    "_obj_rel_insert_input",
    "_arr_rel_insert_input",
    "_inc_input",
    "_append_input",
    "_prepend_input",
    "_delete_key_input",
    "_delete_elem_input",
    "_delete_at_path_input",
    "_updates",
    "_stream_cursor_input",
];

/// Drop tables whose generated GraphQL names clash with an earlier table's (or with the
/// schema's own names), or whose fields clash among themselves, as V2 reports such metadata
/// inconsistent. Without this a later table would silently reuse an earlier table's types,
/// exposing columns through the wrong permissions.
pub fn drop_conflicting(tables: &mut Tables, inconsistencies: &mut Vec<InconsistentObject>) {
    let mut types: HashMap<String, String> = RESERVED_TYPE_NAMES
        .iter()
        .map(|n| ((*n).to_owned(), "the schema".to_owned()))
        .collect();
    // Scalars of tracked columns and their generated input types belong to the schema too.
    for table in &tables.ordered {
        for col in table.columns.values() {
            let scalar = crate::scalars::scalar_name(&col.ty.element());
            for suffix in ["", "_comparison_exp", "_array_comparison_exp", "_cast_exp"] {
                types.insert(format!("{scalar}{suffix}"), format!("scalar \"{scalar}\""));
            }
        }
    }
    let mut roots: HashMap<String, String> = HashMap::new();
    let mut kept = Vec::with_capacity(tables.ordered.len());
    for table in std::mem::take(&mut tables.ordered) {
        let owner = format!("table \"{}\" in source \"{}\"", table.name, table.source);
        let mut type_names: Vec<String> = TYPE_SUFFIXES
            .iter()
            .map(|s| format!("{}{s}", table.gql_name))
            .collect();
        // Aggregation-predicate types, one set per predicate function.
        for function in hge_ir::aggregate::PredicateFunction::ALL {
            let pred = table.aggregate_bool_exp_function_name(function);
            type_names.push(format!(
                "{}_select_column_{pred}_arguments_columns",
                table.gql_name
            ));
            type_names.push(format!("{pred}_arguments"));
            type_names.push(pred);
        }
        // An enum table also generates its enum type.
        if table.enum_values.is_some() {
            for suffix in ["_enum", "_enum_comparison_exp"] {
                type_names.push(format!("{}{suffix}", table.gql_name));
            }
        }
        let rf = &table.root_fields;
        let root_names = [&rf.select, &rf.select_by_pk, &rf.select_aggregate];
        let mut problem = None;
        if let Some(n) = type_names.iter().find(|n| types.contains_key(*n)) {
            problem = Some(format!("type name \"{n}\" is already used by {}", types[n]));
        } else if let Some(n) = root_names.iter().find(|n| roots.contains_key(**n)) {
            problem = Some(format!(
                "root field \"{n}\" is already used by {}",
                roots[*n]
            ));
        } else if let Some(n) = root_names.iter().find(|n| !is_valid_name(n)) {
            problem = Some(format!("root field \"{n}\" is not a valid GraphQL name"));
        } else if root_names[0] == root_names[1]
            || root_names[0] == root_names[2]
            || root_names[1] == root_names[2]
        {
            problem = Some("root field names must be distinct".to_owned());
        } else {
            let mut fields = HashSet::new();
            // Array relationships also generate `<rel>_aggregate` fields.
            let aggregate_names: Vec<String> = table
                .array_relationships
                .iter()
                .map(|r| format!("{}_aggregate", r.name))
                .collect();
            let names = table
                .columns
                .values()
                .map(|c| &c.gql_name)
                .chain(table.object_relationships.iter().map(|r| &r.name))
                .chain(table.array_relationships.iter().map(|r| &r.name))
                .chain(aggregate_names.iter())
                .chain(table.computed_fields.iter().map(|c| &c.name));
            for n in names {
                if !is_valid_name(n) {
                    problem = Some(format!("field \"{n}\" is not a valid GraphQL name"));
                    break;
                }
                if !fields.insert(n.as_str()) {
                    problem = Some(format!("field name \"{n}\" is used more than once"));
                    break;
                }
            }
        }
        if let Some(p) = problem {
            inconsistencies.push(InconsistentObject::table(
                &table.source,
                &table.name,
                p,
                serde_json::to_value(&table.name).unwrap_or_default(),
            ));
            tables
                .by_name
                .remove(&(table.source.clone(), table.name.clone()));
            continue;
        }
        for n in type_names {
            types.insert(n, owner.clone());
        }
        for n in root_names {
            roots.insert(n.clone(), owner.clone());
        }
        kept.push(table);
    }
    tables.ordered = kept;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use hge_core::{
        ColumnInfo, ColumnType, InconsistentObject, QualifiedTable, SourceCatalog, TableCatalog,
        TableKind, TableMutability,
    };
    use indexmap::IndexMap;
    use serde_json::json;

    use super::field_conflict;

    fn table(name: &str) -> (QualifiedTable, TableCatalog) {
        let qt = QualifiedTable::new("public", name);
        let id = ColumnInfo {
            name: "id".to_owned(),
            ty: ColumnType::builtin("int4"),
            nullable: false,
            has_default: false,
            is_identity: false,
            is_identity_always: false,
            is_generated: false,
            description: None,
        };
        let catalog = TableCatalog {
            name: qt.clone(),
            kind: TableKind::Table,
            description: None,
            columns: IndexMap::from([("id".to_owned(), id)]),
            primary_key: Some(vec!["id".into()]),
            unique_constraints: vec![],
            constraints: vec![],
            mutability: TableMutability::ALL,
            foreign_keys: vec![],
        };
        (qt, catalog)
    }

    /// Track `article` plus a table whose name is one of `article`'s generated type names;
    /// the inconsistencies reported.
    fn clash(other: &str) -> Vec<InconsistentObject> {
        let tables = ["article", other].map(|n| json!({"table": {"schema": "public", "name": n}}));
        let metadata = hge_metadata::from_json(json!({
            "version": 3,
            "sources": [{
                "name": "default",
                "kind": "postgres",
                "configuration": {"connection_info": {"database_url": "postgres://x"}},
                "tables": tables
            }]
        }))
        .unwrap();
        let catalog = SourceCatalog {
            tables: HashMap::from([table("article"), table(other)]),
            enums: HashMap::new(),
            functions: HashMap::new(),
            ..Default::default()
        };
        let catalogs = HashMap::from([("default".to_owned(), catalog)]);
        crate::resolve_tables(&metadata, &catalogs).1
    }

    #[test]
    fn aggregation_predicate_type_names_clash() {
        for other in [
            "article_aggregate_fields",
            "article_aggregate_bool_exp_count",
            "article_aggregate_bool_exp_corr_arguments",
            "article_select_column_article_aggregate_bool_exp_sum_arguments_columns",
        ] {
            let errs = clash(other);
            assert_eq!(errs.len(), 1, "{other}: {errs:?}");
        }
        assert!(clash("article_comment").is_empty());
    }

    #[test]
    fn relationship_field_conflicts() {
        let col = |name: &str, gql: &str| {
            (
                name.to_owned(),
                super::ColumnInfo {
                    name: name.to_owned(),
                    gql_name: gql.to_owned(),
                    ty: ColumnType::builtin("text"),
                    nullable: true,
                    description: None,
                    mutable: true,
                    enum_ref: None,
                },
            )
        };
        let columns = IndexMap::from([col("id", "id"), col("name", "articles")]);
        let rel = |n: &str| [n.to_owned(), format!("{n}_aggregate")];
        assert_eq!(
            field_conflict(&columns, "articles", &rel("articles")).as_deref(),
            Some("field definition conflicts with custom field name for postgres column \"name\"")
        );
        assert_eq!(
            field_conflict(&columns, "id", &rel("id")).as_deref(),
            Some("field definition conflicts with postgres column")
        );
        assert_eq!(field_conflict(&columns, "tags", &rel("tags")), None);
    }
}
