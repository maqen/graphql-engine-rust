//! A Postgres source: pool + executor.

use std::sync::Arc;

use deadpool_postgres::{Object, Pool};
use hge_core::{
    Code, EnumTableRow, Error, NativeQueryCheck, QualifiedFunction, QualifiedTable, Session,
    SourceCatalog,
};
use hge_ir::{BoxFuture, MultiplexedQuery, MultiplexedRow, Mutation, QueryRoot, SourceExecutor};
use serde_json::json;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::introspect;
use crate::param::TextParam;
use crate::pool::{IsolationLevel, PoolConfig, connect_pool};
use crate::translate::{translate_multiplexed, translate_query};

/// How long reading one enum table's rows may take.
pub const ENUM_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct PgSource {
    pub name: Arc<str>,
    pool: Pool,
    use_prepared_statements: bool,
    isolation_level: IsolationLevel,
    /// For cancelling queries whose request went away.
    tls: MakeRustlsConnect,
}

impl PgSource {
    pub fn connect(name: &str, url: &str, config: &PoolConfig) -> Result<Self, Error> {
        let (pool, tls) = connect_pool(url, config)?;
        Ok(PgSource {
            name: Arc::from(name),
            pool,
            tls,
            use_prepared_statements: config.use_prepared_statements,
            isolation_level: config.isolation_level,
        })
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    pub(crate) fn tls(&self) -> &MakeRustlsConnect {
        &self.tls
    }

    pub(crate) fn use_prepared_statements(&self) -> bool {
        self.use_prepared_statements
    }

    /// The isolation level of mutation transactions (source `isolation_level`).
    pub fn isolation_level(&self) -> IsolationLevel {
        self.isolation_level
    }

    pub(crate) async fn client(&self) -> Result<Object, Error> {
        self.pool.get().await.map_err(|e| {
            Error::new(
                Code::PostgresError,
                format!("connection error on source \"{}\": {e}", self.name),
            )
        })
    }

    /// Introspect the given tables and functions (and all enum types), resolve the given type
    /// names and validate the native queries.
    pub async fn introspect(
        &self,
        tables: &[QualifiedTable],
        functions: &[QualifiedFunction],
        types: &[String],
        native_queries: &[NativeQueryCheck],
    ) -> Result<SourceCatalog, Error> {
        let param = introspect::tables_param(tables);
        let tables_json = self
            .query_text(introspect::TABLES_SQL, &[Some(&param)])
            .await?
            .unwrap_or_else(|| "[]".to_owned());
        let enums_json = self
            .query_text(introspect::ENUMS_SQL, &[])
            .await?
            .unwrap_or_else(|| "[]".to_owned());
        let mut catalog = introspect::parse_catalog(&tables_json, &enums_json)?;
        if !functions.is_empty() {
            let param = introspect::tables_param(functions);
            let functions_json = self
                .query_text(introspect::FUNCTIONS_SQL, &[Some(&param)])
                .await?
                .unwrap_or_else(|| "[]".to_owned());
            catalog.functions = introspect::parse_functions(&functions_json)?;
        }
        if !types.is_empty() || !native_queries.is_empty() {
            // Never return a connection to the pool inside the validation transaction: if
            // this future is dropped or the transaction did not end cleanly, detach it.
            let mut guard = CancelOnDrop::new(self.client().await?, &self.tls);
            (catalog.types, catalog.native_queries) =
                crate::native::validate(guard.client(), types, native_queries).await?;
            guard.disarm();
        }
        Ok(catalog)
    }

    /// The rows of an enum table: its primary key column and its comment column, if any
    /// (V2's `fetchEnumValuesFromDb`), or V2's error text. The read is bounded by
    /// [`ENUM_READ_TIMEOUT`] so a slow source cannot stall a metadata change.
    pub async fn enum_table_rows(
        &self,
        table: &QualifiedTable,
        key: &str,
        comment: Option<&str>,
    ) -> Result<Vec<EnumTableRow>, String> {
        let sql = introspect::enum_rows_sql(table, key, comment);
        match tokio::time::timeout(ENUM_READ_TIMEOUT, self.read_enum_rows(&sql)).await {
            Ok(read) => read,
            Err(_) => Err(introspect::enum_read_error(None)),
        }
    }

    async fn read_enum_rows(&self, sql: &str) -> Result<Vec<EnumTableRow>, String> {
        let error = |e: tokio_postgres::Error| {
            introspect::enum_read_error(e.as_db_error().map(|d| (d.code().code(), d.message())))
        };
        let mut client = self
            .client()
            .await
            .map_err(|_| introspect::enum_read_error(None))?;
        let tx = client.transaction().await.map_err(error)?;
        tx.batch_execute(&format!(
            "SET LOCAL statement_timeout = {}",
            ENUM_READ_TIMEOUT.as_millis()
        ))
        .await
        .map_err(error)?;
        let rows = tx.query(sql, &[]).await.map_err(error)?;
        tx.commit().await.map_err(error)?;
        Ok(introspect::enum_rows(
            rows.iter()
                .map(|r| vec![r.get::<_, Option<String>>(0), r.get(1)])
                .collect(),
        ))
    }

    /// Run a statement returning (at most) one row with one text column.
    pub async fn query_text(
        &self,
        sql: &str,
        params: &[Option<&str>],
    ) -> Result<Option<String>, Error> {
        let row = self.query_row(sql, params).await?;
        Ok(row.and_then(|mut r| r.pop().flatten()))
    }

    /// Run a statement and return the first row's text columns.
    pub async fn query_row(
        &self,
        sql: &str,
        params: &[Option<&str>],
    ) -> Result<Option<Vec<Option<String>>>, Error> {
        Ok(self.query_rows(sql, params).await?.into_iter().next())
    }

    /// Run a statement and return every row's text columns.
    pub async fn query_rows(
        &self,
        sql: &str,
        params: &[Option<&str>],
    ) -> Result<Vec<Vec<Option<String>>>, Error> {
        let client = self.client().await?;
        let params: Vec<TextParam<'_>> = params.iter().map(|p| TextParam(*p)).collect();
        let refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let prepared = self.use_prepared_statements;
        // If this future is dropped mid-query (client disconnected), cancel the query in
        // Postgres instead of letting it run to completion.
        let mut guard = CancelOnDrop::new(client, &self.tls);
        let client = guard.client();
        let result = if prepared {
            // The cache never evicts; start over rather than grow without bound.
            if client.statement_cache.size() >= MAX_CACHED_STATEMENTS {
                client.statement_cache.clear();
            }
            match client.prepare_cached(sql).await {
                Ok(stmt) => client.query(&stmt, &refs).await,
                Err(e) => Err(e),
            }
        } else {
            let typed: Vec<(&(dyn ToSql + Sync), Type)> =
                refs.iter().map(|p| (*p, Type::TEXT)).collect();
            client.query_typed(sql, &typed).await
        };
        // The query finished (successfully or not): the connection is reusable.
        guard.disarm();
        let rows = result.map_err(|e| pg_error_with(e, sql, prepared))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut cols = Vec::with_capacity(row.len());
            for i in 0..row.len() {
                cols.push(
                    row.try_get::<_, Option<String>>(i)
                        .map_err(|e| pg_error_with(e, sql, prepared))?,
                );
            }
            out.push(cols);
        }
        Ok(out)
    }

    /// Execute arbitrary SQL (several statements allowed), discarding results.
    pub async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        let client = self.client().await?;
        client
            .batch_execute(sql)
            .await
            .map_err(|e| pg_error(e, sql))
    }
}

impl SourceExecutor for PgSource {
    fn execute_query<'a>(
        &'a self,
        fields: &'a [&'a QueryRoot],
        session: &'a Session,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(async move {
            let q = translate_query(fields, session)?;
            let params: Vec<Option<&str>> = q.params.iter().map(|p| p.as_deref()).collect();
            let row = self
                .query_row(&q.sql, &params)
                .await?
                .ok_or_else(|| Error::unexpected("query returned no rows"))?;
            Ok(row
                .into_iter()
                .map(|v| v.unwrap_or_else(|| "null".to_owned()))
                .collect())
        })
    }

    fn execute_mutations<'a>(
        &'a self,
        fields: &'a [&'a Mutation],
        session: &'a Session,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(crate::mutation::execute(self, fields, session))
    }

    fn multiplexed_query(
        &self,
        fields: &[&QueryRoot],
        session: &Session,
    ) -> Result<MultiplexedQuery, Error> {
        translate_multiplexed(fields, session)
    }

    fn execute_multiplexed<'a>(
        &'a self,
        sql: &'a str,
        cohorts: &'a str,
    ) -> BoxFuture<'a, Result<Vec<MultiplexedRow>, Error>> {
        Box::pin(async move {
            let rows = self.query_rows(sql, &[Some(cohorts)]).await?;
            rows.into_iter()
                .map(|mut cols| {
                    // The first column is the 1-based position of the cohort.
                    let index = cols
                        .first_mut()
                        .and_then(Option::take)
                        .and_then(|i| i.parse::<usize>().ok())
                        .and_then(|i| i.checked_sub(1))
                        .ok_or_else(|| {
                            Error::unexpected("multiplexed query without cohort index")
                        })?;
                    cols.remove(0);
                    Ok(MultiplexedRow {
                        index,
                        columns: cols,
                    })
                })
                .collect()
        })
    }
}

/// Map a driver error to a V2-style error (`data-exception`, `constraint-violation`, …).
/// Holds a pooled client during a query. Dropped before [`CancelOnDrop::disarm`] (the
/// request future was dropped), it detaches the connection from the pool, so nothing else
/// can be running on it, and cancels the query on the server.
pub(crate) struct CancelOnDrop<'a> {
    client: Option<Object>,
    tls: &'a MakeRustlsConnect,
}

impl<'a> CancelOnDrop<'a> {
    pub(crate) fn new(client: Object, tls: &'a MakeRustlsConnect) -> Self {
        CancelOnDrop {
            client: Some(client),
            tls,
        }
    }

    pub(crate) fn client(&self) -> &Object {
        self.client.as_ref().expect("not disarmed")
    }

    pub(crate) fn disarm(&mut self) {
        // Dropping the object returns it to the pool.
        self.client = None;
    }
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let token = client.cancel_token();
            let detached = Object::take(client);
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let tls = self.tls.clone();
                rt.spawn(async move {
                    let _ = token.cancel_query(tls).await;
                    drop(detached);
                });
            }
        }
    }
}

/// Per-connection prepared statements kept before the cache is reset.
pub(crate) const MAX_CACHED_STATEMENTS: usize = 1024;

pub fn pg_error(e: tokio_postgres::Error, sql: &str) -> Error {
    pg_error_with(e, sql, false)
}

/// [`pg_error`] for a statement that was (or was not) prepared.
pub fn pg_error_with(e: tokio_postgres::Error, sql: &str, prepared: bool) -> Error {
    match e.as_db_error() {
        Some(db) => {
            let state = db.code().code();
            let (code, message) = classify(state, db.message());
            // Errors shown to clients carry no `internal` details (as in V2).
            if code != Code::Unexpected {
                return Error::new(code, message);
            }
            Error::new(code, message).with_internal(json!({
                "statement": sql,
                "prepared": prepared,
                "error": {
                    "exec_status": "FatalError",
                    "hint": db.hint(),
                    "message": db.message(),
                    "status_code": state,
                    "description": db.detail(),
                },
            }))
        }
        None => Error::new(Code::PostgresError, "connection error")
            .with_internal(json!({ "statement": sql, "error": e.to_string() })),
    }
}

/// V2's mapping of a SQLSTATE to an error code and message (`dmlTxErrorHandler`): integrity
/// and data errors are shown to clients, everything else is `database query error`.
fn classify(state: &str, message: &str) -> (Code, String) {
    let prefixed = |p: &str| format!("{p}{message}");
    match state {
        "23001" => (
            Code::ConstraintViolation,
            prefixed("Can not delete or update due to data being referred. "),
        ),
        "23502" => (Code::ConstraintViolation, prefixed("Not-NULL violation. ")),
        "23503" => (
            Code::ConstraintViolation,
            prefixed("Foreign key violation. "),
        ),
        "23505" => (
            Code::ConstraintViolation,
            prefixed("Uniqueness violation. "),
        ),
        "23514" => (
            Code::PermissionError,
            prefixed("Check constraint violation. "),
        ),
        "23P01" => (Code::ConstraintViolation, prefixed("Exclusion violation. ")),
        "22025" => (Code::BadRequest, message.to_owned()),
        "42704" => (Code::ConstraintError, message.to_owned()),
        "42P10" => (
            Code::ConstraintError,
            "there is no unique or exclusion constraint on target column(s)".to_owned(),
        ),
        _ if state.starts_with("23") => (Code::ConstraintViolation, message.to_owned()),
        _ if state.starts_with("22") => (Code::DataException, message.to_owned()),
        _ => (Code::Unexpected, "database query error".to_owned()),
    }
}
