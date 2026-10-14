//! Backend-agnostic description of database objects, produced by a backend's introspection
//! and consumed by the schema builder.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{QualifiedFunction, QualifiedTable};

/// Everything the engine knows about one source's database.
#[derive(Debug, Clone, Default)]
pub struct SourceCatalog {
    pub tables: HashMap<QualifiedTable, TableCatalog>,
    /// Values of native enum types, keyed by `(schema, type name)`.
    pub enums: HashMap<(String, String), Vec<String>>,
    /// Functions referenced by metadata (tracked functions and computed fields), with every
    /// overload of each name.
    pub functions: HashMap<QualifiedFunction, Vec<FunctionCatalog>>,
    /// Scalar types named by logical model fields and native query arguments, keyed by the
    /// name as written in metadata (`int4`, `integer`, `myschema.mood`).
    pub types: HashMap<String, NamedType>,
    /// The outcome of preparing each native query's validation statement: the result
    /// columns, or V2's `error` object.
    pub native_queries: HashMap<NativeQueryCheck, Result<Vec<ResultColumn>, serde_json::Value>>,
    /// Rows of the tables tracked with `is_enum`, or why they could not be read. Unlike the
    /// rest of the catalog they are not re-read on every introspection: V2 keeps them until
    /// the metadata is reloaded or the table first becomes an enum table.
    pub enum_tables: HashMap<QualifiedTable, Result<Vec<EnumTableRow>, String>>,
}

/// One row of an enum table: the primary key value and the comment column, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumTableRow {
    pub value: String,
    pub comment: Option<String>,
}

impl SourceCatalog {
    pub fn table(&self, name: &QualifiedTable) -> Option<&TableCatalog> {
        self.tables.get(name)
    }
}

/// A type name resolved by the database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedType {
    pub ty: ColumnType,
    /// `format_type`'s name (`integer` for `int4`), as V2 prints it in messages.
    pub display: String,
    pub oid: u32,
}

/// A result column of a prepared statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultColumn {
    pub name: String,
    pub oid: u32,
    /// `format_type`'s name of the column's type.
    pub display: String,
}

/// A native query to validate by preparing it (never executing it).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeQueryCheck {
    /// The statement to prepare, with arguments as `$1`, `$2`, ….
    pub sql: String,
    /// The type of each parameter, as written in metadata (keys of
    /// [`SourceCatalog::types`]).
    pub param_types: Vec<String>,
}

/// A database function (one overload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCatalog {
    pub name: QualifiedFunction,
    pub description: Option<String>,
    /// Input arguments in declaration order (`OUT` arguments excluded).
    pub args: Vec<FunctionArg>,
    /// The return type; for functions returning rows of a table, its row type.
    pub return_type: ColumnType,
    /// `RETURNS SETOF …`
    pub returns_set: bool,
    pub volatility: Volatility,
    /// `VARIADIC` last argument.
    pub variadic: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionArg {
    /// `None` for unnamed arguments.
    pub name: Option<String>,
    pub ty: ColumnType,
    pub has_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Volatility {
    Immutable,
    Stable,
    Volatile,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableCatalog {
    pub name: QualifiedTable,
    pub kind: TableKind,
    pub description: Option<String>,
    /// Columns in table order.
    pub columns: IndexMap<String, ColumnInfo>,
    pub primary_key: Option<Vec<String>>,
    pub unique_constraints: Vec<Vec<String>>,
    pub foreign_keys: Vec<ForeignKey>,
    /// The primary key and unique indexes, by name (the `on_conflict` targets), sorted by name.
    pub constraints: Vec<UniqueConstraint>,
    /// Which DML statements the relation accepts (views may not accept all).
    pub mutability: TableMutability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniqueConstraint {
    pub name: String,
    /// Indexed columns in index order (empty for expression indexes).
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableMutability {
    pub insertable: bool,
    pub updatable: bool,
    pub deletable: bool,
}

impl TableMutability {
    pub const ALL: TableMutability = TableMutability {
        insertable: true,
        updatable: true,
        deletable: true,
    };
}

impl TableCatalog {
    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.get(name)
    }

    /// The columns an enum table's rows are read from: its primary key when that is a single
    /// `text` column, and the comment column when there is exactly one other column and it
    /// is `text` too. `None` when the table cannot be an enum table at all (schema
    /// resolution reports why).
    pub fn enum_columns(&self) -> Option<(&str, Option<&str>)> {
        let is_text =
            |c: &ColumnInfo| c.ty.kind == TypeKind::Base && !c.ty.is_array() && c.ty.name == "text";
        let [key] = self.primary_key.as_deref()? else {
            return None;
        };
        let key = self.column(key).filter(|c| is_text(c))?;
        let others: Vec<&ColumnInfo> = self
            .columns
            .values()
            .filter(|c| c.name != key.name)
            .collect();
        let comment = match others.as_slice() {
            [c] if is_text(c) => Some(c.name.as_str()),
            _ => None,
        };
        Some((key.name.as_str(), comment))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub ty: ColumnType,
    pub nullable: bool,
    pub has_default: bool,
    pub is_identity: bool,
    pub is_generated: bool,
    /// `GENERATED ALWAYS AS IDENTITY`: cannot be inserted or updated.
    #[serde(default)]
    pub is_identity_always: bool,
    pub description: Option<String>,
}

impl ColumnInfo {
    /// May appear in an `INSERT` column list / `UPDATE … SET`.
    pub fn is_mutable(&self) -> bool {
        !self.is_generated && !self.is_identity_always
    }
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

    /// The name V2 prints for this type (`pgScalarTypeToText`), e.g. in `get_table_info`:
    /// `integer`, `float8`, `varchar`, `timestamptz`, `integer[]`.
    pub fn v2_text(&self) -> String {
        let base = pg_scalar_type_text(&self.name);
        format!("{base}{}", "[]".repeat(self.dimensions as usize))
    }
}

/// V2's canonical name of a Postgres scalar type given its `pg_type.typname`: the name its
/// `PGScalarType` prints back (`textToPGScalarType`, then `pgScalarTypeToText`). Types V2
/// has no constructor for keep their name.
pub fn pg_scalar_type_text(typname: &str) -> &str {
    match typname {
        "int2" => "smallint",
        "int4" => "integer",
        "int8" => "bigint",
        "float4" => "real",
        "bool" => "boolean",
        "char" => "bpchar",
        other => other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub constraint: String,
    pub ref_table: QualifiedTable,
    /// `(local column, referenced column)` pairs in constraint order.
    pub columns: Vec<(String, String)>,
}

#[cfg(test)]
mod v2_text_tests {
    use super::*;

    #[test]
    fn v2_type_names() {
        let ty = |name: &str, dimensions: u32| ColumnType {
            dimensions,
            ..ColumnType::builtin(name)
        };
        assert_eq!(ty("int4", 0).v2_text(), "integer");
        assert_eq!(ty("int4", 1).v2_text(), "integer[]");
        assert_eq!(ty("float8", 0).v2_text(), "float8");
        assert_eq!(ty("float4", 0).v2_text(), "real");
        assert_eq!(ty("varchar", 0).v2_text(), "varchar");
        assert_eq!(ty("timestamptz", 0).v2_text(), "timestamptz");
        assert_eq!(ty("char", 0).v2_text(), "bpchar");
        assert_eq!(ty("mood", 0).v2_text(), "mood");
    }
}
