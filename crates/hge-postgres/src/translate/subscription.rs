//! Multiplexed subscription queries: one statement serves every cohort of a poller.
//!
//! Every value the translator would bind as `$n` is instead read from the cohort's variables
//! (a JSON array of text values), so the SQL text depends only on the shape of the
//! subscription. `$1` is a JSON array holding one such variables array per cohort:
//!
//! ```sql
//! SELECT "_subs"."idx"::text, (<field 1>)::text, …
//! FROM json_array_elements(($1)::json) WITH ORDINALITY AS "_subs"("vars", "idx")
//! ```
//!
//! A streaming subscription (always a single root field) joins its select laterally, because
//! it also returns the new cursor value:
//!
//! ```sql
//! SELECT "_subs"."idx"::text, "_st"."root"::text, "_st"."cursor" FROM … AS "_subs"(…)
//! LEFT JOIN LATERAL (
//!   SELECT coalesce(json_agg(<row> ORDER BY "_b1"."id" ASC), '[]') AS "root",
//!          (max("_b1"."id"))::text AS "cursor"
//!   FROM (SELECT "_t1".* FROM "public"."t" AS "_t1"
//!         WHERE <permission> AND ("_t1"."id" > <cursor>) AND <where>
//!         ORDER BY "_t1"."id" ASC FETCH FIRST (<batch size>) ROWS WITH TIES) AS "_b1"
//! ) AS "_st" ON ('true')
//! ```
//!
//! `WITH TIES` returns every row sharing the last cursor value, so none is skipped when
//! cursor values repeat (V2 does the same on Postgres).

use hge_core::{Code, ColumnType, Error, Session};
use hge_ir::{MultiplexedQuery, OrderDirection, QueryRoot, StreamSelect, Value};

use super::Translator;
use crate::sql::SqlBuilder;

const VARS: &str = r#""_subs"."vars""#;
const FROM_COHORTS: &str =
    r#" FROM json_array_elements(($1)::json) WITH ORDINALITY AS "_subs"("vars", "idx")"#;

/// Translate the root fields of a subscription (all on one source) into a multiplexed query.
pub fn translate_multiplexed(
    fields: &[&QueryRoot],
    session: &Session,
) -> Result<MultiplexedQuery, Error> {
    let mut t = Translator::with_builder(session, SqlBuilder::multiplexed(VARS));
    t.b.push(r#"SELECT "_subs"."idx"::text"#);
    let mut cursor = None;
    if let [QueryRoot::Stream(stream)] = fields {
        t.b.push(r#", "_st"."root"::text, "_st"."cursor""#);
        t.b.push(FROM_COHORTS);
        t.b.push(" LEFT JOIN LATERAL (");
        cursor = Some(t.select_stream(stream)?);
        t.b.push(r#") AS "_st" ON ('true')"#);
    } else {
        for field in fields {
            t.b.push(", (");
            match field {
                QueryRoot::Select(select) => t.select_many(select, None)?,
                QueryRoot::SelectOne(select) => t.select_one(select, None)?,
                QueryRoot::Count(count) => t.count(count)?,
                QueryRoot::Stream(_) => {
                    return Err(Error::new(
                        Code::NotSupported,
                        "exactly one root field is allowed for streaming subscriptions",
                    ));
                }
            }
            t.b.push(")::text");
        }
        t.b.push(FROM_COHORTS);
    }
    Ok(MultiplexedQuery {
        sql: t.b.sql,
        variables: t.b.params,
        cursor,
    })
}

impl Translator<'_> {
    /// The next batch of a stream as `"root"` (JSON array) and `"cursor"` (text). Returns the
    /// index of the cursor parameter.
    pub fn select_stream(&mut self, s: &StreamSelect) -> Result<usize, Error> {
        let sel = &s.select;
        let base = self.fresh("_b");
        let inner = self.fresh("_t");
        let (dir, cmp, agg) = match s.cursor.direction {
            OrderDirection::Asc => (" ASC", " > ", "max"),
            OrderDirection::Desc => (" DESC", " < ", "min"),
        };

        self.b.push("SELECT coalesce(json_agg(");
        self.row_json(&sel.fields, &base)?;
        self.b.push(" ORDER BY ");
        self.stream_cursor(s, &base)?;
        self.b.push(dir);
        self.b.push(r#"), '[]') AS "root", ("#);
        self.b.push(agg).push("(");
        self.stream_cursor(s, &base)?;
        self.b.push(r#"))::text AS "cursor" FROM (SELECT "#);
        self.b.ident(&inner).push(".* FROM ");
        self.b
            .ident(&sel.table.schema)
            .push(".")
            .ident(&sel.table.name);
        self.b.push(" AS ").ident(&inner).push(" WHERE ");
        if let Some(p) = &sel.permission_filter {
            self.bool_exp(p, &inner)?;
            self.b.push(" AND ");
        }
        self.b.push("(");
        self.stream_cursor(s, &inner)?;
        self.b.push(cmp);
        self.value(&s.cursor.initial_value)?;
        let cursor = self.b.params.len() - 1;
        self.b.push(")");
        if let Some(w) = &sel.args.where_ {
            self.b.push(" AND ");
            self.bool_exp(w, &inner)?;
        }
        self.b.push(" ORDER BY ");
        self.stream_cursor(s, &inner)?;
        self.b.push(dir);
        // A row limit from permissions caps the batch, like V2's `min(limit, batch_size)`.
        let batch = match sel.permission_limit {
            Some(l) => l.min(s.batch_size),
            None => s.batch_size,
        };
        self.b.push(" FETCH FIRST (");
        self.value(&Value::literal(
            batch.to_string(),
            ColumnType::builtin("int8"),
        ))?;
        self.b.push(") ROWS WITH TIES) AS ").ident(&base);
        Ok(cursor)
    }

    /// The cursor column of rows of `alias`; NULL where it is redacted for the role.
    fn stream_cursor(&mut self, s: &StreamSelect, alias: &str) -> Result<(), Error> {
        match &s.cursor.redact {
            None => {
                self.b.qualified(alias, &s.cursor.column);
            }
            Some(condition) => {
                self.b.push("(CASE WHEN ");
                self.bool_exp(condition, alias)?;
                self.b.push(" THEN ");
                self.b.qualified(alias, &s.cursor.column);
                self.b.push(" ELSE NULL END)");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use hge_core::QualifiedTable;
    use hge_ir::{BoolExp, ColumnField, CompareOp, Field, Select, SelectArgs, StreamCursor};

    use super::*;

    fn int4() -> ColumnType {
        ColumnType::builtin("int4")
    }

    fn select(where_: Option<BoolExp>) -> Select {
        Select {
            table: QualifiedTable::new("public", "author"),
            fields: vec![(
                "id".to_owned(),
                Field::Column(ColumnField {
                    column: "id".to_owned(),
                    ty: int4(),
                    stringify: false,
                    json_path: None,
                    redact: None,
                }),
            )],
            args: SelectArgs {
                where_,
                ..SelectArgs::default()
            },
            permission_filter: None,
            permission_limit: None,
        }
    }

    #[test]
    fn live_query_reads_values_from_cohort_variables() {
        let where_ = BoolExp::Compare {
            column: "id".to_owned(),
            ty: int4(),
            op: CompareOp::Eq(Value::literal("5", int4())),
        };
        let q = translate_multiplexed(
            &[&QueryRoot::Select(select(Some(where_)))],
            &Session::admin(),
        )
        .unwrap();
        assert_eq!(q.variables, vec![Some("5".to_owned())]);
        assert!(
            q.sql
                .contains(r#"("_subs"."vars"->>0)::"pg_catalog"."int4""#)
        );
        assert!(!q.sql.contains("($2)"));
        assert!(
            q.sql
                .starts_with(r#"SELECT "_subs"."idx"::text, (SELECT coalesce("#)
        );
        assert!(
            q.sql
                .ends_with(r#"WITH ORDINALITY AS "_subs"("vars", "idx")"#)
        );
        assert_eq!(q.cursor, None);
    }

    #[test]
    fn same_shape_same_sql() {
        let q = |v: &str| {
            let where_ = BoolExp::Compare {
                column: "id".to_owned(),
                ty: int4(),
                op: CompareOp::Gt(Value::literal(v, int4())),
            };
            translate_multiplexed(
                &[&QueryRoot::Select(select(Some(where_)))],
                &Session::admin(),
            )
            .unwrap()
        };
        assert_eq!(q("1").sql, q("2").sql);
        assert_ne!(q("1").variables, q("2").variables);
    }

    #[test]
    fn stream_cursor_and_batch_size_are_variables() {
        let stream = StreamSelect {
            select: select(None),
            batch_size: 2,
            cursor: StreamCursor {
                column: "id".to_owned(),
                ty: int4(),
                direction: OrderDirection::Desc,
                initial_value: Value::literal("10", int4()),
                redact: None,
            },
        };
        let q = translate_multiplexed(&[&QueryRoot::Stream(stream)], &Session::admin()).unwrap();
        assert_eq!(q.cursor, Some(0));
        assert_eq!(
            q.variables,
            vec![Some("10".to_owned()), Some("2".to_owned())]
        );
        assert!(q.sql.contains(r#"(min("_b1"."id"))::text AS "cursor""#));
        assert!(
            q.sql
                .contains(r#"("_t2"."id" < ("_subs"."vars"->>0)::"pg_catalog"."int4")"#)
        );
        assert!(q.sql.contains(r#"ORDER BY "_t2"."id" DESC FETCH FIRST (("_subs"."vars"->>1)::"pg_catalog"."int8") ROWS WITH TIES"#));
    }
}
