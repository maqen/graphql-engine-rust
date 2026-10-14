//! Asynchronous actions (spec 06 §9): the mutation records the call in the action log, a
//! background processor calls the handler and stores the outcome, and the
//! `<action>(id: uuid!)` query reads it back.
//!
//! Storage is behind [`ActionLogStore`] (V2: `hdb_catalog.hdb_action_log` in the metadata
//! database), so this crate stays backend-agnostic.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hge_core::{Error, Session, SessionVariables};
use hge_ir::BoxFuture;
use hge_ir::action::{ActionOutput, ActionSelection, AsyncField};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::registry::ActionRegistry;
use crate::{RequestContext, Sources, project, webhook};

/// A call to record (`status = 'created'`).
pub struct NewActionLog<'a> {
    pub action_name: &'a str,
    pub input_payload: &'a Value,
    pub session_variables: Value,
    pub request_headers: Value,
}

/// A row claimed by the processor (`status = 'processing'`).
#[derive(Debug, Clone)]
pub struct PendingAction {
    pub id: String,
    /// `NULL` in the log is treated like an action that no longer exists.
    pub action_name: Option<String>,
    pub request_headers: Value,
    pub session_variables: Value,
    pub input_payload: Value,
}

/// A row as read by the async query.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionLogEntry {
    pub id: String,
    pub created_at: UtcTimestamp,
    pub response_payload: Option<Value>,
    pub errors: Option<Value>,
    pub session_variables: Value,
}

impl ActionLogEntry {
    /// The handler has been called (successfully or not).
    pub fn is_finished(&self) -> bool {
        self.response_payload.is_some() || self.errors.is_some()
    }
}

/// A `timestamptz` in UTC with microsecond precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtcTimestamp {
    /// `YYYY-MM-DDTHH:MM:SS`
    pub date_time: String,
    pub micros: u32,
}

impl UtcTimestamp {
    /// Parse `YYYY-MM-DDTHH:MM:SS[.ffffff]`.
    pub fn parse(s: &str) -> Option<Self> {
        let (date_time, frac) = match s.split_once('.') {
            Some((dt, f)) => (dt, f),
            None => (s, ""),
        };
        if date_time.len() < 19 || frac.len() > 6 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let micros = format!("{frac:0<6}").parse().ok()?;
        Some(UtcTimestamp {
            date_time: date_time.to_owned(),
            micros,
        })
    }

    /// The fraction of a second with trailing zeros dropped (`.5`, `.00012`), or nothing.
    fn fraction(&self) -> String {
        if self.micros == 0 {
            return String::new();
        }
        let digits = format!("{:06}", self.micros);
        format!(".{}", digits.trim_end_matches('0'))
    }

    /// aeson's `UTCTime` encoding (`2024-01-02T03:04:05.12345Z`): aeson 2.3's `timeOfDay64`
    /// drops every trailing zero of the fraction, and the fraction itself when zero.
    pub fn to_aeson(&self) -> String {
        format!("{}{}Z", self.date_time, self.fraction())
    }

    /// Postgres' JSON encoding of a `timestamptz` in a UTC session
    /// (`2024-01-02T03:04:05.1234+00:00`: trailing zeros of the fraction dropped).
    pub fn to_postgres_json(&self) -> String {
        format!("{}{}+00:00", self.date_time, self.fraction())
    }
}

/// The action log. Implementations must claim rows so that several engines can poll the
/// same database (`FOR UPDATE SKIP LOCKED`).
pub trait ActionLogStore: Send + Sync {
    /// Insert a `created` row and return its id.
    fn insert<'a>(&'a self, entry: NewActionLog<'a>) -> BoxFuture<'a, Result<String, Error>>;

    /// Claim up to `limit` `created` rows, marking them `processing`.
    fn claim(&self, limit: u32) -> BoxFuture<'_, Result<Vec<PendingAction>, Error>>;

    /// Store the handler's response (`completed`).
    fn complete<'a>(&'a self, id: &'a str, response: &'a Value)
    -> BoxFuture<'a, Result<(), Error>>;

    /// Store the error (`error`).
    fn fail<'a>(&'a self, id: &'a str, errors: &'a Value) -> BoxFuture<'a, Result<(), Error>>;

    /// Put rows still `processing` back to `created` (graceful shutdown).
    fn release<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, Result<(), Error>>;

    /// Read one row.
    fn fetch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<ActionLogEntry>, Error>>;
}

/// The request headers stored with an async call (`request_headers`): one value per name,
/// the last one winning. V2 stores every header; `x-hasura-*` headers (the admin secret,
/// session variables) are never forwarded, so they are not stored.
pub fn stored_request_headers(headers: &HeaderMap) -> Value {
    let mut out = Map::new();
    for (k, v) in webhook::header_pairs(headers) {
        if !hge_core::session::is_session_variable(&k) {
            out.insert(k, Value::String(v));
        }
    }
    Value::Object(out)
}

/// W3C and B3 trace headers (V2's `Tracing.isTraceHeader`), stripped before forwarding.
fn is_trace_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(name.as_str(), "traceparent" | "tracestate" | "b3") || name.starts_with("x-b3-")
}

fn headers_from_log(stored: &Value) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in stored.as_object().into_iter().flatten() {
        let (Some(v), false) = (v.as_str(), is_trace_header(k)) else {
            continue;
        };
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            out.append(k, v);
        }
    }
    out
}

fn session_from_log(stored: &Value) -> SessionVariables {
    let mut vars = SessionVariables::new();
    for (k, v) in stored.as_object().into_iter().flatten() {
        if let Some(v) = v.as_str() {
            vars.insert(k, v);
        }
    }
    vars
}

/// The async mutation: record the call and return its id.
pub(crate) async fn submit(
    action: &str,
    input: &Value,
    ctx: &RequestContext<'_>,
) -> Result<Value, Error> {
    let store = ctx
        .action_log
        .ok_or_else(|| Error::unexpected("asynchronous actions need a metadata database"))?;
    let id = store
        .insert(NewActionLog {
            action_name: action,
            input_payload: input,
            session_variables: ctx.session.variables.to_json(),
            request_headers: stored_request_headers(ctx.client_headers),
        })
        .await?;
    Ok(Value::String(id))
}

/// The current value of an async query field, and whether the action has finished.
#[derive(Debug)]
pub struct AsyncQueryPoll {
    pub value: Value,
    pub finished: bool,
}

/// Evaluate `<action>(id: uuid!)` against the action log.
///
/// Subscriptions hook: an async action subscription (spec §9.5) can poll this every second
/// with the subscriber's [`RequestContext`] and send `value` on each poll, completing the
/// operation once `finished` is true. The subscription layer must reject relationship
/// fields inside `output` beforehand (`Remote relationships are not allowed in
/// subscriptions`, 400 `not-supported`).
pub async fn poll_async_query(
    action: &str,
    id: &str,
    output: &ActionOutput,
    fields: &[(String, AsyncField)],
    has_relationships: bool,
    ctx: &RequestContext<'_>,
    sources: &dyn Sources,
) -> Result<AsyncQueryPoll, Error> {
    let store = ctx
        .action_log
        .ok_or_else(|| Error::unexpected("asynchronous actions need a metadata database"))?;
    // V2 expects exactly one row (`PG.getRow`).
    let not_found = || {
        Error::unexpected("database query error").with_internal(json!({
            "error": {"message": "Rows returned != 1"},
            "action": action,
        }))
    };
    // `id` was checked to be a uuid when the field was lowered.
    let row = store.fetch(id).await?.ok_or_else(not_found)?;
    let finished = row.is_finished();
    // With relationships V2 reads the log through SQL on the relationship's source, which
    // only returns the row to the session that created it (exact JSON equality; quirk Q6).
    if has_relationships
        && !ctx.session.is_admin()
        && row.session_variables != ctx.session.variables.to_json()
    {
        return Ok(AsyncQueryPoll {
            value: Value::Null,
            finished,
        });
    }
    let mut out = Map::with_capacity(fields.len());
    for (alias, f) in fields {
        let v = match f {
            AsyncField::Typename(t) => Value::String(t.clone()),
            AsyncField::Id => Value::String(row.id.clone()),
            AsyncField::CreatedAt => Value::String(if has_relationships {
                row.created_at.to_postgres_json()
            } else {
                row.created_at.to_aeson()
            }),
            AsyncField::Errors => match &row.errors {
                None | Some(Value::Null) => Value::Null,
                Some(e) if has_relationships => {
                    let mut e = e.clone();
                    if !ctx.include_internal
                        && let Some(o) = e.as_object_mut()
                    {
                        o.remove("internal");
                    }
                    e
                }
                Some(e) => reencode_error(e, ctx.include_internal),
            },
            AsyncField::Output(selection) => match &row.response_payload {
                None | Some(Value::Null) => Value::Null,
                Some(payload) => {
                    // With relationships V2 reads the output through `jsonb_to_record`, where a
                    // missing nested object is null.
                    let missing = if has_relationships {
                        project::Missing::Null
                    } else {
                        project::Missing::Omit
                    };
                    output_value(output, selection, payload, missing, ctx.session, sources).await?
                }
            },
        };
        out.insert(alias.clone(), v);
    }
    Ok(AsyncQueryPoll {
        value: Value::Object(out),
        finished,
    })
}

async fn output_value(
    output: &ActionOutput,
    selection: &ActionSelection,
    payload: &Value,
    missing: project::Missing,
    session: &Session,
    sources: &dyn Sources,
) -> Result<Value, Error> {
    let mut joins = project::Joins::default();
    project::collect_joins(output, selection, payload, &mut joins);
    let joined = if joins.is_empty() {
        project::Joined::new()
    } else {
        crate::fetch_joins(&joins, session, sources).await?
    };
    Ok(project::project(
        output,
        selection,
        payload.clone(),
        &joined,
        missing,
    ))
}

/// The stored error decoded as V2's `QErr` and encoded again (keys sorted, as aeson does).
fn reencode_error(stored: &Value, include_internal: bool) -> Value {
    let text = |k: &str| stored.get(k).and_then(Value::as_str);
    let mut out = Map::new();
    out.insert("code".into(), json!(text("code").unwrap_or("unexpected")));
    out.insert(
        "error".into(),
        json!(text("error").unwrap_or("internal: error in parsing the action log")),
    );
    if include_internal && let Some(i) = stored.get("internal") {
        out.insert("internal".into(), i.clone());
    }
    out.insert("path".into(), json!(text("path").unwrap_or("$")));
    Value::Object(out)
}

/// Settings of the async action processor.
#[derive(Debug, Clone)]
pub struct ProcessorConfig {
    /// `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL`; zero disables processing.
    pub fetch_interval: Duration,
    /// `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_BATCH_SIZE`
    pub batch_size: u32,
    /// `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT`: how long in-flight calls may finish.
    pub graceful_shutdown_timeout: Duration,
}

impl Default for ProcessorConfig {
    fn default() -> Self {
        ProcessorConfig {
            fetch_interval: Duration::from_millis(1000),
            batch_size: 10,
            graceful_shutdown_timeout: Duration::from_secs(60),
        }
    }
}

/// Call the handler of one claimed row and store the outcome.
async fn process(registry: &ActionRegistry, store: &dyn ActionLogStore, item: &PendingAction) {
    // A dropped action leaves its rows `processing` (quirk Q8).
    let Some(handler) = item.action_name.as_deref().and_then(|n| registry.get(n)) else {
        return;
    };
    let headers = headers_from_log(&item.request_headers);
    let session = session_from_log(&item.session_variables);
    let call = webhook::Call {
        input: &item.input_payload,
        session: &session,
        request_query: None,
        client_headers: &headers,
    };
    let result = webhook::call_detailed(
        &registry.config.client,
        handler,
        &registry.config,
        registry.env.as_ref(),
        &call,
    )
    .await;
    let stored = match result {
        Ok(resp) => store.complete(&item.id, &resp.body).await,
        Err(e) => store.fail(&item.id, &e.log_json()).await,
    };
    if let Err(e) = stored {
        warn!(target: "async-actions", id = %item.id, "could not store the action result: {e}");
    }
}

/// Claim and process one batch; returns the number of rows claimed. For running the
/// processor out of process (one pass per trigger) and for tests.
pub async fn process_batch(
    registry: &ActionRegistry,
    store: &dyn ActionLogStore,
    batch_size: u32,
) -> Result<usize, Error> {
    let items = store.claim(batch_size).await?;
    futures::future::join_all(items.iter().map(|item| process(registry, store, item))).await;
    Ok(items.len())
}

/// The processor loop (V2's `asyncActionsProcessor`). Runs until `shutdown` is cancelled;
/// calls still running then get `graceful_shutdown_timeout` to
/// finish, and the rows of those that do not are put back to `created`.
pub async fn run_processor(
    registry: impl Fn() -> Arc<ActionRegistry> + Send + Sync,
    store: Arc<dyn ActionLogStore>,
    config: ProcessorConfig,
    shutdown: CancellationToken,
) {
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let interval = if config.fetch_interval.is_zero() {
            Duration::from_secs(1)
        } else {
            let registry = registry();
            if registry.has_async_actions() {
                let items = store.claim(config.batch_size).await.unwrap_or_else(|e| {
                    warn!(target: "async-actions", "could not fetch async actions: {e}");
                    vec![]
                });
                let locked: Mutex<HashSet<&str>> =
                    Mutex::new(items.iter().map(|i| i.id.as_str()).collect());
                let work = futures::future::join_all(items.iter().map(|item| async {
                    process(&registry, store.as_ref(), item).await;
                    locked.lock().expect("lock").remove(item.id.as_str());
                }));
                tokio::pin!(work);
                tokio::select! {
                    _ = &mut work => {}
                    _ = shutdown.cancelled() => {
                        let _ = tokio::time::timeout(config.graceful_shutdown_timeout, &mut work).await;
                        let remaining: Vec<String> =
                            locked.lock().expect("lock").iter().map(|s| (*s).to_owned()).collect();
                        if !remaining.is_empty()
                            && let Err(e) = store.release(&remaining).await
                        {
                            warn!(target: "async-actions", "could not release async actions: {e}");
                        }
                        return;
                    }
                }
            }
            config.fetch_interval
        };
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::HandlerConfig;
    use hge_ir::action::{OutputBase, OutputType};

    /// An in-memory action log: `(row, status)`.
    #[derive(Default)]
    struct MemoryLog(Mutex<Vec<(PendingAction, &'static str, Option<Value>)>>);

    impl MemoryLog {
        fn status(&self, id: &str) -> (&'static str, Option<Value>) {
            let rows = self.0.lock().unwrap();
            let (_, status, stored) = rows.iter().find(|(r, _, _)| r.id == id).unwrap();
            (status, stored.clone())
        }
    }

    impl ActionLogStore for MemoryLog {
        fn insert<'a>(&'a self, e: NewActionLog<'a>) -> BoxFuture<'a, Result<String, Error>> {
            let mut rows = self.0.lock().unwrap();
            let id = format!("id-{}", rows.len());
            rows.push((
                PendingAction {
                    id: id.clone(),
                    action_name: Some(e.action_name.to_owned()),
                    request_headers: e.request_headers,
                    session_variables: e.session_variables,
                    input_payload: e.input_payload.clone(),
                },
                "created",
                None,
            ));
            Box::pin(async move { Ok(id) })
        }
        fn claim(&self, limit: u32) -> BoxFuture<'_, Result<Vec<PendingAction>, Error>> {
            let mut rows = self.0.lock().unwrap();
            let claimed = rows
                .iter_mut()
                .filter(|(_, s, _)| *s == "created")
                .take(limit as usize)
                .map(|(r, s, _)| {
                    *s = "processing";
                    r.clone()
                })
                .collect();
            Box::pin(async move { Ok(claimed) })
        }
        fn complete<'a>(&'a self, id: &'a str, v: &'a Value) -> BoxFuture<'a, Result<(), Error>> {
            self.set(id, "completed", v)
        }
        fn fail<'a>(&'a self, id: &'a str, v: &'a Value) -> BoxFuture<'a, Result<(), Error>> {
            self.set(id, "error", v)
        }
        fn release<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, Result<(), Error>> {
            for (r, s, _) in self.0.lock().unwrap().iter_mut() {
                if *s == "processing" && ids.contains(&r.id) {
                    *s = "created";
                }
            }
            Box::pin(async { Ok(()) })
        }
        fn fetch<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Option<ActionLogEntry>, Error>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl MemoryLog {
        fn set<'a>(
            &'a self,
            id: &str,
            status: &'static str,
            v: &Value,
        ) -> BoxFuture<'a, Result<(), Error>> {
            for (r, s, stored) in self.0.lock().unwrap().iter_mut() {
                if r.id == id {
                    *s = status;
                    *stored = Some(v.clone());
                }
            }
            Box::pin(async { Ok(()) })
        }
    }

    async fn handler_url() -> String {
        use axum::routing::post;
        let app = axum::Router::new()
            .route("/ok", post(|| async { axum::Json(json!({"v": 1})) }))
            .route(
                "/slow",
                post(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    axum::Json(json!({}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}")
    }

    fn registry(base: &str) -> Arc<ActionRegistry> {
        let actions = ["ok", "slow"].map(|n| {
            json!({"name": n, "definition": {
                "kind": "asynchronous", "output_type": "Out", "handler": format!("{base}/{n}")}})
        });
        let metadata =
            hge_metadata::from_json(json!({"version": 3, "sources": [], "actions": actions}))
                .unwrap();
        let output = Arc::new(ActionOutput {
            ty: OutputType::Named("Out".into()),
            base: OutputBase::Object { required: vec![] },
        });
        let outputs = ["ok", "slow"]
            .map(|n| (n.to_owned(), output.clone()))
            .into_iter()
            .collect();
        let built = ActionRegistry::build(
            &metadata,
            &outputs,
            Arc::new(crate::ProcessEnv),
            HandlerConfig::default(),
        );
        Arc::new(built.registry)
    }

    fn submit_to(log: &MemoryLog, action: &str) -> String {
        let input = json!({});
        let fut = log.insert(NewActionLog {
            action_name: action,
            input_payload: &input,
            session_variables: json!({"x-hasura-role": "admin"}),
            request_headers: json!({}),
        });
        futures::executor::block_on(fut).unwrap()
    }

    async fn wait_for(log: &MemoryLog, id: &str, status: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while log.status(id).0 != status {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{id} never became {status}"));
    }

    #[tokio::test]
    async fn processor_stores_results_and_releases_on_shutdown() {
        let base = handler_url().await;
        let log = Arc::new(MemoryLog::default());
        let reg = registry(&base);
        let config = ProcessorConfig {
            fetch_interval: Duration::from_millis(5),
            batch_size: 10,
            graceful_shutdown_timeout: Duration::from_millis(50),
        };
        let token = CancellationToken::new();
        let store: Arc<dyn ActionLogStore> = log.clone();
        let task = tokio::spawn(run_processor(
            move || reg.clone(),
            store,
            config,
            token.clone(),
        ));

        let ok = submit_to(&log, "ok");
        wait_for(&log, &ok, "completed").await;
        assert_eq!(log.status(&ok).1, Some(json!({"v": 1})));
        // Rows of actions that no longer exist stay `processing` (quirk Q8).
        let gone = submit_to(&log, "dropped");
        wait_for(&log, &gone, "processing").await;

        let slow = submit_to(&log, "slow");
        wait_for(&log, &slow, "processing").await;
        token.cancel();
        task.await.unwrap();
        assert_eq!(log.status(&slow).0, "created");
        assert_eq!(log.status(&gone).0, "processing");
    }

    #[tokio::test]
    async fn a_zero_fetch_interval_disables_processing() {
        let base = handler_url().await;
        let log = Arc::new(MemoryLog::default());
        let reg = registry(&base);
        let config = ProcessorConfig {
            fetch_interval: Duration::ZERO,
            ..ProcessorConfig::default()
        };
        let token = CancellationToken::new();
        let store: Arc<dyn ActionLogStore> = log.clone();
        let task = tokio::spawn(run_processor(
            move || reg.clone(),
            store,
            config,
            token.clone(),
        ));
        let id = submit_to(&log, "ok");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(log.status(&id).0, "created");
        // Cancellation interrupts the disabled loop's one-second wait.
        token.cancel();
        tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("processor stops promptly")
            .unwrap();
    }

    #[tokio::test]
    async fn process_batch_claims_up_to_the_batch_size() {
        let base = handler_url().await;
        let log = MemoryLog::default();
        let reg = registry(&base);
        let ids: Vec<String> = (0..3).map(|_| submit_to(&log, "ok")).collect();
        assert_eq!(process_batch(&reg, &log, 2).await.unwrap(), 2);
        assert_eq!(log.status(&ids[2]).0, "created");
        assert_eq!(process_batch(&reg, &log, 2).await.unwrap(), 1);
        assert!(ids.iter().all(|id| log.status(id).0 == "completed"));
    }

    #[test]
    fn timestamps() {
        let t = |s: &str| UtcTimestamp::parse(s).unwrap();
        for (stored, aeson) in [
            ("2024-01-02T03:04:05.123456", "2024-01-02T03:04:05.123456Z"),
            ("2024-01-02T03:04:05.123450", "2024-01-02T03:04:05.12345Z"),
            ("2024-01-02T03:04:05.5", "2024-01-02T03:04:05.5Z"),
            ("2024-01-02T03:04:05.100000", "2024-01-02T03:04:05.1Z"),
            ("2024-01-02T03:04:05.000120", "2024-01-02T03:04:05.00012Z"),
            ("2024-01-02T03:04:05.000000", "2024-01-02T03:04:05Z"),
        ] {
            assert_eq!(t(stored).to_aeson(), aeson);
        }
        assert_eq!(
            t("2024-01-02T03:04:05.123400").to_postgres_json(),
            "2024-01-02T03:04:05.1234+00:00"
        );
        assert_eq!(
            t("2024-01-02T03:04:05").to_postgres_json(),
            "2024-01-02T03:04:05+00:00"
        );
        assert_eq!(UtcTimestamp::parse("2024-01-02"), None);
    }

    #[test]
    fn stored_errors_are_reencoded() {
        let stored = json!({"path": "$", "error": "boom", "code": "invalid-email", "internal": 1});
        assert_eq!(
            reencode_error(&stored, false).to_string(),
            r#"{"code":"invalid-email","error":"boom","path":"$"}"#
        );
        assert_eq!(reencode_error(&stored, true)["internal"], json!(1));
        assert_eq!(
            reencode_error(&json!({}), false),
            json!({"code": "unexpected", "error": "internal: error in parsing the action log", "path": "$"})
        );
    }

    #[test]
    fn headers_round_trip_without_trace_or_session_headers() {
        let mut h = HeaderMap::new();
        for (k, v) in [
            ("authorization", "Bearer t"),
            ("x-hasura-admin-secret", "s"),
            ("traceparent", "00-abc-def-01"),
            ("x-b3-traceid", "1"),
            ("cookie", "a=1"),
        ] {
            h.append(k, v.parse().unwrap());
        }
        let stored = stored_request_headers(&h);
        assert_eq!(
            stored,
            json!({"authorization": "Bearer t", "traceparent": "00-abc-def-01", "x-b3-traceid": "1", "cookie": "a=1"})
        );
        let back = headers_from_log(&stored);
        let names: Vec<&str> = back.keys().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["authorization", "cookie"]);
    }
}
