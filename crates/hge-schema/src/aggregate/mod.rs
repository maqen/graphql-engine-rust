//! Aggregates: the `T_aggregate` root field, `<rel>_aggregate` relationship fields,
//! `<rel>_aggregate` in `order_by` and aggregation predicates in `bool_exp`
//! (V2: `GraphQL/Schema/Select.hs` `tableAggregationFields`, `OrderBy.hs`
//! `orderByAggregation`, `BoolExp/AggregationPredicates.hs`).
//!
//! [`schema`] adds the types to a role's schema, [`lower`] turns the selections and arguments
//! into IR.

mod lower;
mod schema;

use std::sync::Arc;

use hge_ir::{AggregateFunction, PredicateFunction};

use crate::tables::{RelationshipInfo, TableInfo};

/// What a field of an aggregate object means, for the lowering step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFieldKind {
    /// `T_aggregate.aggregate`
    Aggregate,
    /// `T_aggregate.nodes`
    Nodes,
    /// `T_aggregate_fields.count`
    Count,
    /// `T_aggregate_fields.sum`, `.avg`, …
    Function(AggregateFunction),
}

/// Names of the aggregate types of a table.
impl TableInfo {
    /// `T_aggregate`
    pub fn aggregate_name(&self) -> String {
        format!("{}_aggregate", self.gql_name)
    }

    /// `T_aggregate_fields`
    pub fn aggregate_fields_name(&self) -> String {
        format!("{}_aggregate_fields", self.gql_name)
    }

    /// `T_sum_fields`, `T_avg_fields`, …
    pub fn aggregate_function_fields_name(&self, function: AggregateFunction) -> String {
        format!("{}_{}_fields", self.gql_name, function.name())
    }

    /// `T_aggregate_order_by`
    pub fn aggregate_order_by_name(&self) -> String {
        format!("{}_aggregate_order_by", self.gql_name)
    }

    /// `T_sum_order_by`, `T_avg_order_by`, …
    pub fn aggregate_function_order_by_name(&self, function: AggregateFunction) -> String {
        format!("{}_{}_order_by", self.gql_name, function.name())
    }

    /// `T_aggregate_bool_exp`
    pub fn aggregate_bool_exp_name(&self) -> String {
        format!("{}_aggregate_bool_exp", self.gql_name)
    }

    /// `T_aggregate_bool_exp_count`, …
    pub fn aggregate_bool_exp_function_name(&self, function: PredicateFunction) -> String {
        format!("{}_aggregate_bool_exp_{}", self.gql_name, function.name())
    }
}

/// The field name of an array relationship's aggregate (`articles_aggregate`).
pub fn relationship_aggregate_name(relationship: &str) -> String {
    format!("{relationship}_aggregate")
}

/// The array relationship of `table` whose `<rel>_aggregate` input field `key` is.
pub(crate) fn aggregate_relationship<'t>(
    table: &'t TableInfo,
    key: &str,
) -> Option<&'t Arc<RelationshipInfo>> {
    let name = key.strip_suffix("_aggregate")?;
    table.array_relationships.iter().find(|r| r.name == name)
}
