//! Lowering of aggregate selections, aggregate `order_by` items and aggregation predicates.

use hge_core::Error;
use hge_ir::{
    AggregateColumn, AggregateColumnField, AggregateCount, AggregateField, AggregateFunction,
    AggregateOrderBy, AggregateSelect, AggregateSelectField, AggregationPredicate, BoolExp,
    OrderBy, OrderByTarget, PredicateArguments, PredicateFunction, RelationshipAggregate,
};
use serde_json::{Map, Value as Json};

use super::AggregateFieldKind;
use crate::functions::ComputedFieldInfo;
use crate::lower::{HashKeyed, Lowerer, order_type};
use crate::model::FieldKind;
use crate::normalize::{Selected, SelectedField};
use crate::scalars;
use crate::tables::{RelationshipInfo, TableInfo};

impl Lowerer<'_> {
    /// `T_aggregate(…) { aggregate { … } nodes { … } }`
    pub(crate) fn aggregate_select(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
    ) -> Result<AggregateSelect, Error> {
        let mut fields = Vec::with_capacity(field.selection.len());
        for sel in &field.selection {
            let f = match sel.kind {
                Selected::Typename(t) => AggregateSelectField::Typename(t.to_owned()),
                Selected::Field(def) => match &def.kind {
                    FieldKind::Aggregate(AggregateFieldKind::Aggregate) => {
                        AggregateSelectField::Aggregate(self.aggregate_fields(table, sel)?)
                    }
                    FieldKind::Aggregate(AggregateFieldKind::Nodes) => {
                        AggregateSelectField::Nodes(self.fields(table, &sel.selection)?)
                    }
                    other => return Err(unexpected_kind(other)),
                },
            };
            fields.push((sel.alias.clone(), f));
        }
        Ok(AggregateSelect {
            table: table.name.clone(),
            fields,
            args: self.select_args(table, field)?,
            permission_filter: self.permission_filter(table),
            permission_limit: self.rs.select_access(table).and_then(|a| a.limit),
            from: None,
        })
    }

    /// `<rel>_aggregate(…) { … }` on a row.
    pub(crate) fn relationship_aggregate(
        &self,
        rel: &RelationshipInfo,
        field: &SelectedField<'_>,
    ) -> Result<RelationshipAggregate, Error> {
        let target = self.target(rel)?;
        Ok(RelationshipAggregate {
            mapping: rel.mapping.clone(),
            select: self.aggregate_select(target, field)?,
        })
    }

    /// The selection of `T_aggregate_fields`.
    fn aggregate_fields(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
    ) -> Result<Vec<(String, AggregateField)>, Error> {
        let mut out = Vec::with_capacity(field.selection.len());
        for sel in &field.selection {
            let f = match sel.kind {
                Selected::Typename(t) => AggregateField::Typename(t.to_owned()),
                Selected::Field(def) => match &def.kind {
                    FieldKind::Aggregate(AggregateFieldKind::Count) => {
                        AggregateField::Count(self.count(table, sel)?)
                    }
                    FieldKind::Aggregate(AggregateFieldKind::Function(function)) => {
                        AggregateField::Function {
                            function: *function,
                            fields: self.function_fields(table, sel)?,
                        }
                    }
                    other => return Err(unexpected_kind(other)),
                },
            };
            out.push((sel.alias.clone(), f));
        }
        Ok(out)
    }

    /// The selection of `T_<function>_fields`.
    fn function_fields(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
    ) -> Result<Vec<(String, AggregateColumnField)>, Error> {
        let mut out = Vec::with_capacity(field.selection.len());
        for sel in &field.selection {
            let f = match sel.kind {
                Selected::Typename(t) => AggregateColumnField::Typename(t.to_owned()),
                Selected::Field(def) => match &def.kind {
                    FieldKind::Column { column, ty } => AggregateColumnField::Column {
                        column: self.redacted(table, column),
                        ty: ty.clone(),
                        stringify: self.rs.options.stringify_numeric_types
                            && scalars::is_big_num(ty),
                    },
                    FieldKind::ComputedField(cf) => {
                        let ty = cf.scalar_type().ok_or_else(|| unexpected_kind(&def.kind))?;
                        AggregateColumnField::ComputedField {
                            call: self.computed_field_call(table, cf, sel)?,
                            redact: self.redaction(table, &cf.name).cloned().map(Box::new),
                            stringify: self.rs.options.stringify_numeric_types
                                && scalars::is_big_num(ty),
                        }
                    }
                    other => return Err(unexpected_kind(other)),
                },
            };
            out.push((sel.alias.clone(), f));
        }
        Ok(out)
    }

    /// `order_by: {<rel>_aggregate: {count: asc, max: {c: desc}}}`
    pub(crate) fn aggregate_order_by(
        &self,
        rel: &RelationshipInfo,
        value: &Json,
        path: &str,
        out: &mut Vec<OrderBy>,
    ) -> Result<(), Error> {
        let target = self.target(rel)?;
        self.aggregate_order_by_on(target, value, path, out, |permission_filter, aggregate| {
            OrderByTarget::ArrayAggregate {
                table: target.name.clone(),
                mapping: rel.mapping.clone(),
                permission_filter,
                aggregate,
            }
        })
    }

    /// `order_by: {<computed field>_aggregate: {…}}` for a computed field returning rows of a
    /// table and taking no arguments besides the row and the session.
    pub(crate) fn computed_field_aggregate_order_by(
        &self,
        table: &TableInfo,
        cf: &ComputedFieldInfo,
        target: &TableInfo,
        value: &Json,
        path: &str,
        out: &mut Vec<OrderBy>,
    ) -> Result<(), Error> {
        let call = self.computed_call(table, cf, None, path)?;
        self.aggregate_order_by_on(target, value, path, out, |permission_filter, aggregate| {
            OrderByTarget::ComputedFieldAggregate {
                call: call.clone(),
                permission_filter,
                aggregate,
            }
        })
    }

    /// The sort keys of a `T_aggregate_order_by` value over the rows of `target`; `make`
    /// builds each key from the target's permission filter and the aggregate.
    fn aggregate_order_by_on(
        &self,
        target: &TableInfo,
        value: &Json,
        path: &str,
        out: &mut Vec<OrderBy>,
        make: impl Fn(Option<Box<BoolExp>>, AggregateOrderBy) -> OrderByTarget,
    ) -> Result<(), Error> {
        let Json::Object(obj) = value else {
            return Ok(());
        };
        // Rows the role cannot select never count towards the sort key.
        let permission_filter = self.permission_filter(target).map(Box::new);
        let mut push = |aggregate: AggregateOrderBy, order: &Json| {
            let (direction, nulls) = order_type(order.as_str().unwrap_or("asc"));
            out.push(OrderBy {
                target: make(permission_filter.clone(), aggregate),
                direction,
                nulls,
            });
        };
        // V2 applies `count` first, then each function's columns in the hash order of their
        // database names (see `Lowerer::order_by`); functions keep the order written.
        if let Some(v) = obj.get("count").filter(|v| !v.is_null()) {
            push(AggregateOrderBy::Count, v);
        }
        for (key, v) in obj {
            if v.is_null() || key == "count" {
                continue;
            }
            let function = AggregateFunction::from_name(key).ok_or_else(|| {
                Error::validation(format!(
                    "field '{key}' not found in type: '{}'",
                    target.aggregate_order_by_name()
                ))
                .with_path(format!("{path}.{key}"))
            })?;
            let Json::Object(columns) = v else {
                continue;
            };
            let mut columns: Vec<HashKeyed<(&String, &Json)>> = columns
                .iter()
                .map(|item| HashKeyed {
                    field: target
                        .column_by_gql_name(item.0)
                        .map_or_else(|| item.0.clone(), |c| c.name.clone()),
                    item,
                })
                .collect();
            hge_core::sort_v2_hash_order(&mut columns);
            for HashKeyed {
                item: (name, order),
                ..
            } in columns
            {
                if order.is_null() {
                    continue;
                }
                let column = self.column(target, name, &format!("{path}.{key}.{name}"))?;
                push(AggregateOrderBy::Function { function, column }, order);
            }
        }
        Ok(())
    }

    /// `where: {<rel>_aggregate: {<function>: {arguments, distinct, filter, predicate}}}`
    pub(crate) fn aggregation_predicate(
        &self,
        rel: &RelationshipInfo,
        value: &Json,
        path: &str,
    ) -> Result<BoolExp, Error> {
        let target = self.target(rel)?;
        let given: Vec<(&String, &Json)> = match value {
            Json::Object(obj) => obj.iter().filter(|(_, v)| !v.is_null()).collect(),
            _ => vec![],
        };
        let [(name, Json::Object(input))] = given.as_slice() else {
            return Err(
                Error::validation("exactly one predicate should be specified")
                    .with_path(path.to_owned()),
            );
        };
        let path = format!("{path}.{name}");
        let function = PredicateFunction::from_name(name).ok_or_else(|| {
            Error::validation(format!(
                "field '{name}' not found in type: '{}'",
                target.aggregate_bool_exp_name()
            ))
            .with_path(path.clone())
        })?;
        let arguments = self.predicate_arguments(target, function, input, &path)?;
        let filter = match input.get("filter") {
            Some(Json::Object(f)) => self.bool_exp(target, f, &format!("{path}.filter"))?,
            _ => None,
        };
        let mut comparisons = vec![];
        if let Some(Json::Object(ops)) = input.get("predicate") {
            let ty = function.result_type();
            for op in self.comparisons(&ty, ops, &format!("{path}.predicate"))? {
                comparisons.push(BoolExp::Compare {
                    column: AggregationPredicate::RESULT_COLUMN.to_owned(),
                    ty: ty.clone(),
                    op,
                });
            }
        }
        // `distinct` is accepted but, as in V2's Postgres backend, not applied.
        Ok(BoolExp::AggregationPredicate(Box::new(
            AggregationPredicate {
                table: target.name.clone(),
                mapping: rel.mapping.clone(),
                // Rows the role cannot select are never aggregated.
                permission_filter: self.permission_filter(target),
                function,
                arguments,
                filter,
                predicate: BoolExp::And(comparisons),
            },
        )))
    }

    /// `count(columns: …, distinct: …)`
    fn count(&self, table: &TableInfo, field: &SelectedField<'_>) -> Result<AggregateCount, Error> {
        let Some(Json::Array(items)) = field.arg("columns") else {
            return Ok(AggregateCount::Star);
        };
        let path = field.arg_path("columns");
        let columns = items
            .iter()
            .map(|c| self.column(table, c.as_str().unwrap_or_default(), &path))
            .collect::<Result<Vec<_>, _>>()?;
        if columns.is_empty() {
            return Ok(AggregateCount::Star);
        }
        let distinct = field
            .arg("distinct")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        Ok(AggregateCount::Columns { columns, distinct })
    }

    /// The argument columns of an aggregation predicate, in the function's argument order
    /// (`corr(Y, X)`); empty means `*`.
    fn predicate_arguments(
        &self,
        table: &TableInfo,
        function: PredicateFunction,
        input: &Map<String, Json>,
        path: &str,
    ) -> Result<Vec<AggregateColumn>, Error> {
        let path = format!("{path}.arguments");
        let arg = input.get("arguments").unwrap_or(&Json::Null);
        match (function.arguments(), arg) {
            (PredicateArguments::Star, Json::Array(items)) => items
                .iter()
                .map(|c| self.column(table, c.as_str().unwrap_or_default(), &path))
                .collect(),
            (PredicateArguments::Star, _) => Ok(vec![]),
            (PredicateArguments::Single(_), Json::String(c)) => {
                Ok(vec![self.column(table, c, &path)?])
            }
            (PredicateArguments::Pair, Json::Object(pair)) => ["Y", "X"]
                .iter()
                .map(|k| {
                    let c = pair.get(*k).and_then(Json::as_str).unwrap_or_default();
                    self.column(table, c, &format!("{path}.{k}"))
                })
                .collect(),
            _ => Err(Error::validation("missing required field 'arguments'").with_path(path)),
        }
    }

    /// The column with GraphQL name `name`, which the role must be allowed to select, redacted
    /// for the role if it must be. The schema only offers permitted columns; this re-checks.
    fn column(&self, table: &TableInfo, name: &str, path: &str) -> Result<AggregateColumn, Error> {
        table
            .column_by_gql_name(name)
            .filter(|c| {
                self.rs
                    .select_access(table)
                    .is_some_and(|a| a.allows_column(&c.name))
            })
            .map(|c| self.redacted(table, &c.name))
            .ok_or_else(|| {
                Error::validation(format!("unknown column '{name}'")).with_path(path.to_owned())
            })
    }

    /// `column` as an aggregate reads it: NULL where the role's redaction condition fails.
    fn redacted(&self, table: &TableInfo, column: &str) -> AggregateColumn {
        AggregateColumn {
            column: column.to_owned(),
            redact: self.redaction(table, column).cloned().map(Box::new),
        }
    }
}

fn unexpected_kind(kind: &FieldKind) -> Error {
    Error::unexpected(format!(
        "unexpected field kind {kind:?} in aggregate selection"
    ))
}
