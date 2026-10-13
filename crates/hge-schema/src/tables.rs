//! Resolved information about tracked tables: metadata joined with the database catalog,
//! independent of any role.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hge_core::{
    ColumnType, QualifiedTable, RoleName, SourceCatalog, TableCatalog, TableMutability,
    UniqueConstraint,
};
use hge_ir::TablePermissions;
use hge_metadata::{
    ArrayRelUsing, ManualRelConfig, ObjectFkOn, ObjectRelUsing, Source, TableEntry,
};
use indexmap::IndexMap;

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
    pub root_fields: RootFieldNames,
    pub is_enum: bool,
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

    pub fn type_name(&self) -> &str {
        &self.gql_name
    }

    pub fn bool_exp_name(&self) -> String {
        format!("{}_bool_exp", self.gql_name)
    }

    pub fn order_by_name(&self) -> String {
        format!("{}_order_by", self.gql_name)
    }

    pub fn select_column_name(&self) -> String {
        format!("{}_select_column", self.gql_name)
    }

    /// The table name as V2 prints it in descriptions (schema omitted when `public`).
    pub fn display_name(&self) -> String {
        if self.name.schema == "public" {
            self.name.name.clone()
        } else {
            format!("{}.{}", self.name.schema, self.name.name)
        }
    }
}

/// All tracked tables of all sources.
#[derive(Debug, Clone, Default)]
pub struct Tables {
    pub by_name: HashMap<(Arc<str>, QualifiedTable), Arc<TableInfo>>,
    /// Tables in metadata order.
    pub ordered: Vec<Arc<TableInfo>>,
}

impl Tables {
    pub fn get(&self, source: &str, name: &QualifiedTable) -> Option<&Arc<TableInfo>> {
        self.by_name.get(&(Arc::from(source), name.clone()))
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
/// valid GraphQL names, are reported as inconsistencies and skipped.
pub fn resolve_source(
    source: &Source,
    catalog: &SourceCatalog,
    tables: &mut Tables,
    inconsistencies: &mut Vec<String>,
) {
    let source_name: Arc<str> = Arc::from(source.name.as_str());
    for entry in &source.tables {
        let Some(cat) = catalog.table(&entry.table) else {
            inconsistencies.push(format!(
                "table \"{}\" does not exist in source \"{}\"",
                entry.table, source.name
            ));
            continue;
        };
        match resolve_table(&source_name, entry, cat, catalog) {
            Ok(info) => {
                let info = Arc::new(info);
                tables
                    .by_name
                    .insert((source_name.clone(), entry.table.clone()), info.clone());
                tables.ordered.push(info);
            }
            Err(e) => inconsistencies.push(e),
        }
    }
}

fn resolve_table(
    source: &Arc<str>,
    entry: &TableEntry,
    cat: &TableCatalog,
    catalog: &SourceCatalog,
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

    let mut columns = IndexMap::new();
    for col in cat.columns.values() {
        let gql = config.column_name(&col.name).to_owned();
        if !is_valid_name(&gql) {
            continue;
        }
        let description = config
            .column_config
            .get(&col.name)
            .and_then(|c| c.comment.clone())
            .or_else(|| col.description.clone());
        columns.insert(
            col.name.clone(),
            ColumnInfo {
                name: col.name.clone(),
                gql_name: gql,
                ty: col.ty.clone(),
                nullable: col.nullable,
                description,
                mutable: col.is_mutable(),
            },
        );
    }

    let mut object_relationships = vec![];
    for rel in &entry.object_relationships {
        let (target, mapping, forward) = match &rel.using {
            ObjectRelUsing::ForeignKey {
                foreign_key_constraint_on,
            } => match foreign_key_constraint_on {
                ObjectFkOn::Column(c) => {
                    let (t, m) = forward_fk(cat, std::slice::from_ref(c), &rel.name)?;
                    (t, m, true)
                }
                ObjectFkOn::Columns(cs) => {
                    let (t, m) = forward_fk(cat, cs, &rel.name)?;
                    (t, m, true)
                }
                ObjectFkOn::Remote(remote) => {
                    let (t, m) = reverse_fk(cat, catalog, remote, &rel.name)?;
                    (t, m, false)
                }
            },
            ObjectRelUsing::Manual {
                manual_configuration,
            } => {
                let (t, m) = manual(manual_configuration);
                (t, m, false)
            }
        };
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
        let (target, mapping) = match &rel.using {
            ArrayRelUsing::ForeignKey {
                foreign_key_constraint_on,
            } => reverse_fk(cat, catalog, foreign_key_constraint_on, &rel.name)?,
            ArrayRelUsing::Manual {
                manual_configuration,
            } => manual(manual_configuration),
        };
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
        description: config.comment.clone().or_else(|| cat.description.clone()),
        columns,
        primary_key: cat.primary_key.clone(),
        constraints: cat.constraints.clone(),
        mutability: cat.mutability,
        object_relationships,
        array_relationships,
        root_fields,
        is_enum: entry.is_enum,
        entry: entry.clone(),
        permissions: HashMap::new(),
    })
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
                && columns.iter().all(|c| fk.columns.iter().any(|(l, _)| l == c))
        })
        .ok_or_else(|| {
            format!(
                "relationship \"{rel}\" of table \"{}\": no foreign key constraint exists on the columns {:?}",
                cat.name, columns
            )
        })?;
    Ok((fk.ref_table.clone(), fk.columns.clone()))
}

/// Relationship through a foreign key on the remote table pointing at this table.
fn reverse_fk(
    cat: &TableCatalog,
    catalog: &SourceCatalog,
    remote: &hge_metadata::RemoteFk,
    rel: &str,
) -> Result<(QualifiedTable, Vec<(String, String)>), String> {
    let columns = remote.columns();
    let remote_cat = catalog.table(&remote.table).ok_or_else(|| {
        format!(
            "relationship \"{rel}\" of table \"{}\": table \"{}\" does not exist",
            cat.name, remote.table
        )
    })?;
    let fk = remote_cat
        .foreign_keys
        .iter()
        .find(|fk| {
            fk.ref_table == cat.name
                && fk.columns.len() == columns.len()
                && columns.iter().all(|c| fk.columns.iter().any(|(l, _)| l == c))
        })
        .ok_or_else(|| {
            format!(
                "relationship \"{rel}\" of table \"{}\": no foreign key constraint exists on {:?} of table \"{}\" referencing it",
                cat.name, columns, remote.table
            )
        })?;
    let mapping = fk
        .columns
        .iter()
        .map(|(remote_col, local_col)| (local_col.clone(), remote_col.clone()))
        .collect();
    Ok((remote.table.clone(), mapping))
}

fn manual(cfg: &ManualRelConfig) -> (QualifiedTable, Vec<(String, String)>) {
    (
        cfg.remote_table.clone(),
        cfg.column_mapping
            .iter()
            .map(|(l, r)| (l.clone(), r.clone()))
            .collect(),
    )
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
pub fn drop_conflicting(tables: &mut Tables, inconsistencies: &mut Vec<String>) {
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
        let type_names: Vec<String> = TYPE_SUFFIXES
            .iter()
            .map(|s| format!("{}{s}", table.gql_name))
            .collect();
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
            let names = table
                .columns
                .values()
                .map(|c| &c.gql_name)
                .chain(table.object_relationships.iter().map(|r| &r.name))
                .chain(table.array_relationships.iter().map(|r| &r.name));
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
            inconsistencies.push(format!("{owner}: {p}"));
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
