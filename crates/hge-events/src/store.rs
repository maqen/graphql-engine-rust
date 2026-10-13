//! `hdb_catalog.event_log` / `event_invocation_logs` queries (V2's SQL, same locking rules).

use deadpool_postgres::Pool;

use crate::catalog::pg_err;

/// One fetched (and now locked) event.
#[derive(Debug, Clone)]
pub struct Event {
    pub id: String,
    pub schema_name: String,
    pub table_name: String,
    pub trigger_name: String,
    /// The stored `payload` as JSON text.
    pub payload: String,
    /// Attempts made so far.
    pub tries: i32,
    /// `created_at` as Postgres renders a `timestamp` in JSON (`2024-03-01T10:15:30.123456`),
    /// which is also how V2 renders it.
    pub created_at: Option<String>,
}

/// V2's `fetchEvents`: lock up to `limit` deliverable events of the given triggers. Locks
/// older than 30 minutes are treated as abandoned by a crashed instance.
pub const FETCH_EVENTS_SQL: &str = r#"
UPDATE hdb_catalog.event_log
SET locked = NOW()
WHERE id IN ( SELECT l.id
              FROM hdb_catalog.event_log l
              WHERE l.delivered = 'f' and l.error = 'f'
                    and (l.locked IS NULL or l.locked < (NOW() - interval '30 minute'))
                    and (l.next_retry_at is NULL or l.next_retry_at <= now())
                    and l.archived = 'f'
                    and l.trigger_name = ANY($2)
              /* NB: this ordering is important for our index `event_log_fetch_events` */
              ORDER BY locked NULLS FIRST, next_retry_at NULLS FIRST, created_at
              LIMIT $1
              FOR UPDATE SKIP LOCKED )
RETURNING id, schema_name, table_name, trigger_name, payload::text, tries, to_json(created_at) #>> '{}'
"#;

pub async fn fetch_events(
    pool: &Pool,
    limit: i64,
    triggers: &[String],
) -> Result<Vec<Event>, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let rows = client
        .query(FETCH_EVENTS_SQL, &[&limit, &triggers])
        .await
        .map_err(pg_err)?;
    Ok(rows
        .into_iter()
        .map(|r| Event {
            id: r.get(0),
            schema_name: r.get(1),
            table_name: r.get(2),
            trigger_name: r.get(3),
            payload: r.get(4),
            tries: r.get(5),
            created_at: r.get(6),
        })
        .collect())
}

/// A row for `event_invocation_logs`.
#[derive(Debug, Clone)]
pub struct InvocationRecord {
    pub status: Option<i32>,
    /// JSON text (V2's aeson encoding).
    pub request: String,
    pub response: String,
}

/// What happens to an event after a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterFailure {
    /// Try again after this many seconds.
    Retry(i64),
    /// Give up: `error = 't'`.
    SetError,
}

const INSERT_INVOCATION_SQL: &str = "INSERT INTO hdb_catalog.event_invocation_logs \
     (event_id, trigger_name, status, request, response) \
     VALUES ($1, $2, $3, $4::text::json, $5::text::json)";

/// Record a delivery attempt (V2 `recordSuccess` / `recordError`) in one transaction.
pub async fn record_attempt(
    pool: &Pool,
    event: &Event,
    invocation: &InvocationRecord,
    outcome: Option<AfterFailure>,
) -> Result<(), String> {
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(pg_err)?;
    tx.execute(
        INSERT_INVOCATION_SQL,
        &[
            &event.id,
            &event.trigger_name,
            &invocation.status,
            &invocation.request,
            &invocation.response,
        ],
    )
    .await
    .map_err(pg_err)?;
    tx.execute(
        "UPDATE hdb_catalog.event_log SET tries = tries + 1 WHERE id = $1",
        &[&event.id],
    )
    .await
    .map_err(pg_err)?;
    let secs = match outcome {
        Some(AfterFailure::Retry(secs)) => secs as f64,
        _ => 0.0,
    };
    match outcome {
        None => {
            tx.execute(
                "UPDATE hdb_catalog.event_log SET delivered = 't', next_retry_at = NULL, locked = NULL WHERE id = $1",
                &[&event.id],
            )
            .await
        }
        Some(AfterFailure::SetError) => {
            tx.execute(
                "UPDATE hdb_catalog.event_log SET error = 't', next_retry_at = NULL, locked = NULL WHERE id = $1",
                &[&event.id],
            )
            .await
        }
        // `now()` is converted to the session time zone on assignment to the `timestamp`
        // column, exactly like V2's bound `UTCTime`.
        Some(AfterFailure::Retry(_)) => {
            tx.execute(
                "UPDATE hdb_catalog.event_log SET next_retry_at = now() + make_interval(secs => $1::float8), locked = NULL WHERE id = $2",
                &[&secs, &event.id],
            )
            .await
        }
    }
    .map_err(pg_err)?;
    tx.commit().await.map_err(pg_err)?;
    Ok(())
}

/// Postpone an event whose trigger is not (or no longer) known, without counting a try.
pub async fn postpone(pool: &Pool, id: &str, secs: i64) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            "UPDATE hdb_catalog.event_log SET next_retry_at = now() + make_interval(secs => $1::float8), locked = NULL WHERE id = $2",
            &[&(secs as f64), &id],
        )
        .await
        .map_err(pg_err)?;
    Ok(())
}

/// Mark an event as failed without an invocation (request transform errors).
pub async fn set_error(pool: &Pool, id: &str) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            "UPDATE hdb_catalog.event_log SET error = 't', next_retry_at = NULL, locked = NULL WHERE id = $1",
            &[&id],
        )
        .await
        .map_err(pg_err)?;
    Ok(())
}

/// Release locks taken by this instance (shutdown). Returns how many were unlocked.
pub async fn unlock(pool: &Pool, ids: &[String]) -> Result<i64, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let row = client
        .query_one(
            r#"WITH "cte" AS
(UPDATE hdb_catalog.event_log
SET locked = NULL
WHERE id = ANY($1::text[])
-- only unlock those events that have been locked, it's possible
-- that an event has been processed but not yet been removed from
-- the saved locked events, which will lead to a double send
AND locked IS NOT NULL
RETURNING *)
SELECT count(*) FROM "cte""#,
            &[&ids],
        )
        .await
        .map_err(pg_err)?;
    Ok(row.get(0))
}
