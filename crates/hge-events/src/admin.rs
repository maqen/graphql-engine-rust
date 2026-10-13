//! The event-log side of the metadata API: manual invocation, redelivery and the log
//! queries (`pg_invoke_event_trigger`, `pg_redeliver_event`, `pg_get_event_logs`,
//! `pg_get_event_invocation_logs`, `pg_get_event_by_id`), with V2's SQL and response shapes.

use deadpool_postgres::Pool;
use hge_core::QualifiedTable;
use serde_json::{Map, Value};
use tokio_postgres::Row;

use crate::catalog::pg_err;

/// Record a `MANUAL` event with `payload` (JSON text) as its new row. `session` and
/// `trace_context` (JSON text) become the event's `session_variables` / `trace_context`.
pub async fn invoke(
    pool: &Pool,
    table: &QualifiedTable,
    trigger: &str,
    payload: &str,
    session: &str,
    trace_context: &str,
) -> Result<String, String> {
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(pg_err)?;
    tx.execute(
        "SELECT set_config('hasura.user', $1, true), set_config('hasura.tracecontext', $2, true)",
        &[&session, &trace_context],
    )
    .await
    .map_err(pg_err)?;
    let row = tx
        .query_one(
            "SELECT hdb_catalog.insert_event_log($1, $2, $3, 'MANUAL', \
             json_build_object('old', NULL, 'new', $4::text::json))",
            &[&table.schema, &table.name, &trigger, &payload],
        )
        .await
        .map_err(pg_err)?;
    let id: String = row.get(0);
    tx.commit().await.map_err(pg_err)?;
    Ok(id)
}

/// Archive a deleted trigger's events (V2's `archiveEvents`). A source whose event catalog
/// was never created has nothing to archive.
pub async fn archive(pool: &Pool, trigger: &str) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let catalog: bool = client
        .query_one(
            "SELECT to_regclass('hdb_catalog.event_log') IS NOT NULL",
            &[],
        )
        .await
        .map_err(pg_err)?
        .get(0);
    if catalog {
        client
            .execute(
                "UPDATE hdb_catalog.event_log SET archived = 't' WHERE trigger_name = $1",
                &[&trigger],
            )
            .await
            .map_err(pg_err)?;
    }
    Ok(())
}

/// What [`redeliver`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redelivery {
    Queued,
    NotFound,
    /// Locked by a delivery in the last 30 minutes.
    Busy,
}

/// V2's `redeliverEventTx`: reset `delivered`, `error` and `tries` so the event is picked
/// up again (archived events stay archived; `next_retry_at` is kept).
pub async fn redeliver(pool: &Pool, event_id: &str) -> Result<Redelivery, String> {
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(pg_err)?;
    let busy = tx
        .query_opt(
            "SELECT l.locked IS NOT NULL AND l.locked >= (NOW() - interval '30 minute') \
             FROM hdb_catalog.event_log l WHERE l.id = $1 FOR UPDATE",
            &[&event_id],
        )
        .await
        .map_err(pg_err)?;
    match busy.map(|r| r.get::<_, bool>(0)) {
        None => return Ok(Redelivery::NotFound),
        Some(true) => return Ok(Redelivery::Busy),
        Some(false) => {}
    }
    tx.execute(
        "UPDATE hdb_catalog.event_log SET delivered = 'f', error = 'f', tries = 0 WHERE id = $1",
        &[&event_id],
    )
    .await
    .map_err(pg_err)?;
    tx.commit().await.map_err(pg_err)?;
    Ok(Redelivery::Queued)
}

/// The `status` filter of `pg_get_event_logs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStatus {
    All,
    Pending,
    Processed,
}

/// `created_at` / `next_retry_at` are `timestamp` columns that V2 decodes as if they were
/// UTC: the naive value with a `Z`. `locked` is a real `timestamptz`.
const EVENT_COLUMNS: &str = "id, schema_name, table_name, trigger_name, payload::text, \
     delivered, error, tries, (to_json(created_at) #>> '{}') || 'Z', \
     (to_json(locked AT TIME ZONE 'UTC') #>> '{}') || 'Z', \
     (to_json(next_retry_at) #>> '{}') || 'Z', archived";

const INVOCATION_COLUMNS: &str = "id, trigger_name, event_id, status, request::text, \
     response::text, (to_json(created_at) #>> '{}') || 'Z'";

/// `pg_get_event_logs`: the trigger's events, newest first.
pub async fn event_logs(
    pool: &Pool,
    trigger: &str,
    status: LogStatus,
    limit: i64,
    offset: i64,
) -> Result<Vec<Value>, String> {
    let filter = match status {
        LogStatus::All => "",
        LogStatus::Pending => "AND delivered = false AND error = false AND archived = false",
        LogStatus::Processed => "AND (delivered = true OR error = true) AND archived = false",
    };
    let sql = format!(
        "SELECT {EVENT_COLUMNS} FROM hdb_catalog.event_log WHERE trigger_name = $1 {filter} \
         ORDER BY created_at DESC LIMIT $2 OFFSET $3"
    );
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let rows = client
        .query(&sql, &[&trigger, &limit, &offset])
        .await
        .map_err(pg_err)?;
    rows.iter().map(event_json).collect()
}

/// Which invocations [`invocation_logs`] returns.
#[derive(Debug, Clone, Copy)]
pub enum InvocationsOf<'a> {
    Trigger(&'a str),
    Event(&'a str),
}

/// `pg_get_event_invocation_logs` (and the invocations of `pg_get_event_by_id`), newest
/// first.
pub async fn invocation_logs(
    pool: &Pool,
    of: InvocationsOf<'_>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Value>, String> {
    let (column, value) = match of {
        InvocationsOf::Trigger(t) => ("trigger_name", t),
        InvocationsOf::Event(e) => ("event_id", e),
    };
    let sql = format!(
        "SELECT {INVOCATION_COLUMNS} FROM hdb_catalog.event_invocation_logs \
         WHERE {column} = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3"
    );
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let rows = client
        .query(&sql, &[&value, &limit, &offset])
        .await
        .map_err(pg_err)?;
    rows.iter().map(invocation_json).collect()
}

/// `pg_get_event_by_id`: the event, if it exists, and its invocations.
pub async fn event_by_id(
    pool: &Pool,
    event_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Option<(Value, Vec<Value>)>, String> {
    let row = {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        client
            .query_opt(
                &format!("SELECT {EVENT_COLUMNS} FROM hdb_catalog.event_log WHERE id = $1"),
                &[&event_id],
            )
            .await
            .map_err(pg_err)?
    };
    let Some(row) = row else {
        return Ok(None);
    };
    let event = event_json(&row)?;
    let invocations = invocation_logs(pool, InvocationsOf::Event(event_id), limit, offset).await?;
    Ok(Some((event, invocations)))
}

fn event_json(row: &Row) -> Result<Value, String> {
    let mut m = Map::new();
    for (i, key) in ["id", "schema_name", "table_name", "trigger_name"]
        .into_iter()
        .enumerate()
    {
        m.insert(key.into(), Value::String(row.get(i)));
    }
    m.insert("payload".into(), stored_json(row.get(4))?);
    m.insert("delivered".into(), Value::Bool(row.get(5)));
    m.insert("error".into(), Value::Bool(row.get(6)));
    m.insert("tries".into(), Value::from(row.get::<_, i32>(7)));
    for (i, key) in [(8, "created_at"), (9, "locked"), (10, "next_retry_at")] {
        m.insert(key.into(), opt_string(row.get(i)));
    }
    m.insert("archived".into(), Value::Bool(row.get(11)));
    Ok(Value::Object(m))
}

fn invocation_json(row: &Row) -> Result<Value, String> {
    let mut m = Map::new();
    m.insert("id".into(), Value::String(row.get(0)));
    m.insert("trigger_name".into(), opt_string(row.get(1)));
    m.insert("event_id".into(), opt_string(row.get(2)));
    m.insert(
        "http_status".into(),
        row.get::<_, Option<i32>>(3)
            .map_or(Value::Null, Value::from),
    );
    m.insert("request".into(), opt_json(row.get(4))?);
    m.insert("response".into(), opt_json(row.get(5))?);
    m.insert("created_at".into(), opt_string(row.get(6)));
    Ok(Value::Object(m))
}

fn opt_string(s: Option<String>) -> Value {
    s.map_or(Value::Null, Value::String)
}

fn opt_json(text: Option<String>) -> Result<Value, String> {
    text.map_or(Ok(Value::Null), stored_json)
}

/// A stored JSON value as V2 returns it: a decoded aeson `Value`, so keys are sorted.
fn stored_json(text: String) -> Result<Value, String> {
    serde_json::from_str(&text)
        .map(sorted)
        .map_err(|e| format!("invalid JSON in the event log: {e}"))
}

fn sorted(v: Value) -> Value {
    match v {
        Value::Object(obj) => {
            let mut entries: Vec<_> = obj.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}
