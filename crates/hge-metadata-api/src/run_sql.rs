//! `run_sql` (`/v2/query`): arbitrary SQL on a source, with V2's metadata consistency check.
//!
//! The SQL runs through the simple-query protocol (several statements allowed, results as
//! text), inside a transaction unless `no_transaction`. When the SQL may change the schema
//! (`check_metadata_consistency`, or V2's DDL heuristic), the tracked tables are snapshotted
//! by oid before and after inside the same transaction, renames and drops are applied to the
//! metadata, and the new catalog is resolved before committing: SQL that would leave the
//! metadata inconsistent is rolled back.

use std::collections::HashMap;

use deadpool_postgres::GenericClient;
use hge_core::{Code, QualifiedTable};
use hge_postgres::introspect;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::SimpleQueryMessage;

use crate::ctx::{Check, Ctx};
use crate::response::{ApiError, ApiResult, not_exists};
use crate::runtime::Runtime;
use crate::schema_diff::{self, TableSnapshot};

#[derive(Debug, Clone, Deserialize)]
pub struct RunSql {
    pub sql: String,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub cascade: bool,
    #[serde(default)]
    pub check_metadata_consistency: Option<bool>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub no_transaction: bool,
}

fn default_source() -> String {
    "default".to_owned()
}

impl RunSql {
    /// V2's `isSchemaCacheBuildRequiredRunSQL`.
    pub fn checks_metadata(&self) -> bool {
        match self.check_metadata_consistency {
            Some(b) => b,
            None => !self.read_only && looks_like_ddl(&self.sql),
        }
    }
}

/// `\balter\b|\bdrop\b|\breplace\b|\bcreate function\b|\bcomment on\b`, case-insensitive.
pub fn looks_like_ddl(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let has_word = |w: &str| {
        lower.match_indices(w).any(|(i, _)| {
            let before = i == 0 || !is_word(bytes[i - 1]);
            let end = i + w.len();
            let after = end >= bytes.len() || !is_word(bytes[end]);
            before && after
        })
    };
    has_word("alter")
        || has_word("drop")
        || has_word("replace")
        || has_word("create function")
        || has_word("comment on")
}

/// The `run_sql` response for a query's messages: the result of its last statement.
pub fn result_json(messages: &[SimpleQueryMessage]) -> Value {
    let mut header: Option<Vec<String>> = None;
    let mut rows: Vec<Value> = vec![];
    let mut last: Option<(Option<Vec<String>>, Vec<Value>)> = None;
    for m in messages {
        match m {
            SimpleQueryMessage::RowDescription(cols) => {
                header = Some(cols.iter().map(|c| c.name().to_owned()).collect());
            }
            SimpleQueryMessage::Row(row) => {
                if header.is_none() {
                    header = Some(row.columns().iter().map(|c| c.name().to_owned()).collect());
                }
                let cells = (0..row.len())
                    .map(|i| Value::String(row.get(i).unwrap_or("NULL").to_owned()))
                    .collect();
                rows.push(Value::Array(cells));
            }
            SimpleQueryMessage::CommandComplete(_) => {
                last = Some((header.take(), std::mem::take(&mut rows)));
            }
            _ => {}
        }
    }
    match last {
        Some((Some(header), rows)) => {
            let mut result = vec![Value::Array(
                header.into_iter().map(Value::String).collect(),
            )];
            result.extend(rows);
            json!({ "result_type": "TuplesOk", "result": result })
        }
        _ => json!({ "result_type": "CommandOk", "result": null }),
    }
}

/// V2's `query execution failed` error with the Postgres details in `internal`.
pub fn sql_error(e: tokio_postgres::Error, sql: &str) -> ApiError {
    match e.as_db_error() {
        Some(db) => {
            ApiError::bad(Code::PostgresError, "query execution failed").with_internal(json!({
                "statement": sql,
                "prepared": false,
                "arguments": [],
                "error": {
                    "exec_status": "FatalError",
                    "hint": db.hint(),
                    "message": db.message(),
                    "status_code": db.code().code(),
                    "description": db.detail(),
                },
            }))
        }
        None => ApiError::new(500, Code::PostgresError, format!("connection error: {e}"))
            .with_internal(json!({ "statement": sql })),
    }
}

/// Split SQL into statements on top-level `;` (quotes, identifiers, dollar quotes and
/// comments respected), for `no_transaction`.
pub fn split_statements(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = vec![];
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'$' => {
                // $tag$ ... $tag$
                let tag_end = b[i + 1..]
                    .iter()
                    .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                    .map(|p| i + 1 + p);
                if let Some(e) = tag_end
                    && b[e] == b'$'
                {
                    let tag = &sql[i..=e];
                    match sql[e + 1..].find(tag) {
                        Some(p) => i = e + 1 + p + tag.len() - 1,
                        None => i = b.len(),
                    }
                }
            }
            b';' => {
                let stmt = sql[start..i].trim();
                if !stmt.is_empty() {
                    out.push(stmt.to_owned());
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let stmt = sql[start.min(sql.len())..].trim();
    if !stmt.is_empty() {
        out.push(stmt.to_owned());
    }
    out
}

/// Run `run_sql` without touching metadata.
pub async fn run_plain<R: Runtime>(rt: &R, args: &RunSql) -> ApiResult<Value> {
    let source = rt
        .source(None, &args.source)
        .ok_or_else(|| source_missing(&args.source))?;
    let client =
        source.pool().get().await.map_err(|e| {
            ApiError::new(500, Code::PostgresError, format!("connection error: {e}"))
        })?;
    let sql = &args.sql;
    if args.no_transaction {
        let mut result = json!({ "result_type": "CommandOk", "result": null });
        for stmt in split_statements(sql) {
            let msgs = client
                .simple_query(&stmt)
                .await
                .map_err(|e| sql_error(e, &stmt))?;
            result = result_json(&msgs);
        }
        return Ok(result);
    }
    let mut client = client;
    let tx = client
        .build_transaction()
        .read_only(args.read_only)
        .start()
        .await
        .map_err(|e| sql_error(e, "BEGIN"))?;
    set_session(&tx).await?;
    // Dropping `tx` (on error, or if the request goes away) rolls back.
    let msgs = tx.simple_query(sql).await.map_err(|e| sql_error(e, sql))?;
    tx.commit().await.map_err(|e| sql_error(e, "COMMIT"))?;
    Ok(result_json(&msgs))
}

/// Like V2, record the admin session and a trace context for event triggers fired by the SQL.
async fn set_session(tx: &tokio_postgres::Transaction<'_>) -> ApiResult<()> {
    const SQL: &str = "SELECT set_config('hasura.user', $1, true), \
                       set_config('hasura.tracecontext', $2, true)";
    tx.execute(
        SQL,
        &[
            &r#"{"x-hasura-role":"admin"}"#,
            &hge_events::delivery::new_trace_context(),
        ],
    )
    .await
    .map(drop)
    .map_err(|e| sql_error(e, SQL))
}

fn source_missing(name: &str) -> ApiError {
    not_exists(format!("source with name \"{name}\" does not exist"))
}

/// Run `run_sql` with the metadata consistency check, inside the metadata request `ctx`.
pub async fn run_checked<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &RunSql) -> ApiResult<Value> {
    let source = ctx
        .rt
        .source(ctx.prepared.as_ref(), &args.source)
        .ok_or_else(|| source_missing(&args.source))?;
    let tracked: Vec<QualifiedTable> = ctx
        .metadata
        .source(&args.source)
        .map(|s| s.tables.iter().map(|t| t.table.clone()).collect())
        .unwrap_or_default();
    let mut client =
        source.pool().get().await.map_err(|e| {
            ApiError::new(500, Code::PostgresError, format!("connection error: {e}"))
        })?;
    let tx = client
        .build_transaction()
        .read_only(args.read_only)
        .start()
        .await
        .map_err(|e| sql_error(e, "BEGIN"))?;
    set_session(&tx).await?;
    let before = snapshot(&tx, &tracked).await?;
    let msgs = tx
        .simple_query(&args.sql)
        .await
        .map_err(|e| sql_error(e, &args.sql))?;
    let result = result_json(&msgs);
    let oids: Vec<u32> = before.iter().map(|t| t.oid).collect();
    let after = snapshot_oids(&tx, &oids).await?;

    let mut metadata = ctx.metadata.clone();
    schema_diff::apply(&mut metadata, &args.source, &before, &after, args.cascade)?;

    // Resolve against the catalog as this transaction sees it before committing.
    let tables: Vec<QualifiedTable> = metadata
        .source(&args.source)
        .map(|s| s.tables.iter().map(|t| t.table.clone()).collect())
        .unwrap_or_default();
    let catalog = introspect_in(&tx, &tables).await?;
    let previous = std::mem::replace(&mut ctx.metadata, metadata);
    let snapshot_ctx = (previous, ctx.prepared.clone(), ctx.inconsistencies.clone());
    let checked = ctx
        .check_with(
            Check::NoNew,
            HashMap::from([(args.source.clone(), catalog)]),
        )
        .await;
    if let Err(e) = checked {
        ctx.metadata = snapshot_ctx.0;
        ctx.prepared = snapshot_ctx.1;
        ctx.inconsistencies = snapshot_ctx.2;
        // V2's `withNewInconsistentObjsCheck` reports this as a 500.
        let mut e = e;
        if e.error.code == Code::Unexpected {
            e.status = 500;
            e.error.message = "cannot continue due to newly found inconsistent metadata".into();
        }
        return Err(e);
    }
    tx.commit().await.map_err(|e| sql_error(e, "COMMIT"))?;
    Ok(result)
}

const SNAPSHOT_SQL: &str = r#"
SELECT c.oid::int8, n.nspname::text, c.relname::text,
  coalesce((SELECT json_agg(json_build_array(a.attnum, a.attname) ORDER BY a.attnum)
              FROM pg_catalog.pg_attribute a
             WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped), '[]')::text,
  coalesce((SELECT json_agg(json_build_array(con.confrelid::int8, con.conkey) ORDER BY con.conname)
              FROM pg_catalog.pg_constraint con
             WHERE con.conrelid = c.oid AND con.contype = 'f'), '[]')::text
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
"#;

async fn snapshot(
    client: &impl GenericClient,
    tables: &[QualifiedTable],
) -> ApiResult<Vec<TableSnapshot>> {
    if tables.is_empty() {
        return Ok(vec![]);
    }
    let param = introspect::tables_param(tables);
    let sql = format!(
        "{SNAPSHOT_SQL} JOIN json_to_recordset($1::text::json) AS t(schema text, name text) \
         ON n.nspname = t.schema AND c.relname = t.name"
    );
    let rows = client
        .query(&sql, &[&param])
        .await
        .map_err(|e| sql_error(e, &sql))?;
    rows.iter().map(row_snapshot).collect()
}

async fn snapshot_oids(client: &impl GenericClient, oids: &[u32]) -> ApiResult<Vec<TableSnapshot>> {
    if oids.is_empty() {
        return Ok(vec![]);
    }
    let list = oids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("{SNAPSHOT_SQL} WHERE c.oid IN ({list})");
    let rows = client
        .query(&sql, &[])
        .await
        .map_err(|e| sql_error(e, &sql))?;
    rows.iter().map(row_snapshot).collect()
}

fn row_snapshot(row: &tokio_postgres::Row) -> ApiResult<TableSnapshot> {
    let oid: i64 = row.get(0);
    let cols: Vec<(i16, String)> = serde_json::from_str(row.get(3))
        .map_err(|e| ApiError::from(hge_core::Error::unexpected(e.to_string())))?;
    let fks: Vec<(i64, Vec<i16>)> = serde_json::from_str(row.get(4))
        .map_err(|e| ApiError::from(hge_core::Error::unexpected(e.to_string())))?;
    Ok(TableSnapshot {
        oid: oid as u32,
        table: QualifiedTable::new(row.get::<_, String>(1), row.get::<_, String>(2)),
        columns: cols.into_iter().collect(),
        foreign_keys: fks.into_iter().map(|(r, cols)| (r as u32, cols)).collect(),
    })
}

/// Introspect `tables` through `tx` (so uncommitted DDL is visible).
async fn introspect_in(
    tx: &tokio_postgres::Transaction<'_>,
    tables: &[QualifiedTable],
) -> ApiResult<hge_core::SourceCatalog> {
    use tokio_postgres::types::Type;
    let param = introspect::tables_param(tables);
    let tables_json: Option<String> = tx
        .query_typed(introspect::TABLES_SQL, &[(&param, Type::TEXT)])
        .await
        .map_err(|e| sql_error(e, introspect::TABLES_SQL))?
        .first()
        .and_then(|r| r.get(0));
    let enums_json: Option<String> = tx
        .query_typed(introspect::ENUMS_SQL, &[])
        .await
        .map_err(|e| sql_error(e, introspect::ENUMS_SQL))?
        .first()
        .and_then(|r| r.get(0));
    Ok(introspect::parse_catalog(
        tables_json.as_deref().unwrap_or("[]"),
        enums_json.as_deref().unwrap_or("[]"),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_heuristic() {
        assert!(looks_like_ddl("ALTER TABLE a ADD COLUMN b int"));
        assert!(looks_like_ddl("drop table x"));
        assert!(looks_like_ddl("CREATE OR REPLACE VIEW v AS SELECT 1"));
        assert!(looks_like_ddl("create function f() ..."));
        assert!(looks_like_ddl("COMMENT ON TABLE a IS 'x'"));
        assert!(!looks_like_ddl("CREATE TABLE a (id int)"));
        assert!(!looks_like_ddl("select altered, droppable from t"));
    }

    #[test]
    fn splits_statements() {
        let sql = "create table a(x text default 'a;b'); -- c;\n\
                   create function f() returns int as $$ select 1; $$ language sql;\
                   /* x; */ select \"a;\" from a";
        assert_eq!(
            split_statements(sql),
            vec![
                "create table a(x text default 'a;b')",
                "-- c;\ncreate function f() returns int as $$ select 1; $$ language sql",
                "/* x; */ select \"a;\" from a",
            ]
        );
    }
}
