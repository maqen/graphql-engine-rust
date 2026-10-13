//! Backend-agnostic intermediate representation (IR) of a GraphQL operation.
//!
//! `hge-schema` lowers a validated GraphQL request into these types; a backend
//! (`hge-postgres`, …) translates them into its query language. The IR is fully resolved:
//! names are database names (not GraphQL aliases, except for output keys), values carry their
//! database type, and permission filters are already merged in.

pub mod backend;
pub mod select;
pub mod value;

use std::sync::Arc;

pub use backend::{BoxFuture, SourceExecutor};
pub use select::*;
pub use value::*;

/// The kind of a GraphQL operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Query,
    Mutation,
    Subscription,
}

/// A lowered operation: one entry per root response key, in query order.
#[derive(Debug, Clone)]
pub struct Operation {
    pub kind: OperationKind,
    pub root_fields: Vec<(String, RootField)>,
}

/// What produces the value of one root field.
#[derive(Debug, Clone)]
pub enum RootField {
    /// A value fully computed while lowering (`__typename`, introspection).
    Static(serde_json::Value),
    /// A query against a database source.
    Database(Box<DatabaseRootField>),
}

/// A root field executed by a source's backend.
#[derive(Debug, Clone)]
pub struct DatabaseRootField {
    pub source: Arc<str>,
    pub query: QueryRoot,
}

/// Database root fields. Mutations, functions etc. add variants here.
#[derive(Debug, Clone)]
pub enum QueryRoot {
    /// `author(where: ...)`: a JSON array of rows.
    Select(Select),
    /// `author_by_pk(id: 1)`: a single JSON object or `null`.
    SelectOne(Select),
}
