//! The metadata-database side of the cron and one-off scheduled event commands
//! (`create_cron_trigger`, `create_scheduled_event`, `get_scheduled_events`, ...), with V2's
//! SQL and response shapes.

use deadpool_postgres::Pool;
use serde_json::{Map, Value};
use tokio_postgres::Row;
use tokio_postgres::types::ToSql;

use crate::admin::{opt_json, opt_string};
use crate::catalog::pg_err;
use crate::cron::CronSchedule;
use crate::scheduled::{
    CRON_EVENTS_AHEAD, Kind, delete_future_cron_events, insert_cron_events, now_secs, utc,
};

/// A change to a cron trigger's stored events, applied once the metadata change that causes
/// it is committed (so a rejected request never changes what fires).
#[derive(Debug, Clone)]
pub enum CronEventChange {
    /// V2's `populateInitialCronTriggerEvents`: the next 100 events from now. With `replace`,
    /// the trigger's untried future events are deleted first (`dropFutureCronEventsTx`).
    Populate {
        trigger: String,
        schedule: CronSchedule,
        replace: bool,
    },
    /// The trigger was deleted: drop its untried future events.
    Drop { trigger: String },
}

/// Apply committed [`CronEventChange`]s in order.
pub async fn apply_cron_changes(pool: &Pool, changes: &[CronEventChange]) -> Result<(), String> {
    for change in changes {
        match change {
            CronEventChange::Populate {
                trigger,
                schedule,
                replace,
            } => {
                if *replace {
                    delete_future_cron_events(pool, std::slice::from_ref(trigger)).await?;
                }
                let times = schedule.next_n(now_secs(), CRON_EVENTS_AHEAD);
                insert_cron_events(pool, trigger, &times).await?;
            }
            CronEventChange::Drop { trigger } => {
                delete_future_cron_events(pool, std::slice::from_ref(trigger)).await?;
            }
        }
    }
    Ok(())
}

/// A one-off event to insert; JSON columns are given as JSON text.
pub struct NewOneOff<'a> {
    pub webhook_conf: &'a str,
    /// An ISO 8601 time with a zone, as `timestamptz` accepts it.
    pub schedule_at: &'a str,
    pub payload: Option<&'a str>,
    pub retry_conf: &'a str,
    pub header_conf: &'a str,
    pub comment: Option<&'a str>,
}

/// `create_scheduled_event`: returns the new event's id.
pub async fn create_one_off(pool: &Pool, ev: &NewOneOff<'_>) -> Result<String, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let row = client
        .query_one(
            "INSERT INTO hdb_catalog.hdb_scheduled_events \
               (webhook_conf, scheduled_time, payload, retry_conf, header_conf, comment) \
             VALUES ($1::text::json, $2::text::timestamptz, $3::text::json, $4::text::json, \
                     $5::text::json, $6) RETURNING id",
            &[
                &ev.webhook_conf,
                &ev.schedule_at,
                &ev.payload,
                &ev.retry_conf,
                &ev.header_conf,
                &ev.comment,
            ],
        )
        .await
        .map_err(pg_err)?;
    Ok(row.get(0))
}

/// `delete_scheduled_event` (a missing event is not an error, as in V2).
pub async fn delete_event(pool: &Pool, kind: Kind, id: &str) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            &format!(
                "DELETE FROM hdb_catalog.{} WHERE id = $1",
                kind.events_table()
            ),
            &[&id],
        )
        .await
        .map_err(pg_err)?;
    Ok(())
}

/// Paging of the listing commands.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Also count all matching rows.
    pub count: bool,
}

/// One `order_by` item of `get_scheduled_events`.
#[derive(Debug, Clone)]
pub struct OrderBy {
    pub column: String,
    pub descending: bool,
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Run `SELECT {columns} FROM {from} WHERE {filter} ...` and, with `page.count`, the count of
/// all matching rows.
async fn page_query(
    pool: &Pool,
    columns: &str,
    from: &str,
    filter: &str,
    params: &[&(dyn ToSql + Sync)],
    order: &str,
    page: &Page,
) -> Result<(Option<i64>, Vec<Row>), String> {
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    // One snapshot for both statements, so the count agrees with the page.
    let tx = client
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .map_err(pg_err)?;
    let count = if page.count {
        let row = tx
            .query_one(
                &format!("SELECT count(*) FROM {from} WHERE {filter}"),
                params,
            )
            .await
            .map_err(pg_err)?;
        Some(row.get::<_, i64>(0))
    } else {
        None
    };
    let mut sql = format!("SELECT {columns} FROM {from} WHERE {filter} ORDER BY {order}");
    if let Some(limit) = page.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    if let Some(offset) = page.offset {
        sql.push_str(&format!(" OFFSET {offset}"));
    }
    let rows = tx.query(&sql, params).await.map_err(pg_err)?;
    tx.commit().await.map_err(pg_err)?;
    Ok((count, rows))
}

/// `get_scheduled_events`: a cron trigger's events (`trigger` given) or the one-off events,
/// by `scheduled_time` unless `order_by` says otherwise.
pub async fn events(
    pool: &Pool,
    kind: Kind,
    trigger: Option<&str>,
    statuses: &[String],
    order_by: &[OrderBy],
    page: &Page,
) -> Result<(Option<i64>, Vec<Value>), String> {
    let mut filter = vec![];
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![];
    if let Some(t) = &trigger {
        params.push(t);
        filter.push(format!("trigger_name = ${}", params.len()));
    }
    if !statuses.is_empty() {
        params.push(&statuses);
        filter.push(format!("status = ANY(${}::text[])", params.len()));
    }
    let filter = if filter.is_empty() {
        "true".to_owned()
    } else {
        filter.join(" AND ")
    };
    let order = if order_by.is_empty() {
        "scheduled_time ASC".to_owned()
    } else {
        order_by
            .iter()
            .map(|o| {
                let dir = if o.descending { "DESC" } else { "ASC" };
                format!("{} {dir}", quote_ident(&o.column))
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let common = format!(
        "id, {}, status, tries, {}, {}",
        utc("scheduled_time"),
        utc("created_at"),
        utc("next_retry_at")
    );
    let columns = match kind {
        Kind::Cron => format!("{common}, trigger_name"),
        Kind::OneOff => format!(
            "{common}, webhook_conf::text, retry_conf::text, payload::text, \
             header_conf::text, comment"
        ),
    };
    let from = format!("hdb_catalog.{}", kind.events_table());
    let (count, rows) = page_query(pool, &columns, &from, &filter, &params, &order, page).await?;
    let events = rows
        .iter()
        .map(|r| event_json(kind, r))
        .collect::<Result<_, _>>()?;
    Ok((count, events))
}

/// Keys in aeson's (sorted) order.
fn event_json(kind: Kind, r: &Row) -> Result<Value, String> {
    let mut m = Map::new();
    let created_at = opt_string(r.get(4));
    let next_retry_at = opt_string(r.get(5));
    let scheduled_time = Value::String(r.get(1));
    let status = Value::String(r.get(2));
    let tries = Value::from(r.get::<_, i32>(3));
    let id = Value::String(r.get(0));
    match kind {
        Kind::Cron => {
            m.insert("created_at".into(), created_at);
            m.insert("id".into(), id);
            m.insert("next_retry_at".into(), next_retry_at);
            m.insert("scheduled_time".into(), scheduled_time);
            m.insert("status".into(), status);
            m.insert("tries".into(), tries);
            m.insert("trigger_name".into(), Value::String(r.get(6)));
        }
        Kind::OneOff => {
            m.insert("comment".into(), opt_string(r.get(10)));
            m.insert("created_at".into(), created_at);
            m.insert("header_conf".into(), opt_json(r.get(9))?);
            m.insert("id".into(), id);
            m.insert("next_retry_at".into(), next_retry_at);
            m.insert("payload".into(), opt_json(r.get(8))?);
            m.insert("request_transform".into(), Value::Null);
            m.insert("response_transform".into(), Value::Null);
            m.insert("retry_conf".into(), opt_json(r.get(7))?);
            m.insert("scheduled_time".into(), scheduled_time);
            m.insert("status".into(), status);
            m.insert("tries".into(), tries);
            m.insert("webhook_conf".into(), opt_json(r.get(6))?);
        }
    }
    Ok(Value::Object(m))
}

/// Which invocations [`invocations`] returns.
#[derive(Debug, Clone, Copy)]
pub enum InvocationsOf<'a> {
    /// Of one event.
    Event(&'a str),
    /// Of a cron trigger's events.
    Trigger(&'a str),
    /// Of every one-off event.
    AllOneOff,
}

/// `get_scheduled_event_invocations`, newest first.
pub async fn invocations(
    pool: &Pool,
    kind: Kind,
    of: InvocationsOf<'_>,
    page: &Page,
) -> Result<(Option<i64>, Vec<Value>), String> {
    let logs = format!("hdb_catalog.{}", kind.invocations_table());
    let (from, filter, params): (String, &str, Vec<&(dyn ToSql + Sync)>) = match &of {
        InvocationsOf::Event(id) => (format!("{logs} i"), "i.event_id = $1", vec![id]),
        InvocationsOf::Trigger(name) => (
            format!(
                "{logs} i JOIN hdb_catalog.{} e ON e.id = i.event_id",
                kind.events_table()
            ),
            "e.trigger_name = $1",
            vec![name],
        ),
        InvocationsOf::AllOneOff => (format!("{logs} i"), "true", vec![]),
    };
    let columns = format!(
        "i.id, i.event_id, i.status, i.request::text, i.response::text, {}",
        utc("i.created_at")
    );
    let (count, rows) = page_query(
        pool,
        &columns,
        &from,
        filter,
        &params,
        "i.created_at DESC",
        page,
    )
    .await?;
    let invocations = rows
        .iter()
        .map(|r| {
            let mut m = Map::new();
            m.insert("created_at".into(), opt_string(r.get(5)));
            m.insert("event_id".into(), opt_string(r.get(1)));
            m.insert("id".into(), Value::String(r.get(0)));
            m.insert("request".into(), opt_json(r.get(3))?);
            m.insert("response".into(), opt_json(r.get(4))?);
            m.insert(
                "status".into(),
                r.get::<_, Option<i32>>(2).map_or(Value::Null, Value::from),
            );
            Ok(Value::Object(m))
        })
        .collect::<Result<_, String>>()?;
    Ok((count, invocations))
}
