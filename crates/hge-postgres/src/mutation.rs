//! Executing mutations: one transaction per call, with `hasura.tracecontext` and `hasura.user`
//! set for triggers.
//!
//! ```sql
//! BEGIN ISOLATION LEVEL READ COMMITTED READ WRITE;
//! SET LOCAL "hasura.tracecontext" = '{"sampling_state":"1","span_id":"…","trace_id":"…"}';
//! SET LOCAL "hasura.user" = '{"x-hasura-role":"admin"}';
//! <statements of every mutation root field, in order>
//! COMMIT;  -- ROLLBACK on any error
//! ```

use deadpool_postgres::ClientWrapper;
use hge_core::{Code, Error, Session};
use hge_ir::{
    Delete, Insert, InsertInto, InsertObject, Mutation, MutationOutput, MutationResponseField,
    MutationTable, Update, Value,
};
use serde_json::Value as Json;
use tokio_postgres::types::{ToSql, Type};

use crate::param::TextParam;
use crate::source::{CancelOnDrop, PgSource, pg_error_with};
use crate::sql::SqlBuilder;
use crate::translate::mutation::{self as sql, Row, Statement};

/// The trace context event triggers record with the events this transaction creates. A fresh
/// trace per mutation request, like V2 without an incoming trace header.
fn trace_context() -> Json {
    // Sorted keys, as V2 writes them.
    serde_json::json!({
        "sampling_state": "1",
        "span_id": random_hex(8),
        "trace_id": random_hex(16),
    })
}

fn random_hex(bytes: usize) -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..bytes)
        .map(|_| format!("{:02x}", rng.random::<u8>()))
        .collect()
}

const CHECK_FAILED: &str = "check constraint of an insert/update permission has failed";

/// Rows returned by a fetch statement: values as text, in table column order.
type Rows = Vec<Vec<Option<String>>>;

pub(crate) async fn execute(
    source: &PgSource,
    fields: &[&Mutation],
    session: &Session,
) -> Result<Vec<String>, Error> {
    let client = source.client().await?;
    // Dropped mid-transaction (the request went away), the connection is cancelled and
    // detached from the pool, so an open transaction is never reused.
    let mut guard = CancelOnDrop::new(client, source.tls());
    let client: &ClientWrapper = guard.client();
    let tx = Tx {
        client,
        session,
        prepared: source.use_prepared_statements(),
    };
    // BEGIN goes alone in its own message: it is built from fixed words only, so it can't
    // fail to parse and leave the statements pipelined behind it running outside a
    // transaction. The settings follow in a second message; if they fail, the transaction is
    // open but aborted and the first mutation fails with it.
    let begin = format!(
        "BEGIN ISOLATION LEVEL {} READ WRITE",
        source.isolation_level().sql()
    );
    let settings = format!(
        "SET LOCAL \"hasura.tracecontext\" = {}; SET LOCAL \"hasura.user\" = {}",
        quote_literal(&trace_context().to_string()),
        quote_literal(&session.variables.to_sorted_json_string()),
    );
    // The first mutation is sent right behind those two messages instead of after their
    // replies, saving a round trip.
    let (first, rest) = fields
        .split_first()
        .map_or((None, &[][..]), |(f, r)| (Some(f), r));
    let first = match pipelined(client, [&begin, &settings], async {
        match first {
            Some(m) => Some(tx.mutation(m).await),
            None => None,
        }
    })
    .await
    {
        Ok(first) => first,
        // The guard is still armed, so the connection is detached instead of reused.
        Err(e) => return Err(tx_error(e)),
    };

    let mut results = Vec::with_capacity(fields.len());
    let mut failure = None;
    match first {
        Some(Ok(json)) => results.push(json),
        Some(Err(e)) => failure = Some(e),
        None => {}
    }
    if failure.is_none() {
        for m in rest {
            match tx.mutation(m).await {
                Ok(json) => results.push(json),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
    }
    let end = if failure.is_some() {
        client.batch_execute("ROLLBACK").await
    } else {
        client.batch_execute("COMMIT").await
    };
    guard.disarm();
    match failure {
        Some(e) => Err(e),
        None => {
            end.map_err(tx_error)?;
            Ok(results)
        }
    }
}

/// Send the `begin` messages and then whatever `first` sends on the same connection without
/// waiting for replies in between (tokio-postgres keeps requests in the order they are first
/// polled, and `first` is polled last). `first`'s output is returned only if every `begin`
/// message succeeded.
async fn pipelined<T>(
    client: &ClientWrapper,
    [begin, settings]: [&str; 2],
    first: impl Future<Output = T>,
) -> Result<T, tokio_postgres::Error> {
    let (begun, set, first) = futures::join!(
        client.batch_execute(begin),
        client.batch_execute(settings),
        first
    );
    begun.and(set).map(|()| first)
}

/// `BEGIN` / `COMMIT` failures (including deferred constraints checked at commit).
fn tx_error(e: tokio_postgres::Error) -> Error {
    let mut err = Error::new(Code::PostgresError, "postgres tx error");
    if let Some(db) = e.as_db_error() {
        err = err.with_internal(serde_json::json!({
            "error": {
                "exec_status": "FatalError",
                "hint": db.hint(),
                "message": db.message(),
                "status_code": db.code().code(),
                "description": db.detail(),
            },
        }));
    }
    err
}

/// A string literal (standard_conforming_strings).
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

struct Tx<'a> {
    client: &'a ClientWrapper,
    session: &'a Session,
    prepared: bool,
}

impl Tx<'_> {
    async fn mutation(&self, m: &Mutation) -> Result<String, Error> {
        match m {
            Mutation::Insert(i) => self.insert(i).await,
            Mutation::Update(u) => self.update(u).await,
            Mutation::UpdateMany(batches) => {
                if batches.iter().all(|u| u.set.is_empty()) {
                    return Ok("[]".to_owned());
                }
                let mut out = String::from("[");
                for (i, u) in batches.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&self.update(u).await?);
                }
                out.push(']');
                Ok(out)
            }
            Mutation::Delete(d) => self.delete(d).await,
            Mutation::Function(query) => {
                let q = crate::translate::translate_query(&[&**query], self.session)?;
                Ok(first_json(self.run(&q, "$").await?))
            }
        }
    }

    async fn insert(&self, ins: &Insert) -> Result<String, Error> {
        let into = &ins.into;
        if into.objects.is_empty() {
            return Ok(empty_response(&ins.output));
        }
        if !into.is_nested() && !ins.output.has_relationships() {
            let rows: Vec<Row<'_>> = into.objects.iter().map(object_row).collect();
            let q = sql::insert(into, &rows, Statement::Output(&ins.output), self.session)?;
            let row = self.run(&q, &into.path).await?;
            check(&row, 1, &into.path)?;
            return Ok(first_json(row));
        }
        let (affected, rows) = self.insert_into(into, &[]).await?;
        let q = sql::output_from_rows(&into.table, &rows, affected, &ins.output, self.session)?;
        Ok(first_json(self.run(&q, "$").await?))
    }

    /// Insert `into`'s objects, each with the extra column values `parent` (determined by a
    /// parent row). Returns the rows inserted into all tables and this table's rows.
    async fn insert_into(
        &self,
        into: &InsertInto,
        parent: &[(String, Value)],
    ) -> Result<(u64, Rows), Error> {
        if into.objects.is_empty() {
            return Ok((0, vec![]));
        }
        if !into.is_nested() {
            let rows: Vec<Row<'_>> = into
                .objects
                .iter()
                .map(|o| {
                    let mut row = object_row(o);
                    row.extend(parent.iter().map(|(c, v)| (c.as_str(), v)));
                    row
                })
                .collect();
            return self.fetch_insert(into, &rows, &into.path).await;
        }
        let mut affected = 0;
        let mut all_rows = vec![];
        for obj in &into.objects {
            let (n, rows) = Box::pin(self.insert_object(into, obj, parent)).await?;
            affected += n;
            all_rows.extend(rows);
        }
        Ok((affected, all_rows))
    }

    /// One object with its relationships: object relationships first, then the row, then
    /// the relationships that need the row's columns.
    async fn insert_object(
        &self,
        into: &InsertInto,
        obj: &InsertObject,
        parent: &[(String, Value)],
    ) -> Result<(u64, Rows), Error> {
        let mut affected = 0;
        let mut values: Vec<(String, Value)> = obj.columns.clone();
        values.extend(parent.iter().cloned());
        for rel in &obj.before {
            let (n, rows) = Box::pin(self.insert_into(&rel.insert, &[])).await?;
            affected += n;
            let Some(row) = rows.first() else {
                return Err(Error::new(
                    Code::NotSupported,
                    format!(
                        "cannot proceed to insert object relation \"{}\" since insert to table \"{}\" affects zero rows",
                        rel.name,
                        display(&rel.insert.table)
                    ),
                )
                .with_path(rel.path.clone()));
            };
            for (local, remote) in &rel.mapping {
                values.push((local.clone(), column_value(&rel.insert.table, row, remote)?));
            }
        }

        let row: Row<'_> = values.iter().map(|(c, v)| (c.as_str(), v)).collect();
        let (n, rows) = self.fetch_insert(into, &[row], &obj.path).await?;
        affected += n;
        if rows.len() > 1 {
            return Err(Error::unexpected("more than one row returned").with_path(obj.path.clone()));
        }

        for rel in &obj.after {
            let Some(row) = rows.first() else {
                return Err(Error::new(
                    Code::NotSupported,
                    format!(
                        "cannot proceed to insert array relations since insert to table \"{}\" affects zero rows",
                        display(&into.table)
                    ),
                )
                .with_path(obj.path.clone()));
            };
            let mut child = Vec::with_capacity(rel.mapping.len());
            for (local, remote) in &rel.mapping {
                let v = column_value(&into.table, row, local)?;
                let ty = rel
                    .insert
                    .table
                    .columns
                    .iter()
                    .find(|(c, _)| c == remote)
                    .map(|(_, t)| t.clone())
                    .unwrap_or_else(|| v.ty().clone());
                let Value::Literal { text, .. } = v else {
                    unreachable!("column_value returns literals")
                };
                child.push((remote.clone(), Value::Literal { text, ty }));
            }
            let (n, _) = Box::pin(self.insert_into(&rel.insert, &child)).await?;
            affected += n;
        }
        Ok((affected, rows))
    }

    async fn fetch_insert(
        &self,
        into: &InsertInto,
        rows: &[Row<'_>],
        path: &str,
    ) -> Result<(u64, Rows), Error> {
        let q = sql::insert(into, rows, Statement::Fetch, self.session)?;
        let row = self.run(&q, path).await?;
        check(&row, 2, path)?;
        parse_fetch(&row)
    }

    async fn update(&self, u: &Update) -> Result<String, Error> {
        if u.set.is_empty() {
            return Ok(empty_response(&u.output));
        }
        if !u.output.has_relationships() {
            let q = sql::update(u, Statement::Output(&u.output), self.session)?;
            let row = self.run(&q, "$").await?;
            check(&row, 1, "$")?;
            return Ok(first_json(row));
        }
        let q = sql::update(u, Statement::Fetch, self.session)?;
        let row = self.run(&q, "$").await?;
        check(&row, 2, "$")?;
        let (affected, rows) = parse_fetch(&row)?;
        self.output(&u.table, &rows, affected, &u.output).await
    }

    async fn delete(&self, d: &Delete) -> Result<String, Error> {
        if !d.output.has_relationships() {
            let q = sql::delete(d, Statement::Output(&d.output), self.session)?;
            return Ok(first_json(self.run(&q, "$").await?));
        }
        let q = sql::delete(d, Statement::Fetch, self.session)?;
        let (affected, rows) = parse_fetch(&self.run(&q, "$").await?)?;
        self.output(&d.table, &rows, affected, &d.output).await
    }

    async fn output(
        &self,
        table: &MutationTable,
        rows: &Rows,
        affected: u64,
        output: &MutationOutput,
    ) -> Result<String, Error> {
        let q = sql::output_from_rows(table, rows, affected, output, self.session)?;
        Ok(first_json(self.run(&q, "$").await?))
    }

    /// Run a statement returning one row of text columns. Database errors are reported at
    /// `path`.
    async fn run(&self, q: &SqlBuilder, path: &str) -> Result<Vec<Option<String>>, Error> {
        let params: Vec<TextParam<'_>> = q.params.iter().map(|p| TextParam(p.as_deref())).collect();
        let refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let result = if self.prepared {
            if self.client.statement_cache.size() >= crate::source::MAX_CACHED_STATEMENTS {
                self.client.statement_cache.clear();
            }
            match self.client.prepare_cached(&q.sql).await {
                Ok(stmt) => self.client.query(&stmt, &refs).await,
                Err(e) => Err(e),
            }
        } else {
            let typed: Vec<(&(dyn ToSql + Sync), Type)> =
                refs.iter().map(|p| (*p, Type::TEXT)).collect();
            self.client.query_typed(&q.sql, &typed).await
        };
        let err = |e| pg_error_with(e, &q.sql, self.prepared).with_path(path.to_owned());
        let rows = result.map_err(err)?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| Error::unexpected("mutation returned no rows"))?;
        (0..row.len())
            .map(|i| row.try_get::<_, Option<String>>(i).map_err(err))
            .collect()
    }
}

/// The values a flat object inserts.
fn object_row(o: &InsertObject) -> Row<'_> {
    o.columns.iter().map(|(c, v)| (c.as_str(), v)).collect()
}

/// The permission check result is the statement's column `index`.
fn check(row: &[Option<String>], index: usize, path: &str) -> Result<(), Error> {
    match row.get(index).and_then(|v| v.as_deref()) {
        Some("true") | None => Ok(()),
        Some(_) => Err(Error::new(Code::PermissionError, CHECK_FAILED).with_path(path.to_owned())),
    }
}

fn first_json(row: Vec<Option<String>>) -> String {
    row.into_iter()
        .next()
        .flatten()
        .unwrap_or_else(|| "null".to_owned())
}

/// `(affected rows, rows)` from a fetch statement.
fn parse_fetch(row: &[Option<String>]) -> Result<(u64, Rows), Error> {
    let affected = row
        .first()
        .and_then(|v| v.as_deref())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| Error::unexpected("mutation returned no row count"))?;
    let rows = row
        .get(1)
        .and_then(|v| v.as_deref())
        .map(serde_json::from_str::<Rows>)
        .transpose()
        .map_err(|e| Error::unexpected(format!("could not parse mutation rows: {e}")))?
        .unwrap_or_default();
    Ok((affected, rows))
}

/// The value of `column` in a fetched row, typed as the column.
fn column_value(
    table: &MutationTable,
    row: &[Option<String>],
    column: &str,
) -> Result<Value, Error> {
    let (i, (_, ty)) = table
        .columns
        .iter()
        .enumerate()
        .find(|(_, (c, _))| c == column)
        .ok_or_else(|| Error::unexpected(format!("column \"{column}\" not found")))?;
    Ok(Value::Literal {
        text: row.get(i).cloned().flatten(),
        ty: ty.clone(),
    })
}

fn display(table: &MutationTable) -> String {
    let t = &table.name;
    if t.schema == "public" {
        t.name.clone()
    } else {
        format!("{}.{}", t.schema, t.name)
    }
}

/// The response of a mutation that executes nothing (`objects: []`, an update with nothing
/// to set). V2 returns `{}` (not `null`) for single-row outputs here.
fn empty_response(output: &MutationOutput) -> String {
    match output {
        MutationOutput::Single(_) => "{}".to_owned(),
        MutationOutput::Multi(fields) => {
            let obj: serde_json::Map<String, Json> = fields
                .iter()
                .map(|(k, f)| {
                    let v = match f {
                        MutationResponseField::AffectedRows => Json::from(0),
                        MutationResponseField::Returning(_) => Json::Array(vec![]),
                        MutationResponseField::Typename(t) => Json::String(t.clone()),
                    };
                    (k.clone(), v)
                })
                .collect();
            Json::Object(obj).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;

    /// Failing BEGIN settings must keep the pipelined first statement from taking effect.
    #[tokio::test]
    async fn pipelined_statement_does_not_run_when_begin_fails() {
        let Some(db) = hge_testdb::TestDb::create("pipelined_begin").await else {
            return;
        };
        let source = PgSource::connect("default", db.url(), &PoolConfig::default()).unwrap();
        source
            .batch_execute("CREATE TABLE t (id int)")
            .await
            .unwrap();
        let client = source.client().await.unwrap();
        // A settings message that does not even parse: the transaction BEGIN opened is
        // aborted, so the insert behind it fails instead of autocommitting.
        let insert = client.execute("INSERT INTO t VALUES (1)", &[]);
        let out = pipelined(&client, ["BEGIN", "SET LOCAL x = 'unterminated"], insert).await;
        assert!(out.is_err());
        client.batch_execute("ROLLBACK").await.unwrap();
        // A setting that parses but fails.
        let insert = client.execute("INSERT INTO t VALUES (1)", &[]);
        let bad = "SET LOCAL statement_timeout = 'not a duration'";
        assert!(pipelined(&client, ["BEGIN", bad], insert).await.is_err());
        client.batch_execute("ROLLBACK").await.unwrap();

        let ok = pipelined(
            &client,
            ["BEGIN", "SET LOCAL statement_timeout = '5s'"],
            client.execute("INSERT INTO t VALUES (2)", &[]),
        )
        .await
        .unwrap();
        assert_eq!(ok.unwrap(), 1);
        client.batch_execute("COMMIT").await.unwrap();
        let rows = client.query("SELECT id FROM t", &[]).await.unwrap();
        let ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
        assert_eq!(ids, vec![2]);
    }
}
