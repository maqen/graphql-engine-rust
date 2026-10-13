//! Backend-agnostic intermediate representation (IR) of a GraphQL operation.
//!
//! `hge-schema` lowers a validated GraphQL request into these types; a backend
//! (`hge-postgres`, …) translates them into its query language. The IR is fully resolved:
//! names are database names (not GraphQL aliases, except for output keys), values carry their
//! database type, and permission filters are already merged in.

pub mod action;
pub mod aggregate;
pub mod backend;
pub mod count;
pub mod mutation;
pub mod permissions;
pub mod select;
pub mod stream;
pub mod value;

use std::sync::Arc;

pub use action::{ActionCall, ActionRootField};
pub use aggregate::*;
pub use backend::{BoxFuture, MultiplexedQuery, MultiplexedRow, SourceExecutor};
pub use count::Count;
pub use mutation::*;
pub use permissions::*;
pub use select::*;
pub use stream::*;
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
    /// `subscription @_multiple_top_level_fields`: V2's test-only directive that lifts the
    /// one-root-field limit of subscriptions.
    pub multiple_top_level_fields: bool,
}

/// What produces the value of one root field.
#[derive(Debug, Clone)]
pub enum RootField {
    /// A value fully computed while lowering (`__typename`, introspection).
    Static(serde_json::Value),
    /// A query against a database source.
    Database(Box<DatabaseRootField>),
    /// A mutation against a database source.
    Mutation(Box<MutationRootField>),
    /// An action (HTTP handler call).
    Action(Box<ActionRootField>),
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
    /// Legacy `count` (`/v1/query`): a JSON number.
    Count(Count),
    /// `author_stream(...)` (subscriptions only): a JSON array of the next rows.
    Stream(Box<StreamSelect>),
    /// `author_aggregate { … }`: a single JSON object.
    Aggregate(AggregateSelect),
}
