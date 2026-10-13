//! Selections from tables.

use hge_core::{ColumnType, QualifiedTable};

use crate::Value;

/// A selection of rows from a table, with nested fields.
#[derive(Debug, Clone)]
pub struct Select {
    pub table: QualifiedTable,
    /// Output fields keyed by response key, in query order.
    pub fields: Vec<(String, Field)>,
    pub args: SelectArgs,
    /// Row filter from permissions, ANDed with `args.where_`.
    pub permission_filter: Option<BoolExp>,
    /// Row limit from permissions; the effective limit is `min(args.limit, permission_limit)`.
    pub permission_limit: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct SelectArgs {
    pub where_: Option<BoolExp>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub distinct_on: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Field {
    Column(ColumnField),
    /// A constant string, used for `__typename`.
    Typename(String),
    ObjectRelationship(Box<RelationshipSelect>),
    ArrayRelationship(Box<RelationshipSelect>),
}

#[derive(Debug, Clone)]
pub struct ColumnField {
    pub column: String,
    pub ty: ColumnType,
    /// Return the value as a JSON string (`HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`).
    pub stringify: bool,
    /// `path` argument on json/jsonb columns, already parsed into path elements.
    pub json_path: Option<Vec<JsonPathElement>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JsonPathElement {
    Key(String),
    Index(i64),
}

/// A nested selection through a relationship. `mapping` is `(this column, remote column)`.
#[derive(Debug, Clone)]
pub struct RelationshipSelect {
    pub mapping: Vec<(String, String)>,
    pub select: Select,
}

#[derive(Debug, Clone)]
pub struct OrderBy {
    pub target: OrderByTarget,
    pub direction: OrderDirection,
    pub nulls: NullsOrder,
}

#[derive(Debug, Clone)]
pub enum OrderByTarget {
    Column(String),
    /// A column reached through an object relationship
    /// (`mapping`: `(this column, remote column)`).
    ObjectRelationship {
        table: QualifiedTable,
        mapping: Vec<(String, String)>,
        target: Box<OrderByTarget>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder {
    First,
    Last,
}

/// A boolean expression over the rows of a table.
#[derive(Debug, Clone, PartialEq)]
pub enum BoolExp {
    And(Vec<BoolExp>),
    Or(Vec<BoolExp>),
    Not(Box<BoolExp>),
    Compare {
        column: String,
        ty: ColumnType,
        op: CompareOp,
    },
    /// The related rows (through `mapping`, `(this column, remote column)`) satisfy `exp`.
    Relationship {
        table: QualifiedTable,
        mapping: Vec<(String, String)>,
        exp: Box<BoolExp>,
    },
    /// `_exists`: some row of an unrelated table satisfies `exp`.
    Exists {
        table: QualifiedTable,
        exp: Box<BoolExp>,
    },
}

impl BoolExp {
    pub fn always_true() -> Self {
        BoolExp::And(vec![])
    }

    /// AND two optional expressions.
    pub fn and(a: Option<BoolExp>, b: Option<BoolExp>) -> Option<BoolExp> {
        match (a, b) {
            (None, x) | (x, None) => x,
            (Some(a), Some(b)) => Some(BoolExp::And(vec![a, b])),
        }
    }
}

/// Comparison operators (`_eq`, `_like`, `_contains`, …).
#[derive(Debug, Clone, PartialEq)]
pub enum CompareOp {
    Eq(Value),
    Neq(Value),
    Gt(Value),
    Lt(Value),
    Gte(Value),
    Lte(Value),
    /// The value has the column's array type.
    In(Value),
    Nin(Value),
    Like(Value),
    NLike(Value),
    ILike(Value),
    NILike(Value),
    Similar(Value),
    NSimilar(Value),
    Regex(Value),
    NRegex(Value),
    IRegex(Value),
    NIRegex(Value),
    IsNull(bool),
    /// jsonb `@>`
    Contains(Value),
    /// jsonb `<@`
    ContainedIn(Value),
    /// jsonb `?`
    HasKey(Value),
    /// jsonb `?|` (value is `text[]`)
    HasKeysAny(Value),
    /// jsonb `?&` (value is `text[]`)
    HasKeysAll(Value),
}
