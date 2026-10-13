//! Legacy `count`, the same SQL V2 runs:
//!
//! ```sql
//! SELECT count(*) FROM (SELECT DISTINCT "_t1"."c1", … FROM "public"."t" AS "_t1" WHERE …) AS "_r1"
//! ```

use hge_core::Error;
use hge_ir::Count;

use super::Translator;

impl Translator<'_> {
    pub fn count(&mut self, q: &Count) -> Result<(), Error> {
        let (inner, outer) = (self.fresh("_t"), self.fresh("_r"));
        self.b.push("SELECT count(*) FROM (SELECT ");
        if q.distinct.is_empty() {
            self.b.push("1");
        } else {
            self.b.push("DISTINCT ");
            for (i, c) in q.distinct.iter().enumerate() {
                if i > 0 {
                    self.b.push(", ");
                }
                self.b.qualified(&inner, c);
            }
        }
        self.b.push(" FROM ");
        self.b.ident(&q.table.schema).push(".").ident(&q.table.name);
        self.b.push(" AS ").ident(&inner);
        let filters = [&q.permission_filter, &q.where_];
        for (i, f) in filters.into_iter().flatten().enumerate() {
            self.b.push(if i == 0 { " WHERE " } else { " AND " });
            self.bool_exp(f, &inner)?;
        }
        self.b.push(") AS ").ident(&outer);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use hge_core::{ColumnType, QualifiedTable, Session};
    use hge_ir::{BoolExp, CompareOp, Count, QueryRoot, Value};

    use crate::translate::translate_query;

    #[test]
    fn distinct_count() {
        let q = QueryRoot::Count(Count {
            table: QualifiedTable::public("article"),
            distinct: vec!["author_id".into()],
            permission_filter: None,
            where_: Some(BoolExp::Compare {
                column: "is_published".into(),
                ty: ColumnType::builtin("bool"),
                op: CompareOp::Eq(Value::literal("true", ColumnType::builtin("bool"))),
            }),
        });
        let sql = translate_query(&[&q], &Session::admin()).unwrap();
        assert_eq!(
            sql.sql,
            r#"SELECT (SELECT count(*) FROM (SELECT DISTINCT "_t1"."author_id" FROM "public"."article" AS "_t1" WHERE ("_t1"."is_published" = ($1)::"pg_catalog"."bool")) AS "_r2")::text"#
        );
    }
}
