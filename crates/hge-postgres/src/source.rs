//! A Postgres source: pool + executor.

use std::sync::Arc;

use deadpool_postgres::{Object, Pool};
use hge_core::{Code, Error, QualifiedTable, Session, SourceCatalog};
use hge_ir::{BoxFuture, QueryRoot, SourceExecutor};
use serde_json::json;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::introspect;
use crate::param::TextParam;
use crate::pool::{PoolConfig, connect_pool};
use crate::translate::translate_query;

pub struct PgSource {
    pub name: Arc<str>,
    pool: Pool,
    use_prepared_statements: bool,
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
        })
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    async fn client(&self) -> Result<Object, Error> {
        self.pool.get().await.map_err(|e| {
            Error::new(
                Code::PostgresError,
                format!("connection error on source \"{}\": {e}", self.name),
            )
        })
    }

    /// Introspect the given tables (and all enum types).
    pub async fn introspect(&self, tables: &[QualifiedTable]) -> Result<SourceCatalog, Error> {
        let param = introspect::tables_param(tables);
        let tables_json = self
            .query_text(introspect::TABLES_SQL, &[Some(&param)])
            .await?
            .unwrap_or_else(|| "[]".to_owned());
        let enums_json = self
            .query_text(introspect::ENUMS_SQL, &[])
            .await?
            .unwrap_or_else(|| "[]".to_owned());
        introspect::parse_catalog(&tables_json, &enums_json)
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
        let client = self.client().await?;
        let params: Vec<TextParam<'_>> = params.iter().map(|p| TextParam(*p)).collect();
        let refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let prepared = self.use_prepared_statements;
        // If this future is dropped mid-query (client disconnected), cancel the query in
        // Postgres instead of letting it run to completion.
        let mut guard = CancelOnDrop {
            client: Some(client),
            tls: &self.tls,
        };
        let client = guard.client.as_ref().expect("set above");
        let rows = if prepared {
            // The cache never evicts; start over rather than grow without bound.
            if client.statement_cache.size() >= MAX_CACHED_STATEMENTS {
                client.statement_cache.clear();
            }
            let stmt = client
                .prepare_cached(sql)
                .await
                .map_err(|e| pg_error_with(e, sql, prepared))?;
            client.query(&stmt, &refs).await
        } else {
            let typed: Vec<(&(dyn ToSql + Sync), Type)> =
                refs.iter().map(|p| (*p, Type::TEXT)).collect();
            client.query_typed(sql, &typed).await
        }
        .map_err(|e| pg_error_with(e, sql, prepared))?;
        guard.disarm();
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(row.len());
        for i in 0..row.len() {
            out.push(
                row.try_get::<_, Option<String>>(i)
                    .map_err(|e| pg_error_with(e, sql, prepared))?,
            );
        }
        Ok(Some(out))
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
}

/// Map a driver error to a V2-style error (`data-exception`, `constraint-violation`, …).
/// Holds a pooled client during a query. Dropped before [`CancelOnDrop::disarm`] (the
/// request future was dropped), it detaches the connection from the pool, so nothing else
/// can be running on it, and cancels the query on the server.
struct CancelOnDrop<'a> {
    client: Option<Object>,
    tls: &'a MakeRustlsConnect,
}

impl CancelOnDrop<'_> {
    fn disarm(&mut self) {
        // Dropping the object returns it to the pool.
        self.client = None;
    }
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let token = client.cancel_token();
            let tls = self.tls.clone();
            let detached = Object::take(client);
            rt.spawn(async move {
                let _ = token.cancel_query(tls).await;
                drop(detached);
            });
        }
    }
}

/// Per-connection prepared statements kept before the cache is reset.
const MAX_CACHED_STATEMENTS: usize = 1024;

pub fn pg_error(e: tokio_postgres::Error, sql: &str) -> Error {
    pg_error_with(e, sql, false)
}

/// [`pg_error`] for a statement that was (or was not) prepared.
pub fn pg_error_with(e: tokio_postgres::Error, sql: &str, prepared: bool) -> Error {
    match e.as_db_error() {
        Some(db) => {
            let state = db.code().code();
            let code = match &state[..2] {
                "22" => Code::DataException,
                "23" => Code::ConstraintViolation,
                "42" if state == "42501" => Code::PermissionDenied,
                _ => Code::PostgresError,
            };
            let message = if code == Code::PostgresError {
                "database query error".to_owned()
            } else {
                db.message().to_owned()
            };
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
        None => Error::new(Code::PostgresError, format!("connection error: {e}"))
            .with_internal(json!({ "statement": sql })),
    }
}
