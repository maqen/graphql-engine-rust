//! Mutations: inserts (with nested relationship inserts and upserts), updates and deletes.
//!
//! Permission rules are already merged in: `check` is the insert/update permission check,
//! `filter`s include the permission row filter, and preset columns are ordinary values.

use std::sync::Arc;

use hge_core::{ColumnType, QualifiedTable};

use crate::{BoolExp, Field, Value};

/// A mutation root field executed by a source's backend.
#[derive(Debug, Clone)]
pub struct MutationRootField {
    pub source: Arc<str>,
    pub mutation: Mutation,
    /// `validate_input` webhook calls to make, in order, before the mutation runs (one per
    /// table of a nested insert whose permission has one).
    pub validations: Vec<InputValidationCall>,
}

/// One `validate_input` webhook call: the webhook and the `data.input` it is sent.
#[derive(Debug, Clone)]
pub struct InputValidationCall {
    pub webhook: Arc<crate::InputValidation>,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum Mutation {
    /// `insert_t` / `insert_t_one`.
    Insert(Box<Insert>),
    /// `update_t` / `update_t_by_pk`.
    Update(Box<Update>),
    /// `update_t_many`: batches run in order; the result is a JSON array of responses.
    UpdateMany(Vec<Update>),
    /// `delete_t` / `delete_t_by_pk`.
    Delete(Box<Delete>),
    /// A tracked `VOLATILE` function: its rows, selected inside the mutation transaction.
    Function(Box<crate::QueryRoot>),
}

/// What a mutation returns.
#[derive(Debug, Clone)]
pub enum MutationOutput {
    /// `t_mutation_response`: requested fields by response key, in query order.
    Multi(Vec<(String, MutationResponseField)>),
    /// `insert_t_one`, `*_by_pk`: the fields of the affected row, or `null` if none.
    Single(Vec<(String, Field)>),
}

#[derive(Debug, Clone)]
pub enum MutationResponseField {
    AffectedRows,
    /// The affected rows (not filtered by the select permission, as in V2).
    Returning(Vec<(String, Field)>),
    Typename(String),
}

impl MutationOutput {
    /// Does producing the output read other rows than the affected ones (relationships)?
    /// Then it must run in a separate statement that sees the mutation's effects.
    pub fn has_relationships(&self) -> bool {
        let rel = |fields: &[(String, Field)]| {
            fields.iter().any(|(_, f)| {
                matches!(
                    f,
                    Field::ObjectRelationship(_) | Field::ArrayRelationship(_)
                )
            })
        };
        match self {
            MutationOutput::Single(fields) => rel(fields),
            MutationOutput::Multi(fields) => fields.iter().any(|(_, f)| match f {
                MutationResponseField::Returning(r) => rel(r),
                _ => false,
            }),
        }
    }
}

/// The table a mutation targets: name plus every column (in table order), which backends
/// need to carry affected rows between statements.
#[derive(Debug, Clone)]
pub struct MutationTable {
    pub name: QualifiedTable,
    pub columns: Vec<(String, ColumnType)>,
}

#[derive(Debug, Clone)]
pub struct Insert {
    pub into: InsertInto,
    pub output: MutationOutput,
}

/// Rows to insert into one table, with their nested relationship inserts.
#[derive(Debug, Clone)]
pub struct InsertInto {
    pub table: MutationTable,
    pub objects: Vec<InsertObject>,
    pub on_conflict: Option<OnConflict>,
    /// The insert permission check (and, for upserts, the update permission check).
    pub check: Option<InsertCheck>,
    /// JSON path errors of this insert are reported at (`$.selectionSet.f.args.objects`).
    pub path: String,
}

impl InsertInto {
    /// Any relationship data in any row (rows must then be inserted one at a time).
    pub fn is_nested(&self) -> bool {
        self.objects
            .iter()
            .any(|o| !o.before.is_empty() || !o.after.is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct InsertCheck {
    /// Checked on inserted rows.
    pub insert: BoolExp,
    /// Checked instead on rows updated by `ON CONFLICT DO UPDATE` (V2 uses the insert check
    /// when the role has no update check).
    pub update: Option<BoolExp>,
}

#[derive(Debug, Clone)]
pub struct InsertObject {
    /// Column values (including presets), in input order. Missing columns get `DEFAULT`.
    pub columns: Vec<(String, Value)>,
    /// Object relationships inserted before this row; their returned columns fill
    /// `mapping`'s local columns of this row.
    pub before: Vec<RelationshipInsert>,
    /// Relationships inserted after this row, with this row's mapped columns.
    pub after: Vec<RelationshipInsert>,
    /// Path of this object (`…objects[0]`), used for nested insert errors.
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct RelationshipInsert {
    pub name: String,
    /// `(column on the parent table, column on the inserted table)`.
    pub mapping: Vec<(String, String)>,
    pub insert: InsertInto,
    /// Path of the relationship field in the parent object (`…objects[0].author`).
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct OnConflict {
    pub constraint: String,
    /// Legacy `/v1/query` inserts only: a conflict target other than `constraint` (which is
    /// then ignored). GraphQL upserts always name a constraint and leave this `None`.
    pub target: Option<ConflictTarget>,
    /// Empty: `DO NOTHING`.
    pub update_columns: Vec<String>,
    /// Update-permission presets applied by `DO UPDATE`.
    pub presets: Vec<(String, Value)>,
    /// `DO UPDATE … WHERE`: update permission filter AND the user's `where`.
    pub filter: Option<BoolExp>,
}

/// The conflict target of a legacy `/v1/query` insert (`on_conflict.constraint_on`, or no
/// target at all with `"action": "ignore"`).
#[derive(Debug, Clone)]
pub enum ConflictTarget {
    /// `ON CONFLICT (c1, c2)`: Postgres infers the unique index on exactly these columns.
    Columns(Vec<String>),
    /// `ON CONFLICT DO NOTHING` without a target: any unique violation is ignored. Only
    /// valid with empty `update_columns`.
    Any,
}

#[derive(Debug, Clone)]
pub struct Update {
    pub table: MutationTable,
    /// `SET` items in input order. Empty: nothing is executed and an empty response is
    /// returned (V2 behaviour).
    pub set: Vec<(String, UpdateExp)>,
    /// Permission filter AND the user's `where` (or the primary key equality).
    pub filter: BoolExp,
    pub check: Option<BoolExp>,
    pub output: MutationOutput,
}

/// The new value of one column.
#[derive(Debug, Clone)]
pub enum UpdateExp {
    /// `_set` and presets.
    Set(Value),
    /// `_inc`
    Inc(Value),
    /// `_append` (jsonb `||`)
    Append(Value),
    /// `_prepend`
    Prepend(Value),
    /// `_delete_key` (jsonb `- text`)
    DeleteKey(Value),
    /// `_delete_elem` (jsonb `- int`)
    DeleteElem(Value),
    /// `_delete_at_path` (jsonb `#- text[]`)
    DeleteAtPath(Value),
    /// Legacy `/v1/query` `$mul`: `column * value`.
    Mul(Value),
    /// Legacy `/v1/query` `$default`: `column = DEFAULT`.
    Default,
}

#[derive(Debug, Clone)]
pub struct Delete {
    pub table: MutationTable,
    /// Permission filter AND the user's `where` (or the primary key equality).
    pub filter: BoolExp,
    pub output: MutationOutput,
}
