//! Aggregates, computed by Postgres so the JSON formatting of every value matches V2
//! (`avg` of an integer column is a `numeric` with scale, `sum` keeps the column type, …).
//!
//! ```sql
//! SELECT json_build_object(
//!          'aggregate', json_build_object('count', count(*),
//!                                         'sum', json_build_object('id', sum("_b1"."id"))),
//!          'nodes', coalesce(json_agg(row_to_json(…) ORDER BY …), '[]'))
//! FROM (SELECT * FROM "public"."author" AS "_t1" WHERE … LIMIT …) AS "_b1"
//! GROUP BY ()
//! ```
//!
//! `GROUP BY ()` makes the query an aggregate even when only literals (`__typename`) are
//! selected, so it always yields exactly one row.

use hge_core::{ColumnType, Error, QualifiedTable};
use hge_ir::{
    AggregateColumn, AggregateColumnField, AggregateCount, AggregateField, AggregateOrderBy,
    AggregateSelect, AggregateSelectField, AggregationPredicate, BoolExp, Field,
};

use super::Translator;
use super::select::{BaseQuery, Join, MAX_JSON_BUILD_PAIRS, effective_limit};

impl Translator<'_> {
    /// A single JSON object (`T_aggregate`) over the rows of `sel`.
    pub fn select_aggregate(&mut self, sel: &AggregateSelect, join: Join<'_>) -> Result<(), Error> {
        let (base, inner) = self.base_aliases();
        // With aggregates over the rows, V2 applies the permission limit to `nodes` only.
        let (limit, nodes_limit) = if sel.permission_limit_on_nodes_only() {
            (sel.args.limit, sel.permission_limit)
        } else {
            (effective_limit(sel.args.limit, sel.permission_limit), None)
        };

        self.b.push("SELECT ");
        self.json_object(&sel.fields, |t, field| match field {
            AggregateSelectField::Aggregate(fields) => t.aggregate_object(fields, &base),
            AggregateSelectField::Nodes(fields) => t.nodes(fields, sel, &base, nodes_limit),
            AggregateSelectField::Typename(name) => {
                t.b.string_literal(name);
                Ok(())
            }
        })?;
        self.b.push(" FROM (");
        let query = BaseQuery {
            table: &sel.table,
            from: sel.from.as_ref(),
            args: &sel.args,
            permission_filter: sel.permission_filter.as_ref(),
        };
        self.base(query, &inner, join, limit)?;
        self.b.push(") AS ").ident(&base).push(" GROUP BY ()");
        Ok(())
    }

    /// `nodes`: the rows as a JSON array, in `order_by` order, cut to `limit` if given.
    fn nodes(
        &mut self,
        fields: &[(String, Field)],
        sel: &AggregateSelect,
        base: &str,
        limit: Option<u64>,
    ) -> Result<(), Error> {
        let Some(limit) = limit else {
            self.b.push("coalesce(json_agg(");
            self.row_json(fields, base)?;
            self.node_order(&sel.args.order_by, base);
            self.b.push("), '[]')");
            return Ok(());
        };
        self.b.push("coalesce(array_to_json((array_agg(");
        self.row_json(fields, base)?;
        self.node_order(&sel.args.order_by, base);
        self.b.push("))[1:");
        let limit = i32::try_from(limit).unwrap_or(i32::MAX);
        self.b
            .param(Some(limit.to_string()), &ColumnType::builtin("int4"));
        self.b.push("]), '[]')");
        Ok(())
    }

    /// The `aggregate` object (`T_aggregate_fields`).
    fn aggregate_object(
        &mut self,
        fields: &[(String, AggregateField)],
        base: &str,
    ) -> Result<(), Error> {
        self.json_object(fields, |t, field| {
            match field {
                AggregateField::Count(count) => t.aggregate_count(count, base)?,
                AggregateField::Function { function, fields } => {
                    t.json_object(fields, |t, field| {
                        match field {
                            AggregateColumnField::Column {
                                column, stringify, ..
                            } => {
                                if *stringify {
                                    t.b.push("(");
                                }
                                t.b.push(function.name()).push("(");
                                t.aggregate_column(column, base)?;
                                t.b.push(")");
                                if *stringify {
                                    t.b.push(")::text");
                                }
                            }
                            AggregateColumnField::Typename(name) => {
                                t.b.string_literal(name);
                            }
                        }
                        Ok(())
                    })?;
                }
                AggregateField::Typename(name) => {
                    t.b.string_literal(name);
                }
            }
            Ok(())
        })
    }

    /// `count(*)`, `count(("a", "b"))` or `count(DISTINCT ("a", "b"))` (V2's `CountType`).
    fn aggregate_count(&mut self, count: &AggregateCount, base: &str) -> Result<(), Error> {
        match count {
            AggregateCount::Star => {
                self.b.push("count(*)");
            }
            AggregateCount::Columns { columns, distinct } => {
                self.b.push("count(");
                if *distinct {
                    self.b.push("DISTINCT ");
                }
                self.b.push("(");
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        self.b.push(", ");
                    }
                    self.aggregate_column(c, base)?;
                }
                self.b.push("))");
            }
        }
        Ok(())
    }

    /// A column of a row of `alias`, as `CASE WHEN <redact> THEN col ELSE NULL END` when it is
    /// redacted.
    fn aggregate_column(&mut self, c: &AggregateColumn, alias: &str) -> Result<(), Error> {
        let Some(condition) = &c.redact else {
            self.b.qualified(alias, &c.column);
            return Ok(());
        };
        self.b.push("(CASE WHEN ");
        self.bool_exp(condition, alias)?;
        self.b.push(" THEN ");
        self.b.qualified(alias, &c.column);
        self.b.push(" ELSE NULL END)");
        Ok(())
    }

    /// `json_build_object('key', <value>, …)` with literal keys, so response keys of any
    /// length are safe.
    fn json_object<T>(
        &mut self,
        items: &[(String, T)],
        mut value: impl FnMut(&mut Self, &T) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if items.len() > MAX_JSON_BUILD_PAIRS {
            return Err(Error::validation(format!(
                "an aggregate selection can have at most {MAX_JSON_BUILD_PAIRS} fields"
            )));
        }
        self.b.push("json_build_object(");
        for (i, (key, item)) in items.iter().enumerate() {
            if i > 0 {
                self.b.push(", ");
            }
            self.b.string_literal(key).push(", ");
            value(self, item)?;
        }
        self.b.push(")");
        Ok(())
    }

    /// An `order_by: {<rel>_aggregate: …}` key for a row of `alias`: the aggregate over the
    /// related rows that pass `filter`.
    pub(crate) fn aggregate_order_target(
        &mut self,
        table: &QualifiedTable,
        mapping: &[(String, String)],
        filter: Option<&BoolExp>,
        aggregate: &AggregateOrderBy,
        alias: &str,
    ) -> Result<(), Error> {
        let rel = self.fresh("_o");
        self.b.push("(SELECT ");
        match aggregate {
            AggregateOrderBy::Count => {
                self.b.push("count(*)");
            }
            AggregateOrderBy::Function { function, column } => {
                self.b.push(function.name()).push("(");
                self.aggregate_column(column, &rel)?;
                self.b.push(")");
            }
        }
        self.related_rows(table, &rel, mapping, alias);
        if let Some(f) = filter {
            self.b.push(" AND ");
            self.bool_exp(f, &rel)?;
        }
        self.b.push(")");
        Ok(())
    }

    /// `EXISTS (SELECT 1 FROM (SELECT <fn>(<args>) AS "aggregate" FROM <related rows>) AS s
    /// WHERE <predicate on s>)`, like V2's `translateAVAggregationPredicates`.
    pub(crate) fn aggregation_predicate(
        &mut self,
        p: &AggregationPredicate,
        alias: &str,
    ) -> Result<(), Error> {
        let rel = self.fresh("_e");
        let sub = self.fresh("_s");
        self.b.push("EXISTS (SELECT 1 FROM (SELECT ");
        self.b.push(p.function.name()).push("(");
        if p.arguments.is_empty() {
            self.b.push("*");
        }
        for (i, c) in p.arguments.iter().enumerate() {
            if i > 0 {
                self.b.push(", ");
            }
            self.aggregate_column(c, &rel)?;
        }
        self.b
            .push(") AS ")
            .ident(AggregationPredicate::RESULT_COLUMN);
        self.related_rows(&p.table, &rel, &p.mapping, alias);
        for exp in [&p.permission_filter, &p.filter].into_iter().flatten() {
            self.b.push(" AND ");
            self.bool_exp(exp, &rel)?;
        }
        self.b.push(") AS ").ident(&sub).push(" WHERE ");
        self.bool_exp(&p.predicate, &sub)?;
        self.b.push(")");
        Ok(())
    }

    /// ` FROM <table> AS <rel> WHERE <rel joined to the row of alias>`
    fn related_rows(
        &mut self,
        table: &QualifiedTable,
        rel: &str,
        mapping: &[(String, String)],
        alias: &str,
    ) {
        self.b.push(" FROM ");
        self.b.ident(&table.schema).push(".").ident(&table.name);
        self.b.push(" AS ").ident(rel).push(" WHERE ('true')");
        for (local, remote) in mapping {
            self.b.push(" AND (");
            self.b.qualified(rel, remote).push(" = ");
            self.b.qualified(alias, local).push(")");
        }
    }
}

#[cfg(test)]
mod tests {
    use hge_core::Session;
    use hge_ir::{AggregateFunction, SelectArgs};

    use super::*;

    fn select(fields: Vec<(String, AggregateSelectField)>, limit: Option<u64>) -> AggregateSelect {
        AggregateSelect {
            table: QualifiedTable::public("author"),
            fields,
            args: SelectArgs::default(),
            permission_filter: None,
            permission_limit: limit,
            from: None,
        }
    }

    fn sql(sel: &AggregateSelect) -> String {
        let session = Session::admin();
        let mut t = Translator::new(&session);
        t.select_aggregate(sel, None).unwrap();
        t.b.sql
    }

    #[test]
    fn counts_like_v2() {
        let count = |c: AggregateCount| ("c".to_owned(), AggregateField::Count(c));
        let sel = select(
            vec![(
                "aggregate".to_owned(),
                AggregateSelectField::Aggregate(vec![
                    count(AggregateCount::Star),
                    count(AggregateCount::Columns {
                        columns: vec![AggregateColumn::plain("a"), AggregateColumn::plain("b")],
                        distinct: false,
                    }),
                    count(AggregateCount::Columns {
                        columns: vec![AggregateColumn::plain("a")],
                        distinct: true,
                    }),
                ]),
            )],
            None,
        );
        assert_eq!(
            sql(&sel),
            r#"SELECT json_build_object('aggregate', json_build_object('c', count(*), 'c', count(("_b1"."a", "_b1"."b")), 'c', count(DISTINCT ("_b1"."a")))) FROM (SELECT "_t1".* FROM "public"."author" AS "_t1") AS "_b1" GROUP BY ()"#
        );
    }

    #[test]
    fn permission_limit_cuts_only_nodes_when_aggregating() {
        let sum = AggregateField::Function {
            function: AggregateFunction::Sum,
            fields: vec![(
                "id".to_owned(),
                AggregateColumnField::Column {
                    column: AggregateColumn::plain("id"),
                    ty: ColumnType::builtin("int4"),
                    stringify: false,
                },
            )],
        };
        let nodes = AggregateSelectField::Nodes(vec![(
            "__typename".to_owned(),
            Field::Typename("author".to_owned()),
        )]);
        let both = select(
            vec![
                (
                    "aggregate".to_owned(),
                    AggregateSelectField::Aggregate(vec![("sum".to_owned(), sum)]),
                ),
                ("nodes".to_owned(), nodes.clone()),
            ],
            Some(2),
        );
        let sql_both = sql(&both);
        assert!(sql_both.contains(r#"json_build_object('id', sum("_b1"."id"))"#));
        assert!(sql_both.contains("array_to_json((array_agg("));
        assert!(!sql_both.contains(" LIMIT "));

        let nodes_only = select(vec![("nodes".to_owned(), nodes)], Some(2));
        let sql_nodes = sql(&nodes_only);
        assert!(sql_nodes.contains("coalesce(json_agg("));
        assert!(sql_nodes.contains(r#" LIMIT ($1)::"pg_catalog"."int8""#));
    }
}
