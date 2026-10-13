//! GraphQL over WebSockets on `/v1/graphql` and `/v1alpha1/graphql` (spec 09): both
//! sub-protocols, `connection_init` auth, queries and mutations, live queries and streaming
//! subscriptions.
//!
//! Each connection runs two tasks: the connection loop (this module's [`Connection`]), which
//! reads client frames and handles them strictly in order, and a writer that drains the
//! connection's [`Outbox`] and sends keep-alives. Queries and mutations run in tasks of their
//! own, so a slow query does not hold up `stop` messages; subscriptions start inline, so a
//! `stop` always finds the subscription its `start` registered.

mod async_actions;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::extract::FromRequestParts;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use hge_core::{Code, Error, Session};
use hge_ir::{ActionCall, ActionRootField, OperationKind, QueryRoot, RootField, SourceExecutor};
pub use hge_subscriptions::PollerOptions;
use hge_subscriptions::protocol::{
    self, ClientMessage, GRAPHQL_TRANSPORT_WS, GRAPHQL_WS, Protocol, StartPayload,
};
use hge_subscriptions::{Registry, Subscriber, SubscriptionHandle};
use serde_json::Value as Json;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::graphql::{self, GraphQLRequest};
use crate::state::{AppState, Engine};

/// WebSocket and subscription settings.
#[derive(Debug, Clone)]
pub struct WebSocketConfig {
    /// `HASURA_GRAPHQL_WEBSOCKET_KEEPALIVE` (seconds)
    pub keep_alive: Duration,
    /// `HASURA_GRAPHQL_WEBSOCKET_CONNECTION_INIT_TIMEOUT` (seconds)
    pub init_timeout: Duration,
    /// `HASURA_GRAPHQL_WEBSOCKET_QUEUE_SIZE`: queued results per connection before the
    /// operation producing them is stopped.
    pub queue_size: usize,
    /// `HASURA_GRAPHQL_CLOSE_WEBSOCKETS_ON_METADATA_CHANGE`
    pub close_on_metadata_change: bool,
    /// `HASURA_GRAPHQL_WS_READ_COOKIE`: keep the `Cookie` header when CORS is disabled.
    pub read_cookie: bool,
    /// `HASURA_GRAPHQL_LIVE_QUERIES_MULTIPLEXED_*`
    pub live_queries: PollerOptions,
    /// `HASURA_GRAPHQL_STREAMING_QUERIES_MULTIPLEXED_*`
    pub streaming: PollerOptions,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        WebSocketConfig {
            keep_alive: Duration::from_secs(5),
            init_timeout: Duration::from_secs(3),
            queue_size: 300,
            close_on_metadata_change: true,
            read_cookie: false,
            live_queries: PollerOptions::default(),
            streaming: PollerOptions::default(),
        }
    }
}

/// Initial capacity of a connection's read buffer.
const READ_BUFFER_SIZE: usize = 4 * 1024;

/// Server-wide WebSocket state: the pollers and the open connections.
pub struct WebSockets {
    pub live_queries: Registry,
    pub streaming: Registry,
    connections: Mutex<HashMap<u64, mpsc::UnboundedSender<Event>>>,
    next_connection: AtomicU64,
    shutting_down: AtomicBool,
    /// Bounds the async action subscriptions reading the action log at once.
    async_action_polls: Arc<Semaphore>,
}

impl WebSockets {
    pub fn new(config: &WebSocketConfig) -> Self {
        WebSockets {
            live_queries: Registry::new(config.live_queries),
            streaming: Registry::new(config.streaming),
            connections: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            async_action_polls: Arc::new(Semaphore::new(async_actions::CONCURRENT_POLLS)),
        }
    }

    /// Open connections.
    pub fn connection_count(&self) -> usize {
        self.connections.lock().expect("lock").len()
    }

    /// Close every connection with `1012` (service restart): clients reconnect.
    pub fn close_all(&self, reason: &str) {
        for tx in self.connections.lock().expect("lock").values() {
            let _ = tx.send(Event::Close(1012, reason.to_owned()));
        }
    }

    /// Refuse new connections, close the open ones and wait (up to `timeout`) for them to go.
    pub async fn shutdown(&self, timeout: Duration) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.close_all("shutting server down");
        let deadline = tokio::time::Instant::now() + timeout;
        while self.connection_count() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Which endpoint a connection is on: `/v1alpha1/graphql` reports errors in the legacy
/// (`{"path","error","code"}`) format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    V1,
    V1Alpha1,
}

/// A `GET` handler argument: the WebSocket upgrade, when the request is one.
pub struct MaybeUpgrade(pub Option<WebSocketUpgrade>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeUpgrade {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let is_upgrade = parts
            .headers
            .get(header::UPGRADE)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
        if !is_upgrade {
            return Ok(MaybeUpgrade(None));
        }
        Ok(MaybeUpgrade(
            WebSocketUpgrade::from_request_parts(parts, state)
                .await
                .ok(),
        ))
    }
}

/// Middleware: a WebSocket upgrade on a path that does not serve websockets gets V2's 404,
/// whatever route (if any) the path has. V2 checks the path before any HTTP routing.
pub async fn reject_other_paths(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let is_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    // The relay endpoints are not implemented yet; their upgrades fall through to a 404.
    let served = matches!(
        req.uri().path(),
        "/v1/graphql" | "/v1alpha1/graphql" | "/v1/relay" | "/v1beta1/relay"
    );
    if !is_upgrade || served {
        return next.run(req).await;
    }
    {
        let body = serde_json::json!({
            "message": "only '/v1/graphql', '/v1alpha1/graphql', '/v1/relay' and '/v1beta1/relay' are supported on websockets",
            "extensions": { "path": "$", "code": "not-found" },
        });
        (StatusCode::NOT_FOUND, axum::Json(body)).into_response()
    }
}

/// Accept (or refuse) a WebSocket upgrade.
pub fn upgrade(
    engine: Arc<Engine>,
    ws: WebSocketUpgrade,
    mut headers: HeaderMap,
    endpoint: Endpoint,
) -> Response {
    if engine.ws.shutting_down.load(Ordering::SeqCst) {
        let mut resp = (StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down").into_response();
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("0"));
        return resp;
    }
    if let Err(resp) = enforce_cors(&engine, &mut headers) {
        return *resp;
    }
    for h in [
        header::SEC_WEBSOCKET_KEY,
        header::SEC_WEBSOCKET_VERSION,
        header::UPGRADE,
        header::CONNECTION,
    ] {
        headers.remove(h);
    }
    // tungstenite allocates and zero-fills its whole read buffer per connection (128 KiB by
    // default), which dominated the memory of a subscriber. Clients send small messages; a
    // larger one grows the buffer as needed.
    let ws = ws
        .read_buffer_size(READ_BUFFER_SIZE)
        .protocols([GRAPHQL_TRANSPORT_WS, GRAPHQL_WS]);
    let protocol = Protocol::from_selected(ws.selected_protocol().and_then(|p| p.to_str().ok()));
    ws.on_upgrade(move |socket| async move {
        Connection::run(engine, socket, protocol, endpoint, headers).await;
    })
}

/// V2's CORS rules for upgrades (spec 09 §1.2).
fn enforce_cors(engine: &Engine, headers: &mut HeaderMap) -> Result<(), Box<Response>> {
    let config = &engine.config;
    let Some(origin) = headers.get(header::ORIGIN).cloned() else {
        return Ok(());
    };
    if config.disable_cors {
        if !config.websocket.read_cookie {
            headers.remove(header::COOKIE);
        }
        return Ok(());
    }
    let domain = config.cors_domain.trim();
    if domain == "*" {
        return Ok(());
    }
    let origin = origin.to_str().unwrap_or_default();
    if domain
        .split(',')
        .map(str::trim)
        .any(|d| origin_matches(d, origin))
    {
        return Ok(());
    }
    let body = serde_json::json!({
        "message": "received origin header does not match configured CORS domains",
        "extensions": {"path": "$", "code": "access-denied"},
    });
    Err(Box::new(
        (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            body.to_string(),
        )
            .into_response(),
    ))
}

/// `https://*.example.com` matches any subdomain; anything else must match exactly.
fn origin_matches(pattern: &str, origin: &str) -> bool {
    match pattern.split_once("*.") {
        Some((scheme, rest)) => origin
            .strip_prefix(scheme)
            .and_then(|o| o.strip_suffix(rest))
            .is_some_and(|sub| sub.ends_with('.') && sub.len() > 1),
        None => pattern == origin,
    }
}

/// Events for a connection loop from outside it.
enum Event {
    /// Close the connection (server-initiated).
    Close(u16, String),
    /// A subscription's results overflowed the send queue: (operation id, generation, the
    /// cursor to resume a stream from).
    Overflow(String, u64, Option<Json>),
    /// An operation ended on its own (an async action subscription completed or failed):
    /// (operation id, generation).
    Finished(String, u64),
}

/// A running subscription.
enum Running {
    /// A live query or stream, in a poller.
    Poller(#[allow(dead_code)] SubscriptionHandle),
    /// An async action subscription's task.
    Task(async_actions::TaskHandle),
}

/// Frames for the writer.
enum Out {
    Text(String, bool),
    Close(u16, String),
}

/// The send queue of a connection. Results count against `capacity`; control messages
/// (acks, errors, completes) always go through.
#[derive(Clone)]
struct Outbox {
    tx: mpsc::UnboundedSender<Out>,
    queued: Arc<AtomicUsize>,
    capacity: usize,
}

impl Outbox {
    fn control(&self, msg: String) {
        let _ = self.tx.send(Out::Text(msg, false));
    }

    /// Queue a result; `false` when the queue is full.
    fn data(&self, msg: String) -> bool {
        if self.queued.fetch_add(1, Ordering::SeqCst) >= self.capacity {
            self.queued.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        let _ = self.tx.send(Out::Text(msg, true));
        true
    }

    fn close(&self, code: u16, reason: String) {
        let _ = self.tx.send(Out::Close(code, reason));
    }
}

async fn write(
    mut sink: futures::stream::SplitSink<WebSocket, Message>,
    mut rx: mpsc::UnboundedReceiver<Out>,
    queued: Arc<AtomicUsize>,
    protocol: Protocol,
    keep_alive: Duration,
) {
    // V2 starts keep-alives as soon as the connection is accepted.
    let mut ticker = tokio::time::interval(keep_alive.max(Duration::from_millis(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let out = tokio::select! {
            out = rx.recv() => out,
            _ = ticker.tick(), if !keep_alive.is_zero() => {
                Some(Out::Text(protocol.keep_alive().to_owned(), false))
            }
        };
        match out {
            None => return,
            Some(Out::Text(text, is_data)) => {
                if is_data {
                    queued.fetch_sub(1, Ordering::SeqCst);
                }
                if sink.send(Message::text(text)).await.is_err() {
                    return;
                }
            }
            Some(Out::Close(code, reason)) => {
                let frame = CloseFrame {
                    code,
                    reason: protocol::truncate_reason(reason).into(),
                };
                let _ = sink.send(Message::Close(Some(frame))).await;
                // Nothing more may be sent; the connection loop waits for the client's reply.
                while rx.recv().await.is_some() {}
                return;
            }
        }
    }
}

enum ConnState {
    NotInitialised,
    Initialised {
        session: Session,
        expires_at: Option<SystemTime>,
        /// The headers the session was resolved from, forwarded to action handlers.
        headers: Arc<HeaderMap>,
    },
    InitError(String),
}

struct Connection {
    engine: Arc<Engine>,
    protocol: Protocol,
    endpoint: Endpoint,
    id: u64,
    out: Outbox,
    events: mpsc::UnboundedSender<Event>,
    /// Headers of the upgrade request (used by the first `connection_init` only).
    upgrade_headers: HeaderMap,
    state: ConnState,
    /// Running subscriptions by operation id, with the generation they were started with.
    subscriptions: HashMap<String, (u64, Running)>,
    next_generation: u64,
    queries: JoinSet<()>,
    /// Set once the server sent a close frame: wait for the client's reply.
    closing: bool,
}

/// How long to wait for the client to answer our close frame.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// The reply V2 sends when a result cannot be queued.
const OVERFLOW_MESSAGE: &str = "This operation's result could not be delivered because the connection's send queue was at capacity, so it has been stopped. Please retry.";

impl Connection {
    async fn run(
        engine: Arc<Engine>,
        socket: WebSocket,
        protocol: Protocol,
        endpoint: Endpoint,
        upgrade_headers: HeaderMap,
    ) {
        let (sink, mut stream) = socket.split();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let ws_config = engine.config.websocket.clone();
        let mut writer = tokio::spawn(write(
            sink,
            out_rx,
            queued.clone(),
            protocol,
            ws_config.keep_alive,
        ));
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let id = engine.ws.next_connection.fetch_add(1, Ordering::Relaxed);
        engine
            .ws
            .connections
            .lock()
            .expect("lock")
            .insert(id, events_tx.clone());
        tracing::info!(target: "websocket-log", connection = id, ?protocol, "accepted");

        let mut conn = Connection {
            engine: engine.clone(),
            protocol,
            endpoint,
            id,
            out: Outbox {
                tx: out_tx,
                queued,
                capacity: ws_config.queue_size,
            },
            events: events_tx,
            upgrade_headers,
            state: ConnState::NotInitialised,
            subscriptions: HashMap::new(),
            next_generation: 0,
            queries: JoinSet::new(),
            closing: false,
        };

        let init_deadline = tokio::time::sleep(ws_config.init_timeout);
        tokio::pin!(init_deadline);
        let close_deadline = tokio::time::sleep(Duration::MAX / 4);
        tokio::pin!(close_deadline);
        let mut close_armed = false;
        // A finished `JoinHandle` must not be polled again.
        let mut writer_done = false;
        loop {
            let expiry = conn.expiry();
            tokio::select! {
                frame = stream.next() => match frame {
                    Some(Ok(Message::Text(t))) => conn.on_frame(t.as_bytes()).await,
                    Some(Ok(Message::Binary(b))) => conn.on_frame(&b).await,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {} // ping / pong frames are answered by the library
                },
                Some(event) = events_rx.recv() => match event {
                    Event::Close(code, reason) => conn.close(code, reason),
                    Event::Overflow(op, generation, cursor) => conn.on_overflow(&op, generation, cursor),
                    Event::Finished(op, generation) => conn.on_finished(&op, generation),
                },
                () = &mut init_deadline, if matches!(conn.state, ConnState::NotInitialised) && !conn.closing => {
                    let code = match protocol {
                        Protocol::Apollo => 1011,
                        Protocol::GraphqlWs => 4408,
                    };
                    tracing::info!(target: "websocket-log", connection = id, "connection_timeout");
                    conn.close(code, "Connection initialisation timed out".to_owned());
                }
                () = sleep_until_system(expiry), if expiry.is_some() => {
                    // Like V2: drop the connection without a close frame; clients reconnect
                    // with fresh credentials.
                    tracing::info!(target: "websocket-log", connection = id, "jwt_expired");
                    writer.abort();
                    break;
                }
                () = &mut close_deadline, if close_armed => break,
                _ = &mut writer => {
                    writer_done = true;
                    break;
                }
            }
            if conn.closing && !close_armed {
                close_armed = true;
                close_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + CLOSE_GRACE);
            }
        }

        engine.ws.connections.lock().expect("lock").remove(&id);
        conn.subscriptions.clear();
        conn.queries.abort_all();
        // Dropping the outbox lets the writer finish what is queued, then stop.
        drop(conn);
        if !writer_done {
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut writer).await;
            writer.abort();
        }
        tracing::info!(target: "websocket-log", connection = id, "closed");
    }

    fn expiry(&self) -> Option<SystemTime> {
        match &self.state {
            ConnState::Initialised { expires_at, .. } => *expires_at,
            _ => None,
        }
    }

    fn close(&mut self, code: u16, reason: String) {
        if !self.closing {
            self.closing = true;
            self.out.close(code, reason);
        }
    }

    async fn on_frame(&mut self, bytes: &[u8]) {
        if self.closing {
            return;
        }
        let msg = match protocol::parse_client_message(bytes) {
            Ok(msg) => msg,
            Err(e) => {
                match self.protocol {
                    Protocol::Apollo => self.out.control(protocol::connection_error(&e)),
                    Protocol::GraphqlWs => self.close(
                        4400,
                        protocol::close_reason_json(
                            4400,
                            &format!("Parsing client message failed: {e}"),
                        ),
                    ),
                }
                return;
            }
        };
        match msg {
            ClientMessage::ConnectionInit { headers } => self.on_init(headers).await,
            ClientMessage::Start { id, payload } => self.on_start(id, payload).await,
            ClientMessage::Stop { id } => {
                self.subscriptions.remove(&id);
            }
            ClientMessage::Terminate => {
                self.close(1000, "GQL_CONNECTION_TERMINATE received".to_owned());
            }
            ClientMessage::Ping { message } => self.out.control(protocol::pong(message.as_deref())),
            ClientMessage::Pong => {}
        }
    }

    async fn on_init(&mut self, payload_headers: Vec<(String, String)>) {
        if let ConnState::InitError(e) = &self.state {
            let e = e.clone();
            self.init_failed(&e);
            return;
        }
        // Payload headers first, then (on the first init only) the upgrade request's.
        let mut headers = HeaderMap::new();
        for (name, value) in &payload_headers {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.append(n, v);
            }
        }
        if matches!(self.state, ConnState::NotInitialised) {
            for (n, v) in &self.upgrade_headers {
                headers.append(n.clone(), v.clone());
            }
        }
        match self.engine.auth.authenticate(&headers, None).await {
            Ok(auth) => {
                tracing::info!(
                    target: "websocket-log",
                    connection = self.id,
                    role = auth.session.role.as_str(),
                    "initialised"
                );
                self.state = ConnState::Initialised {
                    session: auth.session,
                    expires_at: auth.expires_at,
                    headers: Arc::new(headers),
                };
                self.out.control(protocol::CONNECTION_ACK.to_owned());
                self.out.control(self.protocol.keep_alive().to_owned());
            }
            Err(e) => {
                let message = e.error.message;
                self.state = ConnState::InitError(message.clone());
                self.init_failed(&message);
            }
        }
    }

    fn init_failed(&mut self, message: &str) {
        tracing::info!(target: "websocket-log", connection = self.id, error = message, "init failed");
        match self.protocol {
            Protocol::Apollo => {
                self.out.control(protocol::connection_error(message));
                self.close(
                    4400,
                    protocol::close_reason_json(
                        4400,
                        &format!("Connection initialization failed: {message}"),
                    ),
                );
            }
            Protocol::GraphqlWs => {
                self.close(4403, protocol::close_reason_json(4403, "Forbidden"));
            }
        }
    }

    async fn on_start(&mut self, id: String, payload: StartPayload) {
        // A task that completed may not have reported it yet: its id is free.
        if let Some((_, Running::Task(t))) = self.subscriptions.get(&id)
            && t.is_released()
        {
            self.subscriptions.remove(&id);
        }
        if self.subscriptions.contains_key(&id) {
            return self.start_error(
                &id,
                format!("an operation already exists with this id: {id}"),
            );
        }
        let (session, headers) = match &self.state {
            ConnState::Initialised {
                session, headers, ..
            } => (session.clone(), headers.clone()),
            ConnState::InitError(e) => {
                let msg = format!("cannot start as connection_init failed with: {e}");
                return self.start_error(&id, msg);
            }
            ConnState::NotInitialised => {
                let msg = "start received before the connection is initialised".to_owned();
                return self.start_error(&id, msg);
            }
        };
        let state = self.engine.state.load_full();
        let query = payload.query;
        let req = GraphQLRequest {
            query: Some(query.clone()),
            variables: payload.variables,
            operation_name: payload.operation_name,
            read_only: false,
        };
        let op = match graphql::plan(&state, &session, req) {
            Ok(op) => op,
            Err(e) => return self.request_error(&id, &e),
        };
        let is_mutation = op.kind == OperationKind::Mutation;
        // Action handlers raise errors with the same codes as planning (an unparsable
        // response body, a missing header template), so only database operations qualify.
        let has_actions = op
            .root_fields
            .iter()
            .any(|(_, rf)| matches!(rf, RootField::Action(_)));
        if op.kind != OperationKind::Subscription {
            // Forget finished queries, so a long-lived connection does not accumulate them.
            while self.queries.try_join_next().is_some() {}
            let out = self.out.clone();
            let (protocol, endpoint) = (self.protocol, self.endpoint);
            let engine = self.engine.clone();
            let run = async move {
                match graphql::execute(&engine, &state, &session, &headers, Some(&query), &op).await
                {
                    // Like V2, `Set-Cookie` headers from actions are dropped over websockets.
                    Ok((body, _set_cookies)) => {
                        if !out.data(protocol.data(&id, &body)) {
                            let e = Error::unexpected(OVERFLOW_MESSAGE);
                            out.control(error_message(
                                protocol,
                                endpoint,
                                &id,
                                error_json(&e, endpoint),
                            ));
                        }
                    }
                    // An error V2 would have raised while planning (such as a missing session
                    // variable) is an `error` message; one from running it is `data`.
                    Err(e) if !has_actions && is_planning_error(&e) => {
                        out.control(error_message(
                            protocol,
                            endpoint,
                            &id,
                            error_json(&e, endpoint),
                        ));
                    }
                    Err(e) => {
                        let errors = [error_json(&e, endpoint)];
                        out.control(protocol.execution_error(&id, &errors));
                    }
                }
                out.control(protocol::complete(&id));
            };
            // Mutations run before the next message is read, like V2, so that an operation
            // sent after a mutation sees its effects. Queries run concurrently.
            if is_mutation {
                run.await;
            } else {
                self.queries.spawn(run);
            }
            return;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        let running = match subscription_plan(&op) {
            Err(e) => Err(e),
            Ok(Plan::Live {
                source,
                fields,
                layout,
            }) => self
                .subscribe(&state, &session, &id, generation, source, fields, layout)
                .await
                .map(Running::Poller),
            Ok(Plan::AsyncActions { fields, live }) => Ok(Running::Task(
                async_actions::AsyncActionSubscription {
                    engine: self.engine.clone(),
                    state: state.clone(),
                    session,
                    headers,
                    query,
                    fields: fields
                        .into_iter()
                        .map(|(alias, f)| (alias.to_owned(), f.clone()))
                        .collect(),
                    live,
                    id: id.clone(),
                    generation,
                    protocol: self.protocol,
                    endpoint: self.endpoint,
                    out: self.out.clone(),
                    events: self.events.clone(),
                    polls: self.engine.ws.async_action_polls.clone(),
                }
                .spawn(),
            )),
        };
        match running {
            Ok(running) => {
                self.subscriptions.insert(id, (generation, running));
            }
            Err(e) => self.request_error(&id, &e),
        }
    }

    fn on_finished(&mut self, id: &str, generation: u64) {
        if self
            .subscriptions
            .get(id)
            .is_some_and(|(g, _)| *g == generation)
        {
            self.subscriptions.remove(id);
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn subscribe(
        &self,
        state: &AppState,
        session: &Session,
        id: &str,
        generation: u64,
        source: &str,
        fields: Vec<&QueryRoot>,
        layout: Layout,
    ) -> Result<SubscriptionHandle, Error> {
        let executor: Arc<dyn SourceExecutor> = state
            .sources
            .get(source)
            .ok_or_else(|| Error::unexpected(format!("source \"{source}\" is not available")))?
            .clone();
        let cursor_field = match fields.as_slice() {
            [QueryRoot::Stream(s)] => Some(s.cursor.field.clone()),
            _ => None,
        };
        let query = executor.multiplexed_query(&fields, session)?;
        let registry = if query.cursor.is_some() {
            &self.engine.ws.streaming
        } else {
            &self.engine.ws.live_queries
        };
        let sink = Arc::new(OperationSink {
            id: id.to_owned(),
            generation,
            cursor_field,
            protocol: self.protocol,
            out: self.out.clone(),
            events: self.events.clone(),
            layout,
            overflowed: AtomicBool::new(false),
        });
        registry.subscribe(executor, query, sink).await
    }

    /// `generation` tells the overflowed subscription apart from a later one reusing its id.
    fn on_overflow(&mut self, id: &str, generation: u64, resume_cursor: Option<Json>) {
        if self
            .subscriptions
            .get(id)
            .is_some_and(|(g, _)| *g == generation)
        {
            self.subscriptions.remove(id);
            let e = match resume_cursor {
                None => Error::unexpected(OVERFLOW_MESSAGE),
                Some(cursor) => stream_overflow_error(cursor),
            };
            let error = error_json(&e, self.endpoint);
            self.out
                .control(error_message(self.protocol, self.endpoint, id, error));
            self.out.control(protocol::complete(id));
        }
    }

    /// A request that could not start (spec 09 §4.4, `start-failed`).
    fn start_error(&mut self, id: &str, message: String) {
        let e = Error::new(Code::StartFailed, message.clone());
        self.out
            .control(protocol::error_object(id, &error_json(&e, self.endpoint)));
        match self.protocol {
            Protocol::Apollo => self.out.control(protocol::complete(id)),
            Protocol::GraphqlWs => {
                let direct = serde_json::json!({ "type": "error", "id": id, "payload": message });
                self.out.control(direct.to_string());
                self.close(1000, protocol::close_reason_json(4400, &message));
            }
        }
    }

    /// A request that failed before executing: parse, validation, permissions, planning.
    fn request_error(&mut self, id: &str, e: &Error) {
        let error = error_json(e, self.endpoint);
        self.out
            .control(error_message(self.protocol, self.endpoint, id, error));
        self.out.control(protocol::complete(id));
    }
}

/// Whether an error from executing a database query or mutation is one V2 reports before
/// executing (its planner resolves session variables and validates arguments, which happens
/// here while translating to SQL); database errors have other codes and are execution errors.
fn is_planning_error(e: &Error) -> bool {
    e.extensions.is_none()
        && matches!(
            e.code,
            Code::NotFound | Code::ValidationFailed | Code::NotSupported | Code::ParseFailed
        )
}

/// An `error` message: the bare legacy object on `/v1alpha1/graphql`, the protocol's errors
/// payload otherwise.
fn error_message(protocol: Protocol, endpoint: Endpoint, id: &str, error: Json) -> String {
    match endpoint {
        Endpoint::V1Alpha1 => protocol::error_object(id, &error),
        Endpoint::V1 => protocol.error(id, &[error]),
    }
}

/// An error as the endpoint reports it. WebSockets never include internal details.
fn error_json(e: &Error, endpoint: Endpoint) -> Json {
    match endpoint {
        Endpoint::V1 => e.to_graphql_json(false),
        Endpoint::V1Alpha1 => e.to_api_json(false),
    }
}

async fn sleep_until_system(at: Option<SystemTime>) {
    let wait = at
        .and_then(|t| t.duration_since(SystemTime::now()).ok())
        .unwrap_or_default();
    tokio::time::sleep(wait).await;
}

/// The response keys of a subscription's root fields, in order; field `i` is column `i` of
/// the multiplexed query.
struct Layout(Vec<String>);

impl Layout {
    /// `{"data":{"alias":…}}`
    fn render(&self, fields: &[Option<String>]) -> String {
        let mut out = String::with_capacity(
            16 + fields.iter().flatten().map(String::len).sum::<usize>() + 16 * self.0.len(),
        );
        out.push_str(r#"{"data":{"#);
        for (i, alias) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&Json::String(alias.clone()).to_string());
            out.push(':');
            out.push_str(fields.get(i).and_then(|f| f.as_deref()).unwrap_or("null"));
        }
        out.push_str("}}");
        out
    }
}

/// How a subscription runs.
enum Plan<'a> {
    /// Database root fields of one source, multiplexed in a poller.
    Live {
        source: &'a str,
        fields: Vec<&'a QueryRoot>,
        layout: Layout,
    },
    /// Async actions' results, by response key. `live`: some output type has
    /// relationships, so V2 runs it as a live query (it never completes).
    AsyncActions {
        fields: Vec<(&'a str, &'a ActionRootField)>,
        live: bool,
    },
}

/// Check V2's subscription restrictions (spec 09 §5) and collect the root fields.
fn subscription_plan(op: &hge_ir::Operation) -> Result<Plan<'_>, Error> {
    if op.root_fields.len() != 1 && !op.multiple_top_level_fields {
        return Err(Error::validation(
            "subscriptions must select one top level field",
        ));
    }
    let mut sources = vec![];
    let mut fields = vec![];
    let mut layout = vec![];
    let mut actions = vec![];
    for (alias, rf) in &op.root_fields {
        match rf {
            RootField::Database(db) => {
                sources.push(&*db.source);
                layout.push(alias.clone());
                fields.push(&db.query);
            }
            RootField::Static(_) => {
                return Err(Error::new(
                    Code::NotSupported,
                    "Introspection at root field level is not supported over subscriptions",
                ));
            }
            RootField::Action(a) => actions.push((alias.as_str(), &**a)),
            RootField::Mutation(_) => {
                return Err(Error::new(
                    Code::NotSupported,
                    "not supported over subscriptions",
                ));
            }
        }
    }
    if !actions.is_empty() {
        let (mut live, mut plain) = (false, false);
        for (_, a) in &actions {
            match &a.call {
                ActionCall::AsyncQuery {
                    has_relationships, ..
                } => {
                    live |= *has_relationships;
                    plain |= !*has_relationships;
                }
                ActionCall::Sync { .. } => {
                    return Err(Error::new(
                        Code::NotSupported,
                        "query actions cannot be run as a subscription",
                    ));
                }
                ActionCall::AsyncMutation { .. } => {
                    return Err(Error::new(
                        Code::NotSupported,
                        "not supported over subscriptions",
                    ));
                }
            }
        }
        // V2 runs actions with relationships in a live query, apart from the others.
        if plain && (live || !fields.is_empty()) {
            return Err(Error::new(
                Code::NotSupported,
                "async action queries with no relationships aren't expected to mix with normal source database queries",
            ));
        }
        // V2 also lets them share a live query with database fields of the same source;
        // here they run in their own task.
        if !fields.is_empty() {
            return Err(Error::new(
                Code::NotSupported,
                "async action queries with relationships cannot be mixed with database queries in one subscription",
            ));
        }
        return Ok(Plan::AsyncActions {
            fields: actions,
            live,
        });
    }
    let streams = fields
        .iter()
        .filter(|f| matches!(f, QueryRoot::Stream(_)))
        .count();
    if streams > 0 && streams < fields.len() {
        return Err(Error::new(
            Code::NotSupported,
            "streaming and livequery subscriptions cannot be executed in the same subscription",
        ));
    }
    if streams > 1 {
        return Err(Error::new(
            Code::NotSupported,
            "exactly one root field is allowed for streaming subscriptions",
        ));
    }
    let source = *sources
        .first()
        .ok_or_else(|| Error::validation("no subscriptions exist"))?;
    if sources.iter().any(|s| *s != source) {
        return Err(Error::new(
            Code::NotSupported,
            "all fields of a subscription must be from the same source",
        ));
    }
    Ok(Plan::Live {
        source,
        fields,
        layout: Layout(layout),
    })
}

/// The overflow error of a stream: it says which cursor to resume from (spec 09 §1.6).
fn stream_overflow_error(cursor: Json) -> Error {
    let message = format!(
        "{}, using this cursor as the initial_value to resume from exactly where results were lost: {cursor}.",
        OVERFLOW_MESSAGE.trim_end_matches('.')
    );
    let extensions =
        serde_json::json!({ "path": "$", "code": "unexpected", "resume_cursor": cursor });
    Error::unexpected(message).with_extensions(extensions)
}

/// Delivers a subscription's results to its connection.
struct OperationSink {
    id: String,
    generation: u64,
    /// Streams: the GraphQL name of the cursor column.
    cursor_field: Option<String>,
    protocol: Protocol,
    out: Outbox,
    events: mpsc::UnboundedSender<Event>,
    layout: Layout,
    overflowed: AtomicBool,
}

impl Subscriber for OperationSink {
    fn on_data(&self, fields: &[Option<String>], cursor: Option<&str>) {
        if self.overflowed.load(Ordering::Relaxed) {
            return;
        }
        let msg = self.protocol.data(&self.id, &self.layout.render(fields));
        if !self.out.data(msg) && !self.overflowed.swap(true, Ordering::SeqCst) {
            let resume_cursor = self.cursor_field.as_ref().map(|field| {
                let mut c = serde_json::Map::new();
                c.insert(field.clone(), cursor.map_or(Json::Null, Json::from));
                Json::Object(c)
            });
            let _ = self.events.send(Event::Overflow(
                self.id.clone(),
                self.generation,
                resume_cursor,
            ));
        }
    }

    fn on_error(&self, error: &Error) {
        // Poll errors always use the GraphQL error format, on every endpoint.
        let payload = serde_json::json!({ "data": null, "errors": [error.to_graphql_json(false)] });
        self.out
            .control(self.protocol.data(&self.id, &payload.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert!(origin_matches("https://a.com", "https://a.com"));
        assert!(!origin_matches("https://a.com", "https://b.com"));
        assert!(origin_matches("https://*.a.com", "https://x.a.com"));
        assert!(!origin_matches("https://*.a.com", "https://a.com"));
        assert!(!origin_matches("https://*.a.com", "http://x.a.com"));
    }

    #[test]
    fn layout_renders_aliases_in_order() {
        let layout = Layout(vec!["a".to_owned(), "b\"".to_owned()]);
        assert_eq!(
            layout.render(&[Some("[1]".to_owned()), None]),
            r#"{"data":{"a":[1],"b\"":null}}"#
        );
    }
}
