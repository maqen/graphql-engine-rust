//! Mutations: `INSERT` / `UPDATE` / `DELETE` statements and their JSON output.
//!
//! Every statement wraps the DML in a CTE named [`ROWS`] (`RETURNING *` plus, when a
//! permission check applies, a boolean [`CHECK_COLUMN`]). Then, either
//!
//! * [`Statement::Output`]: the response JSON is built over the CTE in the same statement
//!   (when the output reads no relationships), or
//! * [`Statement::Fetch`]: the affected rows come back as text arrays, and a second statement
//!   ([`output_from_rows`]) rebuilds them with `unnest` and evaluates the output. A
//!   data-modifying CTE's effects are invisible to the rest of its own statement, so
//!   relationships in the output (and nested inserts) need that second statement.

use hge_core::{ColumnType, Error, Session};
use hge_ir::{
    InsertCheck, InsertInto, MutationOutput, MutationResponseField, MutationTable, OnConflict,
    Update, UpdateExp, Value,
};

use super::Translator;
use super::select::MAX_JSON_BUILD_PAIRS;
use crate::sql::SqlBuilder;

/// The CTE holding the affected rows.
const ROWS: &str = "_hge_rows";
/// Alias of the target table inside the DML.
const TARGET: &str = "_hge_target";
/// The permission check column added to `RETURNING`.
const CHECK_COLUMN: &str = "_hge_check";

/// What the statement returns.
#[derive(Debug, Clone, Copy)]
pub enum Statement<'a> {
    /// One row: `(output json, check passed)`.
    Output(&'a MutationOutput),
    /// One row: `(affected row count, rows as a JSON array of text arrays, check passed)`.
    Fetch,
}

/// One row to insert: values for (some) columns; the rest get `DEFAULT`.
pub type Row<'a> = Vec<(&'a str, &'a Value)>;

/// `INSERT` of `rows` into `into.table` (with its `on_conflict` and check).
pub fn insert(
    into: &InsertInto,
    rows: &[Row<'_>],
    stmt: Statement<'_>,
    session: &Session,
) -> Result<SqlBuilder, Error> {
    let mut t = Translator::new(session);
    let has_check = into.check.is_some();
    t.b.push("WITH ").ident(ROWS).push(" AS (INSERT INTO ");
    t.table(&into.table);
    t.b.push(" AS ").ident(TARGET);

    // Columns in first-seen order across all rows.
    let mut columns: Vec<&str> = vec![];
    for row in rows {
        for (c, _) in row {
            if !columns.contains(c) {
                columns.push(c);
            }
        }
    }
    if !columns.is_empty() {
        t.b.push(" (");
        for (i, c) in columns.iter().enumerate() {
            if i > 0 {
                t.b.push(", ");
            }
            t.b.ident(c);
        }
        t.b.push(")");
    }
    t.b.push(" VALUES ");
    for (r, row) in rows.iter().enumerate() {
        if r > 0 {
            t.b.push(", ");
        }
        t.b.push("(");
        if columns.is_empty() {
            t.b.push("DEFAULT");
        }
        for (i, c) in columns.iter().enumerate() {
            if i > 0 {
                t.b.push(", ");
            }
            match row.iter().find(|(name, _)| name == c) {
                Some((_, v)) => t.value(v)?,
                None => {
                    t.b.push("DEFAULT");
                }
            }
        }
        t.b.push(")");
    }
    if let Some(oc) = &into.on_conflict {
        t.on_conflict(oc)?;
    }
    t.b.push(" RETURNING *");
    if let Some(check) = &into.check {
        t.b.push(", (");
        t.insert_check(check, into.on_conflict.is_some())?;
        t.b.push(")::boolean AS ").ident(CHECK_COLUMN);
    }
    t.b.push(")");
    t.finish(&into.table, stmt, has_check)?;
    Ok(t.b)
}

/// `UPDATE` (the caller skips updates with nothing to set).
pub fn update(u: &Update, stmt: Statement<'_>, session: &Session) -> Result<SqlBuilder, Error> {
    let mut t = Translator::new(session);
    t.b.push("WITH ").ident(ROWS).push(" AS (UPDATE ");
    t.table(&u.table);
    t.b.push(" AS ").ident(TARGET).push(" SET ");
    for (i, (column, exp)) in u.set.iter().enumerate() {
        if i > 0 {
            t.b.push(", ");
        }
        t.b.ident(column).push(" = ");
        t.update_exp(column, exp)?;
    }
    t.b.push(" WHERE ");
    t.bool_exp(&u.filter, TARGET)?;
    t.b.push(" RETURNING *");
    if let Some(check) = &u.check {
        t.b.push(", (");
        t.bool_exp(check, TARGET)?;
        t.b.push(")::boolean AS ").ident(CHECK_COLUMN);
    }
    t.b.push(")");
    t.finish(&u.table, stmt, u.check.is_some())?;
    Ok(t.b)
}

/// `DELETE`.
pub fn delete(
    d: &hge_ir::Delete,
    stmt: Statement<'_>,
    session: &Session,
) -> Result<SqlBuilder, Error> {
    let mut t = Translator::new(session);
    t.b.push("WITH ").ident(ROWS).push(" AS (DELETE FROM ");
    t.table(&d.table);
    t.b.push(" AS ").ident(TARGET).push(" WHERE ");
    t.bool_exp(&d.filter, TARGET)?;
    t.b.push(" RETURNING *)");
    t.finish(&d.table, stmt, false)?;
    Ok(t.b)
}

/// Evaluate `output` over rows fetched by an earlier [`Statement::Fetch`] (one inner vector
/// per row, values in `table.columns` order, as text). `affected` is the reported
/// `affected_rows`. One row, one column: the output JSON.
pub fn output_from_rows(
    table: &MutationTable,
    rows: &[Vec<Option<String>>],
    affected: u64,
    output: &MutationOutput,
    session: &Session,
) -> Result<SqlBuilder, Error> {
    let mut t = Translator::new(session);
    t.b.push("WITH ").ident(ROWS).push(" AS (");
    if table.columns.is_empty() || rows.is_empty() {
        t.b.push("SELECT * FROM ");
        t.table(table);
        t.b.push(" WHERE false");
    } else {
        // One text[] parameter per column keeps the statement text independent of the row
        // count, so it stays in the prepared-statement cache.
        t.b.push("SELECT ");
        for (i, (name, ty)) in table.columns.iter().enumerate() {
            if i > 0 {
                t.b.push(", ");
            }
            t.b.push("(").qualified("_u", name).push(")::");
            crate::sql::push_type(&mut t.b.sql, ty);
            t.b.push(" AS ").ident(name);
        }
        t.b.push(" FROM unnest(");
        let text_array = ColumnType::builtin("text").array_of();
        for (i, _) in table.columns.iter().enumerate() {
            if i > 0 {
                t.b.push(", ");
            }
            let column: Vec<Option<&str>> = rows
                .iter()
                .map(|r| r.get(i).and_then(|v| v.as_deref()))
                .collect();
            t.b.param(Some(text_array_literal(&column)), &text_array);
        }
        t.b.push(") AS ").ident("_u").push("(");
        for (i, (name, _)) in table.columns.iter().enumerate() {
            if i > 0 {
                t.b.push(", ");
            }
            t.b.ident(name);
        }
        t.b.push(")");
    }
    t.b.push(") SELECT (");
    t.output(output, Some(affected))?;
    t.b.push(")::text");
    Ok(t.b)
}

impl Translator<'_> {
    fn table(&mut self, table: &MutationTable) {
        self.b
            .ident(&table.name.schema)
            .push(".")
            .ident(&table.name.name);
    }

    /// The `SELECT` after the DML CTE.
    fn finish(
        &mut self,
        table: &MutationTable,
        stmt: Statement<'_>,
        has_check: bool,
    ) -> Result<(), Error> {
        self.b.push(" SELECT ");
        match stmt {
            Statement::Output(output) => {
                self.b.push("(");
                self.output(output, None)?;
                self.b.push(")::text");
            }
            Statement::Fetch => {
                self.b
                    .push("(SELECT count(*) FROM ")
                    .ident(ROWS)
                    .push(")::text, ");
                self.b.push("(SELECT coalesce(json_agg(ARRAY[");
                for (i, (name, _)) in table.columns.iter().enumerate() {
                    if i > 0 {
                        self.b.push(", ");
                    }
                    self.b.push("(").qualified(ROWS, name).push(")::text");
                }
                self.b
                    .push("]::text[]), '[]') FROM ")
                    .ident(ROWS)
                    .push(")::text");
            }
        }
        if has_check {
            self.b
                .push(", (SELECT coalesce(bool_and(")
                .ident(CHECK_COLUMN)
                .push("), true) FROM ")
                .ident(ROWS)
                .push(")::text");
        } else {
            self.b.push(", 'true'");
        }
        Ok(())
    }

    /// The response JSON over the rows in [`ROWS`]. `affected` overrides the row count
    /// (nested inserts count rows of every table).
    fn output(&mut self, output: &MutationOutput, affected: Option<u64>) -> Result<(), Error> {
        match output {
            MutationOutput::Multi(fields) => {
                if fields.len() > MAX_JSON_BUILD_PAIRS {
                    return Err(Error::validation(format!(
                        "a mutation response can have at most {MAX_JSON_BUILD_PAIRS} fields"
                    )));
                }
                self.b.push("json_build_object(");
                for (i, (key, field)) in fields.iter().enumerate() {
                    if i > 0 {
                        self.b.push(", ");
                    }
                    self.b.string_literal(key).push(", ");
                    match field {
                        MutationResponseField::AffectedRows => match affected {
                            Some(n) => {
                                self.b
                                    .param(Some(n.to_string()), &ColumnType::builtin("int8"));
                            }
                            None => {
                                self.b.push("(SELECT count(*) FROM ").ident(ROWS).push(")");
                            }
                        },
                        MutationResponseField::Returning(fields) => {
                            let r = self.fresh("_r");
                            self.b.push("(SELECT coalesce(json_agg(");
                            self.row_json(fields, &r)?;
                            self.b
                                .push("), '[]') FROM ")
                                .ident(ROWS)
                                .push(" AS ")
                                .ident(&r);
                            self.b.push(")");
                        }
                        MutationResponseField::Typename(t) => {
                            self.b.string_literal(t);
                        }
                    }
                }
                self.b.push(")");
            }
            MutationOutput::Single(fields) => {
                let r = self.fresh("_r");
                self.b.push("SELECT ");
                self.row_json(fields, &r)?;
                self.b
                    .push(" FROM ")
                    .ident(ROWS)
                    .push(" AS ")
                    .ident(&r)
                    .push(" LIMIT 1");
            }
        }
        Ok(())
    }

    fn insert_check(&mut self, check: &InsertCheck, upsert: bool) -> Result<(), Error> {
        match (&check.update, upsert) {
            (Some(update), true) => {
                // `xmax = 0` iff the row was inserted rather than updated by ON CONFLICT.
                self.b
                    .push("CASE WHEN (")
                    .qualified(TARGET, "xmax")
                    .push(") = (0) THEN (");
                self.bool_exp(&check.insert, TARGET)?;
                self.b.push(") ELSE (");
                self.bool_exp(update, TARGET)?;
                self.b.push(") END");
            }
            _ => self.bool_exp(&check.insert, TARGET)?,
        }
        Ok(())
    }

    fn on_conflict(&mut self, oc: &OnConflict) -> Result<(), Error> {
        self.b
            .push(" ON CONFLICT ON CONSTRAINT ")
            .ident(&oc.constraint);
        if oc.update_columns.is_empty() {
            self.b.push(" DO NOTHING");
            return Ok(());
        }
        self.b.push(" DO UPDATE SET ");
        let mut first = true;
        for c in &oc.update_columns {
            if oc.presets.iter().any(|(p, _)| p == c) {
                continue;
            }
            if !first {
                self.b.push(", ");
            }
            first = false;
            self.b.ident(c).push(" = EXCLUDED.").ident(c);
        }
        for (c, v) in &oc.presets {
            if !first {
                self.b.push(", ");
            }
            first = false;
            self.b.ident(c).push(" = ");
            self.value(v)?;
        }
        if let Some(filter) = &oc.filter {
            self.b.push(" WHERE ");
            self.bool_exp(filter, TARGET)?;
        }
        Ok(())
    }

    fn update_exp(&mut self, column: &str, exp: &UpdateExp) -> Result<(), Error> {
        let (op, v, column_first) = match exp {
            UpdateExp::Set(v) => return self.value(v),
            UpdateExp::Inc(v) => (" + ", v, true),
            UpdateExp::Append(v) => (" || ", v, true),
            UpdateExp::Prepend(v) => (" || ", v, false),
            UpdateExp::DeleteKey(v) | UpdateExp::DeleteElem(v) => (" - ", v, true),
            UpdateExp::DeleteAtPath(v) => (" #- ", v, true),
        };
        self.b.push("(");
        if column_first {
            self.b.qualified(TARGET, column).push(op);
            self.value(v)?;
        } else {
            self.value(v)?;
            self.b.push(op).qualified(TARGET, column);
        }
        self.b.push(")");
        Ok(())
    }
}

/// A Postgres `text[]` literal with `NULL` elements: `{"a",NULL}`.
fn text_array_literal(items: &[Option<&str>]) -> String {
    let mut out = String::from("{");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(s) => {
                out.push('"');
                for c in s.chars() {
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
        }
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hge_core::QualifiedTable;
    use hge_ir::{BoolExp, CompareOp, Field};

    fn table() -> MutationTable {
        MutationTable {
            name: QualifiedTable::new("public", "author"),
            columns: vec![
                ("id".into(), ColumnType::builtin("int4")),
                ("name".into(), ColumnType::builtin("text")),
            ],
        }
    }

    fn output() -> MutationOutput {
        MutationOutput::Multi(vec![
            ("affected_rows".into(), MutationResponseField::AffectedRows),
            (
                "returning".into(),
                MutationResponseField::Returning(vec![(
                    "id".into(),
                    Field::Column(hge_ir::ColumnField {
                        column: "id".into(),
                        ty: ColumnType::builtin("int4"),
                        stringify: false,
                        json_path: None,
                    }),
                )]),
            ),
        ])
    }

    #[test]
    fn insert_with_defaults_and_upsert() {
        let into = InsertInto {
            table: table(),
            objects: vec![],
            on_conflict: Some(OnConflict {
                constraint: "author_pkey".into(),
                update_columns: vec!["name".into()],
                presets: vec![],
                filter: None,
            }),
            check: None,
            path: "$".into(),
        };
        let a = Value::literal("A", ColumnType::builtin("text"));
        let b = Value::literal("7", ColumnType::builtin("int4"));
        let rows = vec![vec![("name", &a)], vec![("id", &b)]];
        let out = output();
        let q = insert(&into, &rows, Statement::Output(&out), &Session::admin()).unwrap();
        assert!(q.sql.contains(
            r#"INSERT INTO "public"."author" AS "_hge_target" ("name", "id") VALUES (($1)::"pg_catalog"."text", DEFAULT), (DEFAULT, ($2)::"pg_catalog"."int4") ON CONFLICT ON CONSTRAINT "author_pkey" DO UPDATE SET "name" = EXCLUDED."name" RETURNING *)"#
        ), "{}", q.sql);
        assert!(q.sql.ends_with(", 'true'"), "{}", q.sql);
    }

    #[test]
    fn update_operators() {
        let u = Update {
            table: table(),
            set: vec![(
                "id".into(),
                UpdateExp::Inc(Value::literal("1", ColumnType::builtin("int4"))),
            )],
            filter: BoolExp::Compare {
                column: "id".into(),
                ty: ColumnType::builtin("int4"),
                op: CompareOp::Eq(Value::literal("1", ColumnType::builtin("int4"))),
            },
            check: Some(BoolExp::always_true()),
            output: output(),
        };
        let q = update(&u, Statement::Fetch, &Session::admin()).unwrap();
        assert!(
            q.sql
                .contains(r#"SET "id" = ("_hge_target"."id" + ($1)::"pg_catalog"."int4") WHERE"#),
            "{}",
            q.sql
        );
        assert!(q.sql.contains(r#"bool_and("_hge_check")"#), "{}", q.sql);
    }

    #[test]
    fn array_literal_with_nulls() {
        assert_eq!(
            text_array_literal(&[Some("a\"b"), None, Some("c\\")]),
            r#"{"a\"b",NULL,"c\\"}"#
        );
    }
}
