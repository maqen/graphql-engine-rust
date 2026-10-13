//! Aggregate types and fields of a role's schema.
//!
//! Presence follows V2: the `T_aggregate` root field, `<rel>_aggregate` relationship fields and
//! aggregation predicates need `allow_aggregations` on the aggregated table (admin always has
//! it); ordering by `<rel>_aggregate` needs only select permission on the target.

use std::sync::Arc;

use hge_core::ColumnType;
use hge_ir::{AggregateFunction, PredicateArguments, PredicateFunction};
use indexmap::IndexMap;

use super::{AggregateFieldKind, relationship_aggregate_name};
use crate::build::Builder;
use crate::model::*;
use crate::scalars;
use crate::tables::{ColumnInfo, RelationshipInfo, TableInfo};

impl Builder<'_> {
    /// `T_aggregate(distinct_on, limit, offset, order_by, where): T_aggregate!` on the query
    /// root.
    pub(crate) fn add_table_aggregate_root_field(
        &mut self,
        table: &Arc<TableInfo>,
        root: &mut ObjectDef,
    ) {
        let root_field_allowed = self
            .access(table)
            .is_some_and(|a| a.query_root_fields.select_aggregate);
        if !root_field_allowed || !self.may_aggregate(table) {
            return;
        }
        self.ensure_table_types(table);
        let ty = self.ensure_aggregate_types(table);
        let description = table
            .root_fields
            .select_aggregate_comment
            .clone()
            .unwrap_or_else(|| {
                format!(
                    "fetch aggregated fields from the table: \"{}\"",
                    table.display_name()
                )
            });
        let field = FieldDef::new(
            table.root_fields.select_aggregate.clone(),
            TypeRef::nn(ty),
            FieldKind::SelectAggregate(table.clone()),
        )
        .describe(description);
        let field = self.with_select_args(field, table);
        root.fields.insert(field.name.clone(), field);
    }

    /// `<rel>_aggregate(…): <target>_aggregate!` on the object type of an array
    /// relationship's source table.
    pub(crate) fn relationship_aggregate_field(
        &mut self,
        rel: &Arc<RelationshipInfo>,
        target: &Arc<TableInfo>,
    ) -> Option<FieldDef> {
        if !self.may_aggregate(target) {
            return None;
        }
        let ty = self.ensure_aggregate_types(target);
        let field = FieldDef::new(
            relationship_aggregate_name(&rel.name),
            TypeRef::nn(ty),
            FieldKind::ArrayRelationshipAggregate(rel.clone()),
        )
        .describe("An aggregate relationship");
        Some(self.with_select_args(field, target))
    }

    /// `<rel>_aggregate: <target>_aggregate_order_by` in the source table's `order_by`.
    pub(crate) fn relationship_aggregate_order_by(
        &mut self,
        rel: &RelationshipInfo,
        target: &TableInfo,
    ) -> InputValueDef {
        let ty = self.ensure_aggregate_order_by(target);
        InputValueDef::new(relationship_aggregate_name(&rel.name), TypeRef::named(ty))
    }

    /// `<rel>_aggregate: <target>_aggregate_bool_exp` in the source table's `bool_exp`.
    pub(crate) fn relationship_aggregation_predicate(
        &mut self,
        rel: &RelationshipInfo,
        target: &TableInfo,
    ) -> Option<InputValueDef> {
        if !self.may_aggregate(target) {
            return None;
        }
        let ty = self.ensure_aggregate_bool_exp(target)?;
        Some(InputValueDef::new(
            relationship_aggregate_name(&rel.name),
            TypeRef::named(ty),
        ))
    }

    pub(crate) fn may_aggregate(&self, table: &TableInfo) -> bool {
        self.access(table)
            .is_some_and(|a| a.allow_aggregations && !a.columns.is_empty())
    }

    /// The columns of `table` the role may select, in table order.
    fn visible_columns(&self, table: &TableInfo) -> Vec<ColumnInfo> {
        let Some(access) = self.access(table) else {
            return vec![];
        };
        table
            .columns
            .values()
            .filter(|c| access.allows_column(&c.name))
            .cloned()
            .collect()
    }

    /// `T_aggregate`, `T_aggregate_fields` and the `T_<function>_fields` objects; returns the
    /// name of `T_aggregate`.
    pub(crate) fn ensure_aggregate_types(&mut self, table: &TableInfo) -> String {
        let name = table.aggregate_name();
        if self.schema.types.contains_key(&name) {
            return name;
        }
        let display = table.display_name();
        let fields_name = self.ensure_aggregate_fields(table);
        let aggregate = ObjectDef::new(
            name.clone(),
            Some(format!("aggregated selection of \"{display}\"")),
        )
        .field(FieldDef::new(
            "aggregate",
            TypeRef::named(fields_name),
            FieldKind::Aggregate(AggregateFieldKind::Aggregate),
        ))
        .field(FieldDef::new(
            "nodes",
            TypeRef::nn_list_nn(table.type_name()),
            FieldKind::Aggregate(AggregateFieldKind::Nodes),
        ));
        self.schema.add(TypeDef::Object(aggregate));
        name
    }

    /// `T_aggregate_fields` with `count` and one field per function that applies to some
    /// visible column.
    fn ensure_aggregate_fields(&mut self, table: &TableInfo) -> String {
        let columns = self.visible_columns(table);
        let mut count = FieldDef::new(
            "count",
            TypeRef::nn("Int"),
            FieldKind::Aggregate(AggregateFieldKind::Count),
        )
        .arg(InputValueDef::new("distinct", TypeRef::named("Boolean")));
        if !columns.is_empty() {
            count = count.arg(InputValueDef::new(
                "columns",
                TypeRef::nn(table.select_column_name()).list(),
            ));
        }
        let mut fields = ObjectDef::new(
            table.aggregate_fields_name(),
            Some(format!("aggregate fields of \"{}\"", table.display_name())),
        )
        .field(count);
        for function in AggregateFunction::ALL {
            let applicable = applicable_columns(&columns, function);
            if applicable.is_empty() {
                continue;
            }
            let ty = self.add_function_fields(table, function, &applicable);
            fields = fields.field(FieldDef::new(
                function.name(),
                TypeRef::named(ty),
                FieldKind::Aggregate(AggregateFieldKind::Function(function)),
            ));
        }
        let name = fields.name.clone();
        self.schema.add(TypeDef::Object(fields));
        name
    }

    /// `T_<function>_fields`: one nullable field per column. `sum`, `max` and `min` keep the
    /// column's type; the statistical functions return `Float`.
    fn add_function_fields(
        &mut self,
        table: &TableInfo,
        function: AggregateFunction,
        columns: &[&ColumnInfo],
    ) -> String {
        let keeps_type = function == AggregateFunction::Sum || function.is_comparison();
        let mut obj = ObjectDef::new(
            table.aggregate_function_fields_name(function),
            Some(format!("aggregate {} on columns", function.name())),
        );
        for col in columns {
            let ty = if keeps_type {
                scalars::column_type_ref(&col.ty, true)
            } else {
                TypeRef::named("Float")
            };
            obj = obj.field(
                FieldDef::new(
                    col.gql_name.clone(),
                    ty,
                    FieldKind::Column {
                        column: col.name.clone(),
                        ty: col.ty.clone(),
                    },
                )
                .describe_opt(col.description.clone()),
            );
        }
        let name = obj.name.clone();
        self.schema.add(TypeDef::Object(obj));
        name
    }

    /// `T_aggregate_order_by` and its `T_<function>_order_by` inputs.
    fn ensure_aggregate_order_by(&mut self, table: &TableInfo) -> String {
        let name = table.aggregate_order_by_name();
        if self.schema.types.contains_key(&name) {
            return name;
        }
        self.ensure_order_by_enum();
        let display = table.display_name();
        let columns = self.visible_columns(table);
        let mut fields = IndexMap::new();
        insert_input(
            &mut fields,
            InputValueDef::new("count", TypeRef::named("order_by")),
        );
        for function in AggregateFunction::ALL {
            let applicable = applicable_columns(&columns, function);
            if applicable.is_empty() {
                continue;
            }
            let mut op_fields = IndexMap::new();
            for col in applicable {
                insert_input(
                    &mut op_fields,
                    InputValueDef::new(col.gql_name.clone(), TypeRef::named("order_by"))
                        .describe_opt(col.description.clone()),
                );
            }
            let op_name = table.aggregate_function_order_by_name(function);
            self.schema.add(TypeDef::InputObject(InputObjectDef {
                name: op_name.clone(),
                description: Some(format!(
                    "order by {}() on columns of table \"{display}\"",
                    function.name()
                )),
                fields: op_fields,
            }));
            insert_input(
                &mut fields,
                InputValueDef::new(function.name(), TypeRef::named(op_name)),
            );
        }
        self.schema.add(TypeDef::InputObject(InputObjectDef {
            name: name.clone(),
            description: Some(format!("order by aggregate values of table \"{display}\"")),
            fields,
        }));
        name
    }

    /// `T_aggregate_bool_exp` with one `T_aggregate_bool_exp_<function>` per function that has
    /// suitable visible columns; `None` when there is none.
    fn ensure_aggregate_bool_exp(&mut self, table: &TableInfo) -> Option<String> {
        let name = table.aggregate_bool_exp_name();
        if self.schema.types.contains_key(&name) {
            return Some(name);
        }
        let mut fields = IndexMap::new();
        for function in PredicateFunction::ALL {
            if let Some(ty) = self.add_predicate_function(table, function) {
                insert_input(
                    &mut fields,
                    InputValueDef::new(function.name(), TypeRef::named(ty)),
                );
            }
        }
        if fields.is_empty() {
            return None;
        }
        self.schema.add(TypeDef::InputObject(InputObjectDef {
            name: name.clone(),
            description: None,
            fields,
        }));
        Some(name)
    }

    /// `T_aggregate_bool_exp_<function> {arguments, distinct, filter, predicate}`.
    fn add_predicate_function(
        &mut self,
        table: &TableInfo,
        function: PredicateFunction,
    ) -> Option<String> {
        let arguments = match function.arguments() {
            PredicateArguments::Star => {
                if self.visible_columns(table).is_empty() {
                    return None;
                }
                TypeRef::nn(table.select_column_name()).list()
            }
            PredicateArguments::Single(ty) => {
                TypeRef::nn(self.add_predicate_columns_enum(table, function, ty)?)
            }
            PredicateArguments::Pair => {
                let columns = self.add_predicate_columns_enum(table, function, "float8")?;
                TypeRef::nn(self.add_predicate_arguments(table, function, &columns))
            }
        };
        let result = function.result_type();
        self.note_scalar(&result);
        let comparison = self.ensure_comparison_exp(&result);
        let name = table.aggregate_bool_exp_function_name(function);
        let mut fields = IndexMap::new();
        for f in [
            InputValueDef::new("arguments", arguments),
            InputValueDef::new("distinct", TypeRef::named("Boolean")),
            InputValueDef::new("filter", TypeRef::named(table.bool_exp_name())),
            InputValueDef::new("predicate", TypeRef::nn(comparison)),
        ] {
            insert_input(&mut fields, f);
        }
        self.schema.add(TypeDef::InputObject(InputObjectDef {
            name: name.clone(),
            description: None,
            fields,
        }));
        Some(name)
    }

    /// `T_select_column_T_aggregate_bool_exp_<function>_arguments_columns`: the visible
    /// columns of exactly type `ty`; `None` when there is none.
    fn add_predicate_columns_enum(
        &mut self,
        table: &TableInfo,
        function: PredicateFunction,
        ty: &str,
    ) -> Option<String> {
        let columns: Vec<ColumnInfo> = self
            .visible_columns(table)
            .into_iter()
            .filter(|c| scalars::is_exactly(&c.ty, ty))
            .collect();
        if columns.is_empty() {
            return None;
        }
        let pred_name = format!(
            "{}_arguments_columns",
            table.aggregate_bool_exp_function_name(function)
        );
        let name = format!("{}_select_column_{pred_name}", table.gql_name);
        let values = columns
            .iter()
            .map(|c| {
                (
                    c.gql_name.clone(),
                    EnumValueDef {
                        name: c.gql_name.clone(),
                        description: Some("column name".to_owned()),
                        deprecation_reason: None,
                    },
                )
            })
            .collect();
        self.schema.add(TypeDef::Enum(EnumDef {
            name: name.clone(),
            description: Some(format!(
                "select \"{pred_name}\" columns of table \"{}\"",
                table.display_name()
            )),
            values,
        }));
        Some(name)
    }

    /// `T_aggregate_bool_exp_<function>_arguments {X, Y}` for two-argument functions.
    fn add_predicate_arguments(
        &mut self,
        table: &TableInfo,
        function: PredicateFunction,
        columns_enum: &str,
    ) -> String {
        let name = format!(
            "{}_arguments",
            table.aggregate_bool_exp_function_name(function)
        );
        let mut fields = IndexMap::new();
        for arg in ["X", "Y"] {
            insert_input(
                &mut fields,
                InputValueDef::new(arg, TypeRef::nn(columns_enum)),
            );
        }
        self.schema.add(TypeDef::InputObject(InputObjectDef {
            name: name.clone(),
            description: None,
            fields,
        }));
        name
    }
}

/// The columns an aggregate function applies to (V2: numeric for the statistical functions
/// and `sum`, comparable for `max` and `min`).
fn applicable_columns(columns: &[ColumnInfo], function: AggregateFunction) -> Vec<&ColumnInfo> {
    let applies = |ty: &ColumnType| {
        if function.is_comparison() {
            scalars::is_comparable(ty)
        } else {
            scalars::is_numeric(ty)
        }
    };
    columns.iter().filter(|c| applies(&c.ty)).collect()
}

fn insert_input(fields: &mut IndexMap<String, InputValueDef>, field: InputValueDef) {
    fields.insert(field.name.clone(), field);
}
