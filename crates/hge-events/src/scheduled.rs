//! Cron triggers and one-off scheduled events (V2's `ScheduledTrigger.hs`).
//!
//! Both live in the metadata database (`hdb_catalog.hdb_cron_events`,
//! `hdb_scheduled_events` and their invocation logs). A generator keeps 100 upcoming events
//! per cron trigger; a delivery loop locks due events with `FOR UPDATE SKIP LOCKED`, so any
//! number of replicas can run both.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deadpool_postgres::{ClientWrapper, Object, Pool};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::catalog::pg_err;
use crate::cron::CronSchedule;
use crate::delivery::{self, Attempt, AttemptResult, INVOCATION_VERSION_SCHEDULED, TraceIds};
use crate::json::Json;
use crate::model::{
    Env, HeaderConf, ResolvedHeader, ResolvedWebhook, resolve_headers, resolve_template,
    resolve_webhook,
};
use crate::processor::EventingConfig;
use crate::store::{AfterFailure, InvocationRecord};
use crate::webhook::WebhookClient;

/// Upcoming events kept per cron trigger.
pub const CRON_EVENTS_AHEAD: usize = 100;
const GENERATOR_INTERVAL: Duration = Duration::from_secs(60);
/// How often the generator looks for a metadata change (new triggers get events at once).
const GENERATOR_POLL: Duration = Duration::from_secs(1);
const DELIVERY_INTERVAL: Duration = Duration::from_secs(10);
/// Due events locked per table and iteration (V2 has no limit; this bounds a backlog).
const FETCH_LIMIT: i64 = 1000;
/// Advisory lock class held (session level, `(class, hashtext(id))`) by the instance
/// delivering an event. A `locked` event without it belongs to a dead instance.
const DELIVERY_LOCK_CLASS: i32 = 0x4853_4556;
/// The cron / one-off catalog tables.
const CATALOG_TABLES: [&str; 4] = [
    "hdb_cron_events",
    "hdb_cron_event_invocation_logs",
    "hdb_scheduled_events",
    "hdb_scheduled_event_invocation_logs",
];

/// `STRetryConf`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduledRetryConf {
    pub num_retries: i64,
    pub retry_interval_seconds: f64,
    pub timeout_seconds: f64,
    pub tolerance_seconds: f64,
}

impl Default for ScheduledRetryConf {
    fn default() -> Self {
        ScheduledRetryConf {
            num_retries: 0,
            retry_interval_seconds: 10.0,
            timeout_seconds: 60.0,
            tolerance_seconds: 21600.0,
        }
    }
}

impl ScheduledRetryConf {
    pub fn validate(&self) -> Result<(), String> {
        let checks = [
            (self.num_retries < 0, "num_retries"),
            (self.retry_interval_seconds < 0.0, "retry_interval_seconds"),
            (self.timeout_seconds < 0.0, "timeout_seconds"),
            (self.tolerance_seconds < 0.0, "tolerance_seconds"),
        ];
        match checks.iter().find(|(bad, _)| *bad) {
            Some((_, field)) => Err(format!("{field} cannot be a negative value")),
            None => Ok(()),
        }
    }

    /// Haskell `round` (ties to even) of the retry interval.
    fn retry_interval(&self) -> i64 {
        self.retry_interval_seconds.round_ties_even() as i64
    }

    fn http_timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_seconds.round_ties_even().max(0.0) as u64)
    }

    fn processing_timeout(&self) -> Duration {
        Duration::from_secs_f64(self.timeout_seconds.max(0.0)).min(delivery::MAX_PROCESSING_TIMEOUT)
    }
}

/// `CronTriggerMetadata` (an entry of `cron_triggers.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronTriggerConf {
    pub name: String,
    pub webhook: String,
    pub schedule: String,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub retry_conf: ScheduledRetryConf,
    #[serde(default)]
    pub headers: Vec<HeaderConf>,
    pub include_in_metadata: bool,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub request_transform: Option<Value>,
    #[serde(default)]
    pub response_transform: Option<Value>,
}

/// A cron trigger resolved for delivery.
#[derive(Debug, Clone)]
pub struct CronTriggerInfo {
    pub conf: CronTriggerConf,
    pub schedule: CronSchedule,
    pub webhook: ResolvedWebhook,
    pub headers: Vec<ResolvedHeader>,
}

impl CronTriggerInfo {
    /// Parse and resolve against the environment; errors are V2's inconsistency messages.
    pub fn resolve(conf: CronTriggerConf, env: &dyn Env) -> Result<Self, String> {
        let schedule = CronSchedule::parse(&conf.schedule)?;
        conf.retry_conf.validate()?;
        let webhook = resolve_webhook(Some(&conf.webhook), None, env)?;
        let headers = resolve_headers(&conf.headers, env)?;
        Ok(CronTriggerInfo {
            conf,
            schedule,
            webhook,
            headers,
        })
    }
}

/// Everything the scheduled-event workers need from one metadata version.
#[derive(Default)]
pub struct ScheduledTriggers {
    /// Deliverable cron triggers, by name.
    pub cron: HashMap<String, Arc<CronTriggerInfo>>,
    /// Every cron trigger name in metadata, including inconsistent ones: their stored events
    /// are kept (a trigger can be inconsistent on one replica only, e.g. a missing env var).
    pub in_metadata: HashSet<String>,
}

pub type ScheduledProvider = Arc<dyn Fn() -> Arc<ScheduledTriggers> + Send + Sync>;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Check that the cron / one-off tables exist. They belong to the metadata catalog, which
/// the server creates or upgrades (V2's `initialise.sql`) before workers start.
pub async fn check_catalog(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let mut missing = vec![];
    for table in CATALOG_TABLES {
        let present: bool = client
            .query_one(
                "SELECT to_regclass('hdb_catalog.' || $1) IS NOT NULL",
                &[&table],
            )
            .await
            .map_err(pg_err)?
            .get(0);
        if !present {
            missing.push(table);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the metadata catalog has no hdb_catalog.{}",
            missing.join(", hdb_catalog.")
        ))
    }
}

/// Insert cron events (`ON CONFLICT DO NOTHING` relies on the partial unique index).
pub async fn insert_cron_events(pool: &Pool, trigger: &str, times: &[i64]) -> Result<(), String> {
    if times.is_empty() {
        return Ok(());
    }
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let times: Vec<f64> = times.iter().map(|t| *t as f64).collect();
    client
        .execute(
            "INSERT INTO hdb_catalog.hdb_cron_events (trigger_name, scheduled_time) \
             SELECT $1, to_timestamp(t) FROM unnest($2::float8[]) AS t ON CONFLICT DO NOTHING",
            &[&trigger, &times],
        )
        .await
        .map_err(pg_err)?;
    Ok(())
}

/// Delete future untried events of cron triggers (trigger deleted or changed).
pub async fn delete_future_cron_events(pool: &Pool, triggers: &[String]) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            "DELETE FROM hdb_catalog.hdb_cron_events \
             WHERE scheduled_time > now() AND tries = 0 AND trigger_name = ANY($1::text[])",
            &[&triggers],
        )
        .await
        .map_err(pg_err)?;
    Ok(())
}

/// Bring stored future events in line with the current cron triggers (V2 does this when
/// metadata is replaced): delete untried future events of triggers that are gone, and those
/// that no longer match their trigger's schedule.
pub async fn reconcile_cron_events(
    pool: &Pool,
    triggers: &ScheduledTriggers,
) -> Result<(), String> {
    let names: Vec<String> = triggers
        .cron
        .keys()
        .chain(&triggers.in_metadata)
        .cloned()
        .collect();
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            "DELETE FROM hdb_catalog.hdb_cron_events WHERE scheduled_time > now() AND tries = 0 \
             AND status = 'scheduled' AND NOT (trigger_name = ANY($1::text[]))",
            &[&names],
        )
        .await
        .map_err(pg_err)?;
    let rows = client
        .query(
            "SELECT id, trigger_name, extract(epoch FROM scheduled_time)::float8 \
             FROM hdb_catalog.hdb_cron_events WHERE scheduled_time > now() AND tries = 0 \
             AND status = 'scheduled'",
            &[],
        )
        .await
        .map_err(pg_err)?;
    let stale: Vec<String> = rows
        .iter()
        .filter(|r| {
            let t: f64 = r.get(2);
            triggers.cron.get(r.get::<_, &str>(1)).is_some_and(|info| {
                t.fract() != 0.0 || (t as i64) % 60 != 0 || !info.schedule.matches(t as i64)
            })
        })
        .map(|r| r.get(0))
        .collect();
    if !stale.is_empty() {
        client
            .execute(
                "DELETE FROM hdb_catalog.hdb_cron_events WHERE id = ANY($1::text[])",
                &[&stale],
            )
            .await
            .map_err(pg_err)?;
    }
    Ok(())
}

/// One generator run: top up every cron trigger with fewer than 100 upcoming events.
pub async fn generate_cron_events(pool: &Pool, triggers: &ScheduledTriggers) -> Result<(), String> {
    if triggers.cron.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = triggers.cron.keys().cloned().collect();
    let rows = {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        client
            .query(
                "SELECT t.trigger_name, coalesce(q.upcoming_events_count, 0), \
                        extract(epoch FROM coalesce(q.max_scheduled_time, now()))::float8 \
                 FROM (SELECT UNNEST ($1::text[]) as trigger_name) as t \
                 LEFT JOIN \
                 ( SELECT trigger_name, count(1) as upcoming_events_count, max(scheduled_time) as max_scheduled_time \
                   FROM hdb_catalog.hdb_cron_events \
                   WHERE tries = 0 and status = 'scheduled' \
                   GROUP BY trigger_name ) AS q \
                 ON t.trigger_name = q.trigger_name \
                 WHERE coalesce(q.upcoming_events_count, 0) < 100",
                &[&names],
            )
            .await
            .map_err(pg_err)?
    };
    for row in rows {
        let name: String = row.get(0);
        let from: f64 = row.get(2);
        let Some(info) = triggers.cron.get(&name) else {
            continue;
        };
        let times = info.schedule.next_n(from.floor() as i64, CRON_EVENTS_AHEAD);
        insert_cron_events(pool, &name, &times).await?;
    }
    Ok(())
}

/// Release `locked` events whose delivering instance is gone, i.e. nobody holds their
/// [`DELIVERY_LOCK_CLASS`] advisory lock. Runs on every delivery iteration, so an instance
/// that died mid-delivery holds its events only until another instance looks, and an
/// instance that is still delivering never loses them (V2 instead unlocks everything at
/// startup, which can deliver twice). Returns the number of released events.
pub async fn unlock_stale(pool: &Pool) -> Result<u64, String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let mut released = 0;
    for table in ["hdb_cron_events", "hdb_scheduled_events"] {
        released += client
            .execute(
                &format!(
                    "WITH locked AS MATERIALIZED ( \
                       SELECT id FROM hdb_catalog.{table} WHERE status = 'locked' \
                       FOR UPDATE SKIP LOCKED), \
                     orphaned AS MATERIALIZED ( \
                       SELECT id FROM locked \
                       WHERE pg_try_advisory_xact_lock({DELIVERY_LOCK_CLASS}, hashtext(id::text))) \
                     UPDATE hdb_catalog.{table} t SET status = 'scheduled' \
                     FROM orphaned WHERE t.id = orphaned.id"
                ),
                &[],
            )
            .await
            .map_err(pg_err)?;
    }
    Ok(released)
}

/// Put events this instance locked but did not finish back to `scheduled` (shutdown).
async fn release(pool: &Pool, events: &[(String, Kind)]) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    for kind in [Kind::Cron, Kind::OneOff] {
        let ids: Vec<&str> = events
            .iter()
            .filter(|(_, k)| *k == kind)
            .map(|(id, _)| id.as_str())
            .collect();
        if ids.is_empty() {
            continue;
        }
        client
            .execute(
                &format!(
                    "UPDATE hdb_catalog.{} SET status = 'scheduled' \
                     WHERE id = ANY($1) AND status = 'locked'",
                    kind.events_table()
                ),
                &[&ids],
            )
            .await
            .map_err(pg_err)?;
    }
    Ok(())
}

/// Which table an event lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cron,
    OneOff,
}

impl Kind {
    pub fn events_table(self) -> &'static str {
        match self {
            Kind::Cron => "hdb_cron_events",
            Kind::OneOff => "hdb_scheduled_events",
        }
    }

    pub fn invocations_table(self) -> &'static str {
        match self {
            Kind::Cron => "hdb_cron_event_invocation_logs",
            Kind::OneOff => "hdb_scheduled_event_invocation_logs",
        }
    }
}

/// A locked, due scheduled event.
#[derive(Debug, Clone)]
pub struct ScheduledEvent {
    pub kind: Kind,
    pub id: String,
    pub trigger_name: Option<String>,
    /// Unix seconds (with fraction).
    pub scheduled_epoch: f64,
    /// `YYYY-MM-DDTHH:MM:SS[.ffffff]Z`
    pub scheduled_time: String,
    pub created_at: Option<String>,
    pub tries: i32,
    // One-off only:
    /// JSON text of the stored columns.
    pub webhook_conf: Option<String>,
    pub retry_conf: Option<String>,
    pub payload: Option<String>,
    pub header_conf: Option<String>,
    pub comment: Option<String>,
}

/// UTC rendering of a `timestamptz` like aeson's `UTCTime`.
const UTC_JSON: &str = "(to_json({c} AT TIME ZONE 'UTC') #>> '{{}}') || 'Z'";

fn utc(col: &str) -> String {
    UTC_JSON.replace("{c}", col).replace("{{}}", "{}")
}

/// Lock up to `limit` due events of one kind. `client` is the instance's lock-holding
/// connection: each event's [`DELIVERY_LOCK_CLASS`] advisory lock is taken in the same
/// statement, so no other instance can see it `locked` but unowned.
pub async fn fetch_due(
    client: &tokio_postgres::Client,
    kind: Kind,
    cron_triggers: &[String],
    limit: i64,
) -> Result<Vec<ScheduledEvent>, String> {
    let table = kind.events_table();
    let filter = match kind {
        Kind::Cron => "AND trigger_name = ANY($2)",
        Kind::OneOff => "",
    };
    let extra = match kind {
        Kind::Cron => "trigger_name, NULL::text, NULL::text, NULL::text, NULL::text, NULL::text".to_owned(),
        Kind::OneOff => {
            "NULL::text, webhook_conf::text, retry_conf::text, payload::text, header_conf::text, comment".to_owned()
        }
    };
    let sql = format!(
        "WITH cte AS
  ( UPDATE hdb_catalog.{table}
    SET status = 'locked'
    WHERE id IN ( SELECT t.id
                  FROM hdb_catalog.{table} t
                  WHERE ( t.status = 'scheduled'
                          and (
                           (t.next_retry_at is NULL and t.scheduled_time <= now()) or
                           (t.next_retry_at is not NULL and t.next_retry_at <= now())
                          )
                        ) {filter}
                  ORDER BY coalesce(t.next_retry_at, t.scheduled_time)
                  LIMIT $1
                  FOR UPDATE SKIP LOCKED
                  )
    RETURNING *
  )
SELECT id, extract(epoch FROM scheduled_time)::float8, {st}, {ca}, tries, {extra}, \
       pg_try_advisory_lock({DELIVERY_LOCK_CLASS}, hashtext(id::text)) FROM cte",
        st = utc("scheduled_time"),
        ca = utc("created_at"),
    );
    let limit = limit.min(FETCH_LIMIT);
    let rows = match kind {
        Kind::Cron => client.query(&sql, &[&limit, &cron_triggers]).await,
        Kind::OneOff => client.query(&sql, &[&limit]).await,
    }
    .map_err(pg_err)?;
    Ok(rows
        .into_iter()
        .map(|r| ScheduledEvent {
            kind,
            id: r.get(0),
            scheduled_epoch: r.get(1),
            scheduled_time: r.get(2),
            created_at: r.get(3),
            tries: r.get(4),
            trigger_name: r.get(5),
            webhook_conf: r.get(6),
            retry_conf: r.get(7),
            payload: r.get(8),
            header_conf: r.get(9),
            comment: r.get(10),
        })
        .collect())
}

async fn set_status(pool: &Pool, ev: &ScheduledEvent, status: &str) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    client
        .execute(
            &format!(
                "UPDATE hdb_catalog.{} SET status = $2 WHERE id = $1",
                ev.kind.events_table()
            ),
            &[&ev.id, &status],
        )
        .await
        .map_err(pg_err)?;
    Ok(())
}

async fn record(
    pool: &Pool,
    ev: &ScheduledEvent,
    invocation: &InvocationRecord,
    outcome: Option<AfterFailure>,
) -> Result<(), String> {
    {
        let mut client = pool.get().await.map_err(|e| e.to_string())?;
        let tx = client.transaction().await.map_err(pg_err)?;
        tx.execute(
            &format!(
                "INSERT INTO hdb_catalog.{} (event_id, status, request, response) \
                 VALUES ($1, $2, $3::text::json, $4::text::json)",
                ev.kind.invocations_table()
            ),
            &[
                &ev.id,
                &invocation.status,
                &invocation.request,
                &invocation.response,
            ],
        )
        .await
        .map_err(pg_err)?;
        tx.execute(
            &format!(
                "UPDATE hdb_catalog.{} SET tries = tries + 1 WHERE id = $1",
                ev.kind.events_table()
            ),
            &[&ev.id],
        )
        .await
        .map_err(pg_err)?;
        tx.commit().await.map_err(pg_err)?;
    }
    match outcome {
        None => set_status(pool, ev, "delivered").await,
        Some(AfterFailure::SetError) => set_status(pool, ev, "error").await,
        Some(AfterFailure::Retry(secs)) => {
            let client = pool.get().await.map_err(|e| e.to_string())?;
            client
                .execute(
                    &format!(
                        "UPDATE hdb_catalog.{} SET next_retry_at = now() + make_interval(secs => $1::float8), STATUS = 'scheduled' WHERE id = $2",
                        ev.kind.events_table()
                    ),
                    &[&(secs as f64), &ev.id],
                )
                .await
                .map_err(pg_err)?;
            Ok(())
        }
    }
}

/// What one scheduled event is delivered with.
struct Target {
    url: String,
    headers: Vec<ResolvedHeader>,
    retry: ScheduledRetryConf,
    body: Json,
    timeout_message: String,
}

fn cron_target(ev: &ScheduledEvent, info: &CronTriggerInfo) -> Target {
    let mut body = vec![
        ("id", Json::str(&ev.id)),
        ("name", Json::str(&info.conf.name)),
        (
            "payload",
            info.conf
                .payload
                .as_ref()
                .map(Json::from)
                .unwrap_or(Json::Null),
        ),
        ("scheduled_time", Json::str(&ev.scheduled_time)),
    ];
    if let Some(c) = &info.conf.comment {
        body.push(("comment", Json::str(c)));
    }
    Target {
        url: info.webhook.url.clone(),
        headers: info.headers.clone(),
        retry: info.conf.retry_conf,
        body: Json::object(body),
        timeout_message: format!(
            "Cron Scheduled event \"{}\" of cron trigger \"{}\" timed out while processing.",
            ev.id, info.conf.name
        ),
    }
}

fn one_off_retry(ev: &ScheduledEvent) -> ScheduledRetryConf {
    ev.retry_conf
        .as_deref()
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default()
}

/// One-off events resolve their webhook and headers at delivery time. On failure, returns
/// the missing-variables message V2 records as a failed attempt.
fn one_off_target(
    ev: &ScheduledEvent,
    env: &dyn Env,
) -> Result<Target, (ScheduledRetryConf, String)> {
    let retry = one_off_retry(ev);
    let template = match ev
        .webhook_conf
        .as_deref()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
    {
        Some(Value::String(s)) => s,
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let headers: Vec<HeaderConf> = ev
        .header_conf
        .as_deref()
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default();
    let mut missing = vec![];
    let url = resolve_template(&template, env).unwrap_or_else(|m| {
        missing.extend(m);
        String::new()
    });
    let mut resolved = vec![];
    for h in &headers {
        match resolve_headers(std::slice::from_ref(h), env) {
            Ok(mut r) => resolved.append(&mut r),
            Err(_) => match &h.value {
                crate::model::HeaderValue::Env(v) => missing.push(v.clone()),
                crate::model::HeaderValue::Value(t) => {
                    if let Err(m) = resolve_template(t, env) {
                        missing.extend(m);
                    }
                }
            },
        }
    }
    if !missing.is_empty() {
        return Err((
            retry,
            format!(
                "The value for environment variables not found: {}",
                missing.join(", ")
            ),
        ));
    }
    let payload = ev
        .payload
        .as_deref()
        .map(|p| Json::parse(p).unwrap_or(Json::Null))
        .unwrap_or(Json::Null);
    let mut body = vec![
        (
            "created_at",
            ev.created_at
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        ("id", Json::str(&ev.id)),
        ("payload", payload),
        ("scheduled_time", Json::str(&ev.scheduled_time)),
    ];
    if let Some(c) = &ev.comment {
        body.push(("comment", Json::str(c)));
    }
    Ok(Target {
        url,
        headers: resolved,
        retry,
        body: Json::object(body),
        timeout_message: format!(
            "One-off Scheduled event \"{}\" timed out while processing.",
            ev.id
        ),
    })
}

struct Ctx {
    config: EventingConfig,
    client: Arc<dyn WebhookClient>,
    provider: ScheduledProvider,
    env: Arc<dyn Env + Send + Sync>,
    pool: Pool,
}

async fn process(ctx: &Ctx, ev: &ScheduledEvent) -> Result<(), String> {
    let triggers = (ctx.provider)();
    let cron = match ev.kind {
        Kind::Cron => {
            let name = ev.trigger_name.clone().unwrap_or_default();
            let Some(info) = triggers.cron.get(&name) else {
                error!(
                    target: "scheduled-trigger",
                    "could not find cron trigger \"{name}\" in the schema cache"
                );
                // Leave it for another run instead of stranding it `locked` (V2 does).
                return set_status(&ctx.pool, ev, "scheduled").await;
            };
            Some(info)
        }
        Kind::OneOff => None,
    };
    let tolerance = match cron {
        Some(info) => info.conf.retry_conf.tolerance_seconds,
        None => one_off_retry(ev).tolerance_seconds,
    };
    let target = match cron {
        Some(info) => Ok(cron_target(ev, info)),
        None => one_off_target(ev, ctx.env.as_ref()),
    };
    // Too late: dead, without an attempt (measured from `scheduled_time`, like V2). V2 only
    // checks this once the request could be built: an unresolvable one-off event is recorded
    // as a failed attempt instead.
    if target.is_ok() && now_secs() as f64 - ev.scheduled_epoch > tolerance {
        return set_status(&ctx.pool, ev, "dead").await;
    }
    let (retry, result, log_headers, request_payload, url) = match target {
        Err((retry, message)) => {
            // Counts as a failed attempt, with an `{"error": …}` request payload.
            let payload = Json::object([(
                "error",
                Json::String(format!("Error creating the request. {message}")),
            )]);
            (
                retry,
                AttemptResult::Internal(message),
                vec![],
                payload,
                String::new(),
            )
        }
        Ok(t) => {
            let (headers, log_headers) =
                delivery::prepare_headers(&ctx.config.user_agent, &t.headers);
            let body_text = t.body.encode();
            let result = delivery::attempt(
                ctx.client.as_ref(),
                Attempt {
                    url: &t.url,
                    headers,
                    body: body_text,
                    http_timeout: t.retry.http_timeout(),
                    processing_timeout: t.retry.processing_timeout(),
                    timeout_message: t.timeout_message.clone(),
                    trace: TraceIds::for_event(None),
                },
            )
            .await;
            let (payload, log_headers) = match &result {
                AttemptResult::Internal(m) => (Json::object([("error", Json::str(m))]), vec![]),
                _ => (t.body, log_headers),
            };
            (t.retry, result, log_headers, payload, t.url)
        }
    };
    let invocation = delivery::invocation(
        INVOCATION_VERSION_SCHEDULED,
        request_payload,
        &log_headers,
        &result,
        &url,
        retry.http_timeout(),
    );
    let outcome = (!result.is_success()).then(|| {
        delivery::after_failure(
            i64::from(ev.tries),
            retry.num_retries,
            retry.retry_interval(),
            &result,
        )
    });
    if !result.is_success() {
        let msg = format!(
            "scheduled event {} failed: status {:?}",
            ev.id,
            result.status()
        );
        if ctx.config.triggers_error_log_level {
            error!(target: "scheduled-trigger", "{msg}");
        } else {
            info!(target: "scheduled-trigger", "{msg}");
        }
    }
    record(&ctx.pool, ev, &invocation, outcome).await
}

/// Start the cron generator and the scheduled-event delivery loop on the metadata database.
pub fn spawn(
    config: EventingConfig,
    pool: Pool,
    client: Arc<dyn WebhookClient>,
    provider: ScheduledProvider,
    env: Arc<dyn Env + Send + Sync>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    let ctx = Arc::new(Ctx {
        config,
        client,
        provider,
        env,
        pool,
    });
    tokio::spawn(async move {
        if let Err(e) = check_catalog(&ctx.pool).await {
            error!(target: "scheduled-trigger", "scheduled events disabled: {e}");
            return;
        }
        let generator = tokio::spawn(generator_loop(ctx.clone(), shutdown.clone()));
        delivery_loop(ctx, shutdown).await;
        let _ = generator.await;
    })
}

/// Reconcile after startup and every metadata change, and top up cron events every minute
/// (at once for a new metadata version, so new triggers get their first events quickly).
async fn generator_loop(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    let mut current: Option<Arc<ScheduledTriggers>> = None;
    let mut reconciled = false;
    let mut last_run: Option<Instant> = None;
    loop {
        let triggers = (ctx.provider)();
        let changed = !current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &triggers));
        let due = last_run.is_none_or(|t| t.elapsed() >= GENERATOR_INTERVAL);
        if changed || due {
            if changed {
                current = Some(triggers.clone());
                reconciled = false;
            }
            if !reconciled {
                match reconcile_cron_events(&ctx.pool, &triggers).await {
                    Ok(()) => reconciled = true,
                    Err(e) => {
                        error!(target: "scheduled-trigger", "cron event reconciliation: {e}")
                    }
                }
            }
            if let Err(e) = generate_cron_events(&ctx.pool, &triggers).await {
                error!(target: "scheduled-trigger", "cron event generator: {e}");
            }
            last_run = Some(Instant::now());
        }
        tokio::select! {
            _ = tokio::time::sleep(GENERATOR_POLL) => {}
            _ = shutdown.cancelled() => return,
        }
    }
}

/// Events this instance has locked and not finished, by id.
type Held = Arc<Mutex<HashMap<String, Kind>>>;

/// The connection holding this instance's delivery advisory locks; taken out of the pool so
/// the locks are released exactly when it closes (shutdown or a crash).
async fn lock_holder(pool: &Pool) -> Result<Arc<ClientWrapper>, String> {
    let object = pool.get().await.map_err(|e| e.to_string())?;
    Ok(Arc::new(Object::take(object)))
}

/// Every [`DELIVERY_INTERVAL`]: release orphaned locks, then lock as many due events as there
/// are free delivery slots and deliver them. On shutdown, stop fetching, give running
/// deliveries the grace period, and put whatever is unfinished back to `scheduled`.
async fn delivery_loop(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    let sem = Arc::new(Semaphore::new(ctx.config.http_pool_size.max(1)));
    let held: Held = Arc::default();
    let mut tasks = JoinSet::new();
    let mut holder: Option<Arc<ClientWrapper>> = None;
    loop {
        while tasks.try_join_next().is_some() {}
        match unlock_stale(&ctx.pool).await {
            Ok(0) => {}
            Ok(n) => {
                info!(target: "scheduled-trigger", "released {n} events locked by stopped instances")
            }
            Err(e) => {
                warn!(target: "scheduled-trigger", "could not release orphaned scheduled events: {e}")
            }
        }
        if holder.as_ref().is_none_or(|h| h.is_closed()) {
            holder = match lock_holder(&ctx.pool).await {
                Ok(h) => Some(h),
                Err(e) => {
                    error!(target: "scheduled-trigger", "could not connect to the metadata database: {e}");
                    None
                }
            };
        }
        if let Some(h) = &holder {
            let names: Vec<String> = (ctx.provider)().cron.keys().cloned().collect();
            for kind in [Kind::Cron, Kind::OneOff] {
                let free = sem.available_permits() as i64;
                if free == 0 {
                    break;
                }
                if kind == Kind::Cron && names.is_empty() {
                    continue;
                }
                let events = match fetch_due(h, kind, &names, free).await {
                    Ok(e) => e,
                    Err(e) => {
                        error!(target: "scheduled-trigger", "could not fetch scheduled events: {e}");
                        continue;
                    }
                };
                for ev in events {
                    held.lock().expect("lock").insert(ev.id.clone(), kind);
                    let permit = sem.clone().acquire_owned();
                    let (ctx, held, h) = (ctx.clone(), held.clone(), h.clone());
                    tasks.spawn(async move {
                        let _permit = permit.await.expect("semaphore is never closed");
                        if let Err(e) = process(&ctx, &ev).await {
                            error!(target: "scheduled-trigger", event_id = %ev.id, "{e}");
                        }
                        held.lock().expect("lock").remove(&ev.id);
                        let _ = h
                            .execute(
                                "SELECT pg_advisory_unlock($1, hashtext($2))",
                                &[&DELIVERY_LOCK_CLASS, &ev.id],
                            )
                            .await;
                    });
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(DELIVERY_INTERVAL) => {}
            _ = shutdown.cancelled() => break,
        }
    }
    let grace = ctx.config.graceful_shutdown_timeout;
    let drained = tokio::time::timeout(grace, async { while tasks.join_next().await.is_some() {} })
        .await
        .is_ok();
    if !drained {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    let unfinished: Vec<(String, Kind)> = held.lock().expect("lock").drain().collect();
    if !unfinished.is_empty() {
        warn!(
            target: "scheduled-trigger",
            "{} scheduled events still in flight at shutdown, putting them back",
            unfinished.len()
        );
        if let Err(e) = release(&ctx.pool, &unfinished).await {
            error!(target: "scheduled-trigger", "could not release scheduled events: {e}");
        }
    }
    // Dropping the holder closes its connection and with it any remaining advisory locks.
    drop(holder);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cron_conf_defaults() {
        let c: CronTriggerConf = serde_json::from_value(json!({
            "name": "daily", "webhook": "{{HOOK}}/x", "schedule": "0 12 * * 1-5",
            "include_in_metadata": true, "retry_conf": {"num_retries": 2}
        }))
        .unwrap();
        assert_eq!(c.retry_conf.num_retries, 2);
        assert_eq!(c.retry_conf.tolerance_seconds, 21600.0);
        let env = std::collections::BTreeMap::from([("HOOK".to_owned(), "http://h".to_owned())]);
        let info = CronTriggerInfo::resolve(c, &env).unwrap();
        assert_eq!(info.webhook.url, "http://h/x");
        let bad: CronTriggerConf = serde_json::from_value(json!({
            "name": "x", "webhook": "http://h", "schedule": "* * *", "include_in_metadata": true
        }))
        .unwrap();
        assert!(CronTriggerInfo::resolve(bad, &env).is_err());
    }

    #[test]
    fn rounding_is_bankers() {
        let r = ScheduledRetryConf {
            retry_interval_seconds: 2.5,
            ..Default::default()
        };
        assert_eq!(r.retry_interval(), 2);
        let r = ScheduledRetryConf {
            retry_interval_seconds: 3.5,
            ..Default::default()
        };
        assert_eq!(r.retry_interval(), 4);
        assert!(
            ScheduledRetryConf {
                num_retries: -1,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn utc_sql() {
        assert_eq!(
            utc("scheduled_time"),
            "(to_json(scheduled_time AT TIME ZONE 'UTC') #>> '{}') || 'Z'"
        );
    }
}
