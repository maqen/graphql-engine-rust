//! Action root fields: calls to an HTTP handler, plus joins from the handler's output to
//! tables. Executed by `hge-actions`; the handler configuration (URL, headers, transforms) is
//! looked up there by action name.

use std::sync::Arc;

use hge_core::ColumnType;

use crate::{AggregateSelect, Select};

/// One action root field.
#[derive(Debug, Clone)]
pub struct ActionRootField {
    pub action: Arc<str>,
    pub call: ActionCall,
}

#[derive(Debug, Clone)]
pub enum ActionCall {
    /// A query action or a synchronous mutation action: call the handler now.
    Sync {
        /// The `input` object of the handler payload.
        input: serde_json::Value,
        output: Arc<ActionOutput>,
        selection: ActionSelection,
    },
    /// An asynchronous mutation action: record the call, return its id.
    AsyncMutation { input: serde_json::Value },
    /// `<action>(id: uuid!)`: the stored result of an asynchronous action.
    AsyncQuery {
        id: String,
        output: Arc<ActionOutput>,
        fields: Vec<(String, AsyncField)>,
        /// The output object has relationships, which switches V2 to session-checked reads.
        has_relationships: bool,
    },
}

/// The result fields of an asynchronous action (`id`, `created_at`, `errors`, `output`).
#[derive(Debug, Clone)]
pub enum AsyncField {
    Typename(String),
    Id,
    CreatedAt,
    Errors,
    Output(ActionSelection),
}

/// What the handler must return: the declared output type, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionOutput {
    /// `output_type` with its list / non-null wrappers.
    pub ty: OutputType,
    /// How the base (named) type is treated.
    pub base: OutputBase,
}

/// A GraphQL output type.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputType {
    Named(String),
    List(Box<OutputType>),
    NonNull(Box<OutputType>),
}

impl OutputType {
    pub fn base(&self) -> &str {
        match self {
            OutputType::Named(n) => n,
            OutputType::List(t) | OutputType::NonNull(t) => t.base(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputBase {
    /// A custom object; `required` lists its non-null fields (checked in the response).
    Object { required: Vec<String> },
    /// `Int`, `Float`, `String`, `Boolean`, `ID`: the response is type-checked.
    BuiltinScalar,
    /// A custom or Postgres scalar: any JSON is accepted and returned as is.
    CustomScalar,
}

/// The selection on an action's output.
#[derive(Debug, Clone)]
pub enum ActionSelection {
    /// A scalar output: return the value.
    Leaf,
    /// An object output (or list of objects): project the selected fields.
    Object(Vec<(String, ActionField)>),
}

#[derive(Debug, Clone)]
pub enum ActionField {
    Typename(String),
    /// A scalar or enum field, read from the handler's JSON by its name.
    Scalar {
        name: String,
    },
    /// A field of a nested custom object type.
    Object {
        name: String,
        fields: Vec<(String, ActionField)>,
    },
    /// A relationship from the output object to a table.
    Relationship(Box<ActionRelationship>),
}

/// A join from an action output object to a table, run after the handler responds.
#[derive(Debug, Clone)]
pub struct ActionRelationship {
    pub source: Arc<str>,
    /// `(output field name, table column, column type)`.
    pub mapping: Vec<(String, String, ColumnType)>,
    /// The selection on the table, with the role's permissions and the user's arguments; the
    /// join condition is added per output object at execution.
    pub target: ActionRelTarget,
}

/// What an action relationship field selects from the table.
#[derive(Debug, Clone)]
pub enum ActionRelTarget {
    /// An object relationship: one row or `null`.
    Object(Select),
    /// An array relationship: the matching rows.
    Array(Select),
    /// `<rel>_aggregate` of an array relationship.
    Aggregate(AggregateSelect),
}
