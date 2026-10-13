//! Values that end up as query parameters.

use hge_core::ColumnType;

/// A scalar value bound into a query, already in the database's text representation.
///
/// Backends bind these as parameters (never by string interpolation) cast to `ty`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A literal from the query or variables. `None` is SQL `NULL`.
    Literal {
        text: Option<String>,
        ty: ColumnType,
    },
    /// A session variable (`x-hasura-user-id`), resolved at execution time.
    Session { name: String, ty: ColumnType },
}

impl Value {
    pub fn literal(text: impl Into<String>, ty: ColumnType) -> Self {
        Value::Literal {
            text: Some(text.into()),
            ty,
        }
    }

    pub fn null(ty: ColumnType) -> Self {
        Value::Literal { text: None, ty }
    }

    pub fn ty(&self) -> &ColumnType {
        match self {
            Value::Literal { ty, .. } | Value::Session { ty, .. } => ty,
        }
    }
}
