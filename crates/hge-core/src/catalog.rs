//! Backend-agnostic description of database objects, produced by a backend's introspection
//! and consumed by the schema builder.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::QualifiedTable;

/// Everything the engine knows about one source's database.
#[derive(Debug, Clone, Default)]
pub struct SourceCatalog {
    pub tables: HashMap<QualifiedTable, TableCatalog>,
    /// Values of native enum types, keyed by `(schema, type name)`.
    pub enums: HashMap<(String, String), Vec<String>>,
}

impl SourceCatalog {
    pub fn table(&self, name: &QualifiedTable) -> Option<&TableCatalog> {
        self.tables.get(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    PartitionedTable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableCatalog {
    pub name: QualifiedTable,
    pub kind: TableKind,
    pub description: Option<String>,
    /// Columns in table order.
    pub columns: IndexMap<String, ColumnInfo>,
    pub primary_key: Option<Vec<String>>,
    pub unique_constraints: Vec<Vec<String>>,
    pub foreign_keys: Vec<ForeignKey>,
}

impl TableCatalog {
    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.get(name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub ty: ColumnType,
    pub nullable: bool,
    pub has_default: bool,
    pub is_identity: bool,
    pub is_generated: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeKind {
    Base,
    Enum,
    Composite,
    Domain,
    Range,
    Pseudo,
    Other,
}

/// The type of a column. For arrays, `name` is the *element* type and `dimensions > 0`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColumnType {
    pub schema: String,
    pub name: String,
    pub kind: TypeKind,
    pub dimensions: u32,
}

impl ColumnType {
    /// A built-in (`pg_catalog`) scalar type such as `int4`.
    pub fn builtin(name: &str) -> Self {
        ColumnType {
            schema: "pg_catalog".to_owned(),
            name: name.to_owned(),
            kind: TypeKind::Base,
            dimensions: 0,
        }
    }

    pub fn is_array(&self) -> bool {
        self.dimensions > 0
    }

    /// The element type of an array type (identity for scalars).
    pub fn element(&self) -> ColumnType {
        ColumnType {
            dimensions: 0,
            ..self.clone()
        }
    }

    /// The one-dimensional array type of this type.
    pub fn array_of(&self) -> ColumnType {
        ColumnType {
            dimensions: self.dimensions + 1,
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKey {
    pub constraint: String,
    pub ref_table: QualifiedTable,
    /// `(local column, referenced column)` pairs in constraint order.
    pub columns: Vec<(String, String)>,
}
