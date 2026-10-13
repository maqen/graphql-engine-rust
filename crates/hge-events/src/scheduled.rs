//! Cron triggers and one-off scheduled events (V2's `ScheduledTrigger.hs`).
//!
//! Both live in the metadata database (`hdb_catalog.hdb_cron_events`,
//! `hdb_scheduled_events` and their invocation logs). A generator keeps 100 upcoming events
//! per cron trigger; a delivery loop locks due events with `FOR UPDATE SKIP LOCKED`, so any
//! number of replicas can run both.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deadpool_postgres::Pool;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
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

/// Cron and one-off event tables (from V2's `initialise.sql`).
pub const SCHEDULED_CATALOG: &str = include_str!("sql/scheduled_catalog.sql");

/// Upcoming events kept per cron trigger.
pub const CRON_EVENTS_AHEAD: usize = 100;
const GENERATOR_INTERVAL: Duration = Duration::from_secs(60);
const DELIVERY_INTERVAL: Duration = Duration::from_secs(10);
/// Due events locked per table and iteration (V2 has no limit; this bounds a backlog).
const FETCH_LIMIT: i64 = 1000;

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
    pub cron: HashMap<String, Arc<CronTriggerInfo>>,
}

pub type ScheduledProvider = Arc<dyn Fn() -> Arc<ScheduledTriggers> + Send + Sync>;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Create the cron / one-off tables in the metadata database if they are missing.
pub async fn ensure_catalog(pool: &Pool) -> Result<(), String> {
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(pg_err)?;
    tx.execute(
        "SELECT pg_advisory_xact_lock($1)",
        &[&0x4855_5241_5343_4844_i64],
    )
    .await
    .map_err(pg_err)?;
    let present: bool = tx
        .query_one(
            "SELECT to_regclass('hdb_catalog.hdb_cron_events') IS NOT NULL",
            &[],
        )
        .await
        .map_err(pg_err)?
        .get(0);
    if !present {
        tx.batch_execute(
            "CREATE SCHEMA IF NOT EXISTS hdb_catalog;
             CREATE OR REPLACE FUNCTION hdb_catalog.gen_hasura_uuid() RETURNS uuid AS
             'select gen_random_uuid()' LANGUAGE SQL;",
        )
        .await
        .map_err(pg_err)?;
        tx.batch_execute(SCHEDULED_CATALOG).await.map_err(pg_err)?;
        info!(target: "startup", "created the scheduled events catalog");
    }
    tx.commit().await.map_err(pg_err)?;
    Ok(())
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
    let names: Vec<String> = triggers.cron.keys().cloned().collect();
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

/// Release events locked by instances that died. V2 unlocks *every* locked event at startup,
/// which double-delivers when replicas start while others are delivering; here only events
/// that cannot still be in flight (due more than the 30-minute processing bound ago) are
/// released.
pub async fn unlock_stale(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    for table in ["hdb_cron_events", "hdb_scheduled_events"] {
        client
            .execute(
                &format!(
                    "UPDATE hdb_catalog.{table} SET status = 'scheduled' WHERE status = 'locked' \
                     AND coalesce(next_retry_at, scheduled_time) < now() - interval '35 minutes'"
                ),
                &[],
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
    fn events_table(self) -> &'static str {
        match self {
            Kind::Cron => "hdb_cron_events",
            Kind::OneOff => "hdb_scheduled_events",
        }
    }

    fn invocations_table(self) -> &'static str {
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

/// Lock due events of one kind.
pub async fn fetch_due(
    pool: &Pool,
    kind: Kind,
    cron_triggers: &[String],
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
SELECT id, extract(epoch FROM scheduled_time)::float8, {st}, {ca}, tries, {extra} FROM cte",
        st = utc("scheduled_time"),
        ca = utc("created_at"),
    );
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let rows = match kind {
        Kind::Cron => client.query(&sql, &[&FETCH_LIMIT, &cron_triggers]).await,
        Kind::OneOff => client.query(&sql, &[&FETCH_LIMIT]).await,
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
    // Too late: dead, without an attempt (measured from `scheduled_time`, like V2).
    if now_secs() as f64 - ev.scheduled_epoch > tolerance {
        return set_status(&ctx.pool, ev, "dead").await;
    }
    let target = match cron {
        Some(info) => Ok(cron_target(ev, info)),
        None => one_off_target(ev, ctx.env.as_ref()),
    };
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
        if let Err(e) = ensure_catalog(&ctx.pool).await {
            error!(target: "scheduled-trigger", "scheduled events disabled: {e}");
            return;
        }
        if let Err(e) = unlock_stale(&ctx.pool).await {
            warn!(target: "scheduled-trigger", "could not unlock stale scheduled events: {e}");
        }
        let generator = tokio::spawn(generator_loop(ctx.clone(), shutdown.clone()));
        delivery_loop(ctx, shutdown).await;
        let _ = generator.await;
    })
}

async fn generator_loop(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    let mut reconciled: Option<Arc<ScheduledTriggers>> = None;
    loop {
        let triggers = (ctx.provider)();
        // After startup and after every metadata change.
        if !reconciled
            .as_ref()
            .is_some_and(|r| Arc::ptr_eq(r, &triggers))
        {
            match reconcile_cron_events(&ctx.pool, &triggers).await {
                Ok(()) => reconciled = Some(triggers.clone()),
                Err(e) => error!(target: "scheduled-trigger", "cron event reconciliation: {e}"),
            }
        }
        if let Err(e) = generate_cron_events(&ctx.pool, &triggers).await {
            error!(target: "scheduled-trigger", "cron event generator: {e}");
        }
        tokio::select! {
            _ = tokio::time::sleep(GENERATOR_INTERVAL) => {}
            _ = shutdown.cancelled() => return,
        }
    }
}

/// Run one delivery iteration: lock due events and deliver them (bounded concurrency).
async fn deliver_due(ctx: &Arc<Ctx>, sem: &Arc<Semaphore>) {
    let names: Vec<String> = (ctx.provider)().cron.keys().cloned().collect();
    for kind in [Kind::Cron, Kind::OneOff] {
        if kind == Kind::Cron && names.is_empty() {
            continue;
        }
        let events = match fetch_due(&ctx.pool, kind, &names).await {
            Ok(e) => e,
            Err(e) => {
                error!(target: "scheduled-trigger", "could not fetch scheduled events: {e}");
                continue;
            }
        };
        let tasks = events.into_iter().map(|ev| {
            let ctx = ctx.clone();
            let sem = sem.clone();
            async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .expect("semaphore is never closed");
                if let Err(e) = process(&ctx, &ev).await {
                    error!(target: "scheduled-trigger", event_id = %ev.id, "{e}");
                }
            }
        });
        join_all(tasks).await;
    }
}

async fn delivery_loop(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    let sem = Arc::new(Semaphore::new(ctx.config.http_pool_size.max(1)));
    loop {
        // An iteration finishes its deliveries even during shutdown (bounded by the
        // processing timeouts); the server's grace period caps the wait.
        tokio::select! {
            _ = deliver_due(&ctx, &sem) => {}
            _ = shutdown.cancelled() => {
                let _ = tokio::time::timeout(ctx.config.graceful_shutdown_timeout, deliver_wait(&sem, ctx.config.http_pool_size)).await;
                return;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(DELIVERY_INTERVAL) => {}
            _ = shutdown.cancelled() => return,
        }
    }
}

async fn deliver_wait(sem: &Semaphore, n: usize) {
    let _ = sem
        .acquire_many(u32::try_from(n.max(1)).unwrap_or(u32::MAX))
        .await;
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
