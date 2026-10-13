//! The event-trigger delivery loop (V2's `processEventQueue`).
//!
//! Every iteration fetches (and locks, `FOR UPDATE SKIP LOCKED`) a batch per source, so any
//! number of engine replicas can run it against the same database. The next batch is fetched
//! while the current one is dispatched; at most `http_pool_size` deliveries run at once.
//! On shutdown, fetched-but-unstarted events are unlocked at once, in-flight deliveries get
//! `graceful_shutdown_timeout`, and whatever is still locked after that is unlocked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deadpool_postgres::Pool;
use futures::future::join_all;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::delivery::{self, Attempt, AttemptResult, INVOCATION_VERSION_EVENT, TraceIds};
use crate::json::Json;
use crate::store::{self, Event};
use crate::triggers::{EventTriggerInfo, EventTriggers, SourceTriggers};
use crate::webhook::WebhookClient;

/// Event processing settings (`HASURA_GRAPHQL_EVENTS_*`).
#[derive(Debug, Clone)]
pub struct EventingConfig {
    /// `HASURA_GRAPHQL_EVENTS_HTTP_POOL_SIZE` (default 100): concurrent deliveries.
    pub http_pool_size: usize,
    /// `HASURA_GRAPHQL_EVENTS_FETCH_BATCH_SIZE` (default 100); 0 disables delivery.
    pub fetch_batch_size: i64,
    /// `HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL` (default 1 s); 0 disables delivery.
    pub fetch_interval: Duration,
    /// `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT` (default 60 s).
    pub graceful_shutdown_timeout: Duration,
    /// `HASURA_GRAPHQL_DISABLE_EVENTING`: no catalog set-up, no workers.
    pub disable_eventing: bool,
    /// `HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING`: catalogs and SQL triggers are maintained,
    /// but this instance delivers nothing (API-only replicas).
    pub disable_event_processing: bool,
    /// `HASURA_GRAPHQL_ENABLE_TRIGGERS_ERROR_LOG_LEVEL`: failed deliveries logged as errors.
    pub triggers_error_log_level: bool,
    /// `User-Agent` sent with every delivery.
    pub user_agent: String,
}

impl Default for EventingConfig {
    fn default() -> Self {
        EventingConfig {
            http_pool_size: 100,
            fetch_batch_size: 100,
            fetch_interval: Duration::from_millis(1000),
            graceful_shutdown_timeout: Duration::from_secs(60),
            disable_eventing: false,
            disable_event_processing: false,
            triggers_error_log_level: false,
            user_agent: "hasura-graphql-engine".to_owned(),
        }
    }
}

impl EventingConfig {
    /// Whether this instance should run delivery workers.
    pub fn processing_enabled(&self) -> bool {
        !self.disable_eventing
            && !self.disable_event_processing
            && self.fetch_batch_size > 0
            && !self.fetch_interval.is_zero()
    }
}

/// Supplies the current triggers (re-read every iteration, so metadata reloads apply).
pub type TriggersProvider = Arc<dyn Fn() -> Arc<EventTriggers> + Send + Sync>;

/// One lock this instance holds: a generation (the same id can be fetched again once its
/// lock is released, before the previous task has cleaned up) and whether delivery started.
#[derive(Clone, Copy)]
struct Lock {
    generation: u64,
    in_flight: bool,
}

/// Event ids this instance holds locks for, per source.
#[derive(Default)]
struct Locked {
    next_generation: u64,
    sources: HashMap<String, (Pool, HashMap<String, Lock>)>,
}

impl Locked {
    /// Record a fetched event; returns its generation.
    fn add(&mut self, source: &SourceTriggers, id: String) -> u64 {
        self.next_generation += 1;
        let generation = self.next_generation;
        let entry = self
            .sources
            .entry(source.source.clone())
            .or_insert_with(|| (source.pool.clone(), HashMap::new()));
        entry.1.insert(
            id,
            Lock {
                generation,
                in_flight: false,
            },
        );
        generation
    }

    fn start(&mut self, source: &str, id: &str) {
        if let Some(e) = self.sources.get_mut(source).and_then(|s| s.1.get_mut(id)) {
            e.in_flight = true;
        }
    }

    /// Forget a lock, unless the event was fetched again since (a newer generation).
    fn remove(&mut self, source: &str, id: &str, generation: u64) {
        if let Some((_, ids)) = self.sources.get_mut(source)
            && ids.get(id).is_some_and(|l| l.generation == generation)
        {
            ids.remove(id);
        }
    }

    /// Take ids matching `pred(in_flight)`, grouped by source pool.
    fn take(&mut self, pred: impl Fn(bool) -> bool) -> Vec<(String, Pool, Vec<String>)> {
        let mut out = vec![];
        for (name, (pool, ids)) in &mut self.sources {
            let taken: Vec<String> = ids
                .iter()
                .filter(|(_, l)| pred(l.in_flight))
                .map(|(id, _)| id.clone())
                .collect();
            for id in &taken {
                ids.remove(id);
            }
            if !taken.is_empty() {
                out.push((name.clone(), pool.clone(), taken));
            }
        }
        out
    }
}

struct Ctx {
    config: EventingConfig,
    client: Arc<dyn WebhookClient>,
    provider: TriggersProvider,
    locked: Mutex<Locked>,
}

/// Fetched events with their lock generation, and whether any source returned a full batch.
struct Batch {
    events: Vec<(Arc<SourceTriggers>, Event, u64)>,
    any_full: bool,
}

/// Start the delivery loop. Resolves when `shutdown` is cancelled and locks are released.
pub fn spawn(
    config: EventingConfig,
    client: Arc<dyn WebhookClient>,
    provider: TriggersProvider,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    let ctx = Arc::new(Ctx {
        config,
        client,
        provider,
        locked: Mutex::new(Locked::default()),
    });
    tokio::spawn(run(ctx, shutdown))
}

async fn fetch_batch(ctx: Arc<Ctx>) -> Batch {
    let triggers = (ctx.provider)();
    let fetches = triggers
        .sources
        .iter()
        .filter(|s| !s.triggers.is_empty())
        .map(|s| {
            let s = s.clone();
            let limit = ctx.config.fetch_batch_size;
            async move {
                let names = s.trigger_names();
                match store::fetch_events(&s.pool, limit, &names).await {
                    Ok(events) => (s, events),
                    Err(e) => {
                        error!(
                            target: "event-trigger",
                            source = %s.source,
                            "something went wrong while fetching events: {e}"
                        );
                        (s, vec![])
                    }
                }
            }
        });
    let fetched = join_all(fetches).await;
    let mut locked = ctx.locked.lock().expect("lock");
    let mut batch = Batch {
        events: vec![],
        any_full: false,
    };
    for (s, events) in fetched {
        batch.any_full |= events.len() as i64 >= ctx.config.fetch_batch_size;
        for e in events {
            let generation = locked.add(&s, e.id.clone());
            batch.events.push((s.clone(), e, generation));
        }
    }
    batch
}

async fn run(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    let pool_size = ctx.config.http_pool_size.max(1);
    let sem = Arc::new(Semaphore::new(pool_size));
    let mut batch = fetch_batch(ctx.clone()).await;
    let mut full_fetches = 0u32;
    let mut warned = false;
    'outer: loop {
        if shutdown.is_cancelled() {
            break;
        }
        if batch.events.is_empty() {
            tokio::select! {
                _ = tokio::time::sleep(ctx.config.fetch_interval) => {}
                _ = shutdown.cancelled() => break,
            }
        }
        let prefetch = tokio::spawn(fetch_batch(ctx.clone()));
        let was_full = batch.any_full;
        for (source, event, generation) in batch.events.drain(..) {
            let permit = tokio::select! {
                p = sem.clone().acquire_owned() => p.expect("semaphore is never closed"),
                _ = shutdown.cancelled() => {
                    // Unstarted events of this batch and the prefetched one are unlocked
                    // below; wait for the prefetch so its locks are known.
                    let _ = prefetch.await;
                    break 'outer;
                }
            };
            ctx.locked
                .lock()
                .expect("lock")
                .start(&source.source, &event.id);
            let ctx = ctx.clone();
            tokio::spawn(async move {
                process_event(&ctx, &source, &event).await;
                ctx.locked
                    .lock()
                    .expect("lock")
                    .remove(&source.source, &event.id, generation);
                drop(permit);
            });
        }
        batch = match prefetch.await {
            Ok(b) => b,
            Err(e) => {
                error!(target: "event-trigger", "event fetch task failed: {e}");
                Batch {
                    events: vec![],
                    any_full: false,
                }
            }
        };
        if was_full {
            if full_fetches >= 3 && !warned {
                warn!(
                    target: "event-trigger",
                    "Events processor may not be keeping up with events generated in postgres, or we're working on a backlog of events. Consider increasing HASURA_GRAPHQL_EVENTS_HTTP_POOL_SIZE"
                );
                warned = true;
            }
            full_fetches += 1;
        } else {
            if warned {
                warn!(target: "event-trigger", "It looks like the events processor is keeping up again.");
            }
            full_fetches = 0;
            warned = false;
        }
    }
    shutdown_unlock(&ctx, &sem, pool_size).await;
}

async fn unlock_all(groups: Vec<(String, Pool, Vec<String>)>) {
    for (source, pool, ids) in groups {
        match store::unlock(&pool, &ids).await {
            Ok(n) => info!(
                target: "event-trigger",
                "{n} events for source {source} were successfully unlocked"
            ),
            Err(e) => {
                error!(target: "event-trigger", "could not unlock events for source {source}: {e}")
            }
        }
    }
}

async fn shutdown_unlock(ctx: &Ctx, sem: &Arc<Semaphore>, pool_size: usize) {
    // Never started: release now so another replica can take them immediately.
    let idle = ctx
        .locked
        .lock()
        .expect("lock")
        .take(|in_flight| !in_flight);
    unlock_all(idle).await;
    let all = u32::try_from(pool_size).unwrap_or(u32::MAX);
    let waited =
        tokio::time::timeout(ctx.config.graceful_shutdown_timeout, sem.acquire_many(all)).await;
    if waited.is_err() {
        warn!(target: "event-trigger", "graceful shutdown timed out with deliveries in flight");
    }
    let rest = ctx.locked.lock().expect("lock").take(|_| true);
    unlock_all(rest).await;
}

/// Process one locked event: deliver it and record the outcome.
async fn process_event(ctx: &Ctx, source: &SourceTriggers, event: &Event) {
    // Use the trigger definition of the *current* metadata.
    let current = (ctx.provider)();
    let info = current
        .source(&source.source)
        .and_then(|s| s.triggers.get(&event.trigger_name))
        .filter(|t| t.table.schema == event.schema_name && t.table.name == event.table_name)
        .cloned();
    let Some(info) = info else {
        error!(
            target: "event-trigger",
            event_id = %event.id,
            "event trigger '{}' on table '{}.{}' not found",
            event.trigger_name, event.schema_name, event.table_name
        );
        if let Err(e) = store::postpone(&source.pool, &event.id, 60).await {
            error!(target: "event-trigger", "{e}");
        }
        return;
    };
    if let Err(e) = deliver(ctx, &source.pool, &info, event).await {
        error!(target: "event-trigger", event_id = %event.id, "{e}");
    }
}

/// The webhook body (`EventPayload`), keys sorted like V2.
pub fn event_body(info: &EventTriggerInfo, event: &Event, stored: Json) -> Json {
    Json::object([
        (
            "created_at",
            event
                .created_at
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "delivery_info",
            Json::object([
                ("current_retry", Json::int(i64::from(event.tries))),
                ("max_retries", Json::int(info.retry_conf.num_retries)),
            ]),
        ),
        ("event", stored),
        ("id", Json::str(&event.id)),
        (
            "table",
            Json::object([
                ("name", Json::str(&event.table_name)),
                ("schema", Json::str(&event.schema_name)),
            ]),
        ),
        ("trigger", Json::object([("name", Json::str(&info.name))])),
    ])
}

async fn deliver(
    ctx: &Ctx,
    pool: &Pool,
    info: &EventTriggerInfo,
    event: &Event,
) -> Result<(), String> {
    let stored = match Json::parse(&event.payload) {
        Ok(v) => v,
        Err(e) => {
            // Unreadable payload: never deliverable, so fail it rather than leaving it locked.
            store::set_error(pool, &event.id).await?;
            return Err(format!("invalid event payload, marked as error: {e}"));
        }
    };
    let trace = TraceIds::for_event(stored.get("trace_context"));
    let body = event_body(info, event, stored);
    let body_text = body.encode();
    let (headers, log_headers) = delivery::prepare_headers(&ctx.config.user_agent, &info.headers);
    let http_timeout = Duration::from_secs(info.timeout_seconds());
    let timeout_message = format!(
        "Event Trigger \"{}\" timed out while processing.",
        info.name
    );
    let result = delivery::attempt(
        ctx.client.as_ref(),
        Attempt {
            url: &info.webhook.url,
            headers,
            body: body_text,
            http_timeout,
            processing_timeout: info.processing_timeout(),
            timeout_message,
            trace,
        },
    )
    .await;
    let request_payload = match &result {
        AttemptResult::Internal(_) => Json::Null,
        _ => body,
    };
    log_delivery(ctx, info, event, &result);
    let invocation = delivery::invocation(
        INVOCATION_VERSION_EVENT,
        request_payload,
        &log_headers,
        &result,
        &info.webhook.url,
        http_timeout,
    );
    let outcome = (!result.is_success()).then(|| {
        delivery::after_failure(
            i64::from(event.tries),
            info.retry_conf.num_retries,
            info.retry_conf.interval_sec,
            &result,
        )
    });
    store::record_attempt(pool, event, &invocation, outcome).await
}

fn log_delivery(ctx: &Ctx, info: &EventTriggerInfo, event: &Event, result: &AttemptResult) {
    let status = result.status();
    let detail = match result {
        AttemptResult::Success(_) => "delivered".to_owned(),
        AttemptResult::Status(r) => format!("webhook returned status {}", r.status),
        AttemptResult::Client(e) => format!("{e:?}"),
        AttemptResult::Internal(m) => m.clone(),
    };
    if !result.is_success() && ctx.config.triggers_error_log_level {
        error!(
            target: "event-trigger",
            event_id = %event.id,
            event_name = %info.name,
            url = %info.webhook.display,
            ?status,
            "{detail}"
        );
    } else {
        info!(
            target: "event-trigger",
            event_id = %event.id,
            event_name = %info.name,
            url = %info.webhook.display,
            ?status,
            "{detail}"
        );
    }
}
