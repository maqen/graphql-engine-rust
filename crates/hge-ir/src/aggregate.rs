//! Aggregates: `T_aggregate` selections, ordering by an array relationship's aggregate and
//! aggregation predicates (`where: {<rel>_aggregate: {count: …}}`).

use hge_core::{ColumnType, QualifiedTable};

use crate::{BoolExp, Field, FunctionCall, SelectArgs};

/// `author_aggregate(where: …) { aggregate { … } nodes { … } }`: one JSON object computed over
/// the rows left after the filters, ordering, `limit` and `offset`.
#[derive(Debug, Clone)]
pub struct AggregateSelect {
    pub table: QualifiedTable,
    /// Output fields keyed by response key, in query order.
    pub fields: Vec<(String, AggregateSelectField)>,
    pub args: SelectArgs,
    /// Row filter from permissions, ANDed with `args.where_`.
    pub permission_filter: Option<BoolExp>,
    /// Row limit from permissions. As in V2, it limits only `nodes` when an `aggregate` field
    /// is selected or the rows are ordered by an aggregate; otherwise it limits all rows.
    pub permission_limit: Option<u64>,
    /// Read the rows from this function (returning rows of `table`) instead of the table:
    /// `<function>_aggregate`.
    pub from_function: Option<FunctionCall>,
}

impl AggregateSelect {
    /// Whether the permission limit applies to `nodes` only (V2's `PLSQRequired`): the
    /// aggregates then see every row allowed by the filters and the requested `limit`.
    pub fn permission_limit_on_nodes_only(&self) -> bool {
        self.permission_limit.is_some()
            && (self
                .fields
                .iter()
                .any(|(_, f)| matches!(f, AggregateSelectField::Aggregate(_)))
                || self
                    .args
                    .order_by
                    .iter()
                    .any(|ob| ob.target.is_array_aggregate()))
    }
}

/// A field of the `T_aggregate` object.
#[derive(Debug, Clone)]
pub enum AggregateSelectField {
    /// `aggregate { count sum { … } … }`
    Aggregate(Vec<(String, AggregateField)>),
    /// `nodes { … }`: the rows, as a JSON array.
    Nodes(Vec<(String, Field)>),
    /// `__typename`
    Typename(String),
}

/// A field of the `T_aggregate_fields` object.
#[derive(Debug, Clone)]
pub enum AggregateField {
    Count(AggregateCount),
    /// `sum { a b }`: one JSON object with the function applied to each column.
    Function {
        function: AggregateFunction,
        fields: Vec<(String, AggregateColumnField)>,
    },
    Typename(String),
}

/// A field of an operator object such as `T_sum_fields`.
#[derive(Debug, Clone)]
pub enum AggregateColumnField {
    Column {
        column: AggregateColumn,
        /// The column's type; it decides stringification, not the function's result type.
        ty: ColumnType,
        /// Return the value as a JSON string (`HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`).
        stringify: bool,
    },
    Typename(String),
}

/// A column an aggregate reads. For an inherited role that redacts the column, the value is
/// NULL on rows where `redact` fails, as in `nodes` (V2 aggregates the redacted values).
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateColumn {
    pub column: String,
    pub redact: Option<Box<BoolExp>>,
}

impl AggregateColumn {
    /// A column read as is.
    pub fn plain(column: impl Into<String>) -> Self {
        AggregateColumn {
            column: column.into(),
            redact: None,
        }
    }
}

/// `count(columns: …, distinct: …)` (V2's `CountType`).
#[derive(Debug, Clone, PartialEq)]
pub enum AggregateCount {
    /// `COUNT(*)`
    Star,
    /// `COUNT((a, b))`, or `COUNT(DISTINCT (a, b))`.
    Columns {
        columns: Vec<AggregateColumn>,
        distinct: bool,
    },
}

/// The aggregate functions of `T_aggregate_fields` and `T_aggregate_order_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggregateFunction {
    Avg,
    Max,
    Min,
    Stddev,
    StddevPop,
    StddevSamp,
    Sum,
    VarPop,
    VarSamp,
    Variance,
}

impl AggregateFunction {
    /// Every function, in GraphQL field order.
    pub const ALL: [AggregateFunction; 10] = [
        AggregateFunction::Avg,
        AggregateFunction::Max,
        AggregateFunction::Min,
        AggregateFunction::Stddev,
        AggregateFunction::StddevPop,
        AggregateFunction::StddevSamp,
        AggregateFunction::Sum,
        AggregateFunction::VarPop,
        AggregateFunction::VarSamp,
        AggregateFunction::Variance,
    ];

    /// The GraphQL field name, which is also the SQL function name.
    pub fn name(self) -> &'static str {
        match self {
            AggregateFunction::Avg => "avg",
            AggregateFunction::Max => "max",
            AggregateFunction::Min => "min",
            AggregateFunction::Stddev => "stddev",
            AggregateFunction::StddevPop => "stddev_pop",
            AggregateFunction::StddevSamp => "stddev_samp",
            AggregateFunction::Sum => "sum",
            AggregateFunction::VarPop => "var_pop",
            AggregateFunction::VarSamp => "var_samp",
            AggregateFunction::Variance => "variance",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name() == name)
    }

    /// `max` and `min` apply to comparable columns; the others to numeric ones.
    pub fn is_comparison(self) -> bool {
        matches!(self, AggregateFunction::Max | AggregateFunction::Min)
    }
}

/// A nested aggregate through an array relationship (`mapping`: `(this column, remote
/// column)`).
#[derive(Debug, Clone)]
pub struct RelationshipAggregate {
    pub mapping: Vec<(String, String)>,
    pub select: AggregateSelect,
}

/// What an `order_by: {<rel>_aggregate: …}` item sorts by.
#[derive(Debug, Clone, PartialEq)]
pub enum AggregateOrderBy {
    /// `count: asc`
    Count,
    /// `max: {column: desc}`
    Function {
        function: AggregateFunction,
        column: AggregateColumn,
    },
}

/// `<rel>_aggregate: {<function>: {arguments: …, filter: …, predicate: {…}}}` in a boolean
/// expression: the aggregate over the related rows satisfies `predicate`.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregationPredicate {
    pub table: QualifiedTable,
    /// `(this column, remote column)`
    pub mapping: Vec<(String, String)>,
    /// The related table's select permission filter.
    pub permission_filter: Option<BoolExp>,
    pub function: PredicateFunction,
    /// Argument columns; empty means `*` (only for `count`).
    pub arguments: Vec<AggregateColumn>,
    /// `filter`: restricts the related rows before aggregating.
    pub filter: Option<BoolExp>,
    /// Comparisons on the aggregate's value, which is exposed as the column
    /// [`AggregationPredicate::RESULT_COLUMN`].
    pub predicate: BoolExp,
}

impl AggregationPredicate {
    /// The name the predicate's comparisons use for the aggregate's value.
    pub const RESULT_COLUMN: &'static str = "aggregate";
}

/// The functions usable in aggregation predicates (V2's Postgres `aggregationFunctions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PredicateFunction {
    Avg,
    BoolAnd,
    BoolOr,
    Corr,
    Count,
    CovarSamp,
    Max,
    Min,
    StddevSamp,
    Sum,
    VarSamp,
}

/// The arguments a predicate function takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateArguments {
    /// `count`: an optional list of any columns, `*` when empty.
    Star,
    /// One column of the given type.
    Single(&'static str),
    /// `corr`, `covar_samp`: an object `{Y, X}` of two `float8` columns.
    Pair,
}

impl PredicateFunction {
    /// Every function, in GraphQL field order.
    pub const ALL: [PredicateFunction; 11] = [
        PredicateFunction::Avg,
        PredicateFunction::BoolAnd,
        PredicateFunction::BoolOr,
        PredicateFunction::Corr,
        PredicateFunction::Count,
        PredicateFunction::CovarSamp,
        PredicateFunction::Max,
        PredicateFunction::Min,
        PredicateFunction::StddevSamp,
        PredicateFunction::Sum,
        PredicateFunction::VarSamp,
    ];

    /// The GraphQL field name, which is also the SQL function name.
    pub fn name(self) -> &'static str {
        match self {
            PredicateFunction::Avg => "avg",
            PredicateFunction::BoolAnd => "bool_and",
            PredicateFunction::BoolOr => "bool_or",
            PredicateFunction::Corr => "corr",
            PredicateFunction::Count => "count",
            PredicateFunction::CovarSamp => "covar_samp",
            PredicateFunction::Max => "max",
            PredicateFunction::Min => "min",
            PredicateFunction::StddevSamp => "stddev_samp",
            PredicateFunction::Sum => "sum",
            PredicateFunction::VarSamp => "var_samp",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name() == name)
    }

    pub fn arguments(self) -> PredicateArguments {
        match self {
            PredicateFunction::Count => PredicateArguments::Star,
            PredicateFunction::BoolAnd | PredicateFunction::BoolOr => {
                PredicateArguments::Single("bool")
            }
            PredicateFunction::Corr | PredicateFunction::CovarSamp => PredicateArguments::Pair,
            _ => PredicateArguments::Single("float8"),
        }
    }

    /// The Postgres type of the value `predicate` compares (V2 declares `max`, `min` and
    /// `sum` as `float8`, like their argument).
    pub fn result_type(self) -> ColumnType {
        ColumnType::builtin(match self {
            PredicateFunction::Count => "int4",
            PredicateFunction::BoolAnd | PredicateFunction::BoolOr => "bool",
            _ => "float8",
        })
    }
}
