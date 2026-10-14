//! Typed Hasura V2 metadata (`version: 3`), as produced by `export_metadata` and stored in
//! `hdb_catalog.hdb_metadata`.
//!
//! Conventions:
//! * Every struct keeps unknown keys in `extra` so metadata round-trips through
//!   `export_metadata` without loss, even for fields we do not model yet.
//! * Defaults are skipped on serialisation, matching V2's export output.
//! * Parts owned by later workstreams (actions, remote schemas, event triggers, …) are
//!   `serde_json::Value` until that workstream types them.

use hge_core::QualifiedTable;

use crate::collections::{AllowlistEntry, ApiLimits, QueryCollection, RestEndpoint};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Extra = Map<String, Value>;

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_version() -> u32 {
    3
}

/// The whole metadata document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub sources: Vec<Source>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_schemas: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_types: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_collections: Vec<QueryCollection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowlist: Vec<AllowlistEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cron_triggers: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherited_roles: Vec<InheritedRole>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rest_endpoints: Vec<RestEndpoint>,
    #[serde(default, skip_serializing_if = "crate::collections::api_limits_absent")]
    pub api_limits: Option<ApiLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics_config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_configs: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opentelemetry: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphql_schema_introspection: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Metadata {
    /// V2's `emptyMetadata`: version 3, no sources, nothing else.
    pub fn empty() -> Self {
        Metadata {
            version: 3,
            ..Metadata::default()
        }
    }

    pub fn source_mut(&mut self, name: &str) -> Option<&mut Source> {
        self.sources.iter_mut().find(|s| s.name == name)
    }

    pub fn source(&self, name: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InheritedRole {
    pub role_name: String,
    pub role_set: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A database source (`databases.yaml` entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub tables: Vec<TableEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<Value>,
    pub configuration: SourceConfiguration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customization: Option<SourceCustomization>,
    #[serde(flatten)]
    pub extra: Extra,
}

fn default_kind() -> String {
    "postgres".to_owned()
}

/// Source configuration. For Postgres-like sources `connection_info` is typed; other
/// backends keep their configuration in `extra`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_info: Option<PgConnectionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_replicas: Option<Vec<PgConnectionInfo>>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl SourceConfiguration {
    /// The connection defaults V2 fills in when it parses a Postgres source configuration.
    pub fn fill_pg_defaults(&mut self) {
        let fill = |info: &mut PgConnectionInfo| {
            if info.isolation_level.is_none() {
                info.isolation_level = Some("read-committed".to_owned());
            }
            if info.use_prepared_statements.is_none() {
                info.use_prepared_statements = Some(false);
            }
            if let Some(pool) = &mut info.pool_settings
                && pool.connection_lifetime.is_none()
            {
                pool.connection_lifetime = Some(600);
            }
        };
        if let Some(info) = &mut self.connection_info {
            fill(info);
        }
        for info in self.read_replicas.iter_mut().flatten() {
            fill(info);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PgConnectionInfo {
    pub database_url: UrlConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_settings: Option<PoolSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_prepared_statements: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation_level: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A connection string: literal, `{from_env: VAR}`, or `{connection_parameters: {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UrlConfig {
    Literal(String),
    FromEnv {
        from_env: String,
    },
    Parameters {
        connection_parameters: ConnectionParameters,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionParameters {
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    pub database: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PoolSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_max_connections: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_lifetime: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceCustomization {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_fields: Option<RootFieldsCustomization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_names: Option<TypeNamesCustomization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub naming_convention: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RootFieldsCustomization {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TypeNamesCustomization {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

/// A tracked table with everything attached to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableEntry {
    pub table: QualifiedTable,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_enum: bool,
    #[serde(default, skip_serializing_if = "TableConfig::is_empty")]
    pub configuration: TableConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub object_relationships: Vec<ObjectRelationship>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub array_relationships: Vec<ArrayRelationship>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub computed_fields: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_relationships: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub insert_permissions: Vec<Permission<InsertPermission>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub select_permissions: Vec<Permission<SelectPermission>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub update_permissions: Vec<Permission<UpdatePermission>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delete_permissions: Vec<Permission<DeletePermission>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_triggers: Vec<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl TableEntry {
    pub fn new(table: QualifiedTable) -> Self {
        TableEntry {
            table,
            is_enum: false,
            configuration: TableConfig::default(),
            object_relationships: vec![],
            array_relationships: vec![],
            computed_fields: vec![],
            remote_relationships: vec![],
            insert_permissions: vec![],
            select_permissions: vec![],
            update_permissions: vec![],
            delete_permissions: vec![],
            event_triggers: vec![],
            extra: Extra::new(),
        }
    }

    pub fn select_permission(&self, role: &str) -> Option<&SelectPermission> {
        self.select_permissions
            .iter()
            .find(|p| p.role == role)
            .map(|p| &p.permission)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TableConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_root_fields: Option<CustomRootFields>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub column_config: IndexMap<String, ColumnConfig>,
    /// Deprecated V2 form of `column_config.*.custom_name`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub custom_column_names: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl TableConfig {
    pub fn is_empty(&self) -> bool {
        *self == TableConfig::default()
    }

    /// The GraphQL name of a column, honouring `column_config` and the deprecated
    /// `custom_column_names`.
    pub fn column_name<'a>(&'a self, column: &'a str) -> &'a str {
        if let Some(name) = self
            .column_config
            .get(column)
            .and_then(|c| c.custom_name.as_deref())
        {
            return name;
        }
        self.custom_column_names
            .get(column)
            .map(String::as_str)
            .unwrap_or(column)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ColumnConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `custom_root_fields`: each entry is either a bare name or `{name, comment}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomRootFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select_by_pk: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select_aggregate: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select_stream: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_one: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_by_pk: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_many: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<CustomRootField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_by_pk: Option<CustomRootField>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomRootField {
    Name(String),
    Object {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
    },
}

impl CustomRootField {
    pub fn name(&self) -> Option<&str> {
        match self {
            CustomRootField::Name(n) => Some(n),
            CustomRootField::Object { name, .. } => name.as_deref(),
        }
    }

    pub fn comment(&self) -> Option<&str> {
        match self {
            CustomRootField::Name(_) => None,
            CustomRootField::Object { comment, .. } => comment.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Relationships

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectRelationship {
    pub name: String,
    pub using: ObjectRelUsing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ObjectRelUsing {
    ForeignKey {
        foreign_key_constraint_on: ObjectFkOn,
    },
    Manual {
        manual_configuration: ManualRelConfig,
    },
}

/// `foreign_key_constraint_on` for object relationships: a column, a list of columns
/// (both on the current table), or `{table, column(s)}` for a reverse (remote-side) fk.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ObjectFkOn {
    Column(String),
    Columns(Vec<String>),
    Remote(RemoteFk),
}

/// A one-element column list is kept as a plain column, like V2.
impl Serialize for ObjectFkOn {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ObjectFkOn::Column(c) => c.serialize(s),
            ObjectFkOn::Columns(cs) if cs.len() == 1 => cs[0].serialize(s),
            ObjectFkOn::Columns(cs) => cs.serialize(s),
            ObjectFkOn::Remote(r) => r.serialize(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "RawRemoteFk")]
pub struct RemoteFk {
    pub table: QualifiedTable,
    pub column: Option<String>,
    pub columns: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawRemoteFk {
    table: QualifiedTable,
    #[serde(default)]
    column: Option<String>,
    #[serde(default)]
    columns: Option<Vec<String>>,
}

/// V2 requires exactly one of `column` and `columns`.
impl TryFrom<RawRemoteFk> for RemoteFk {
    type Error = &'static str;

    fn try_from(raw: RawRemoteFk) -> Result<Self, Self::Error> {
        if raw.column.is_some() == raw.columns.is_some() {
            return Err(REMOTE_FK_COLUMNS_ERROR);
        }
        Ok(RemoteFk {
            table: raw.table,
            column: raw.column,
            columns: raw.columns,
        })
    }
}

/// V2's parse error for a `{table, column(s)}` foreign key without exactly one of them.
pub const REMOTE_FK_COLUMNS_ERROR: &str = "expected exactly one of 'column' or 'columns'";

/// V2 keeps a single column as `column` and several as `columns`.
impl Serialize for RemoteFk {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let columns = self.columns();
        let mut map = s.serialize_map(Some(2))?;
        map.serialize_entry("table", &self.table)?;
        match columns.as_slice() {
            [one] => map.serialize_entry("column", one)?,
            _ => map.serialize_entry("columns", &columns)?,
        }
        map.end()
    }
}

impl RemoteFk {
    pub fn columns(&self) -> Vec<String> {
        match (&self.column, &self.columns) {
            (_, Some(cols)) => cols.clone(),
            (Some(col), None) => vec![col.clone()],
            (None, None) => vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArrayRelationship {
    pub name: String,
    pub using: ArrayRelUsing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArrayRelUsing {
    ForeignKey {
        foreign_key_constraint_on: RemoteFk,
    },
    Manual {
        manual_configuration: ManualRelConfig,
    },
}

/// A manual relationship: onto a table (`remote_table`) or a native query
/// (`remote_native_query`); V2 reads `remote_table` when both are given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManualRelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_table: Option<QualifiedTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_native_query: Option<String>,
    pub column_mapping: IndexMap<String, String>,
    /// Always written (`null` when unset), like V2.
    #[serde(default)]
    pub insertion_order: Option<String>,
}

// ---------------------------------------------------------------------------------------
// Permissions

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Permission<P> {
    pub role: String,
    pub permission: P,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// `columns: "*"` or an explicit list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Columns {
    Star(StarToken),
    List(Vec<String>),
}

impl Default for Columns {
    fn default() -> Self {
        Columns::List(vec![])
    }
}

impl Columns {
    pub fn contains(&self, column: &str) -> bool {
        match self {
            Columns::Star(_) => true,
            Columns::List(cols) => cols.iter().any(|c| c == column),
        }
    }
}

/// The literal string `"*"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarToken;

impl Serialize for StarToken {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("*")
    }
}

impl<'de> Deserialize<'de> for StarToken {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "*" {
            Ok(StarToken)
        } else {
            Err(serde::de::Error::custom("expected \"*\""))
        }
    }
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn is_empty_object(v: &Value) -> bool {
    matches!(v, Value::Object(m) if m.is_empty())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectPermission {
    #[serde(default)]
    pub columns: Columns,
    #[serde(default = "empty_object")]
    pub filter: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_aggregations: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub computed_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_root_fields: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_root_fields: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InsertPermission {
    #[serde(default = "empty_object")]
    pub check: Value,
    /// Absent means every insertable column (unlike select and update permissions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Columns>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub set: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub backend_only: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdatePermission {
    #[serde(default)]
    pub columns: Columns,
    #[serde(default = "empty_object")]
    pub filter: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Value>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub set: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub backend_only: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeletePermission {
    #[serde(default = "empty_object", skip_serializing_if = "is_empty_object")]
    pub filter: Value,
    #[serde(default, skip_serializing_if = "is_false")]
    pub backend_only: bool,
    #[serde(flatten)]
    pub extra: Extra,
}
