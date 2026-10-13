//! `websocket-log`: V2's `WSLog` (`Transport/WebSocket.hs`), one line per connection event.
//!
//! `{"user_vars", "connection_info": {"websocket_id", "token_expiry", "msg"}, "event"}`;
//! `error` level for `rejected`, `conn_err`, `proto_err` and `query_err`, `info` otherwise.

use std::time::SystemTime;

use hge_core::{Error, Session};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::telemetry::{self, Level, LogType, time::utc_iso8601};

/// V2's note when the `Cookie` header is dropped from an upgrade (CORS disabled).
pub(super) const CORS_NOTE: &str = "Cookie is not read when CORS is disabled, because it is a potential security issue. If you're already handling CORS before Hasura and enforcing CORS on websocket connections, then you can use the flag --ws-read-cookie or HASURA_GRAPHQL_WS_READ_COOKIE to force read cookie when CORS is disabled.";

/// Write one `websocket-log` line; `event` is only built when the line is written.
pub(super) fn log(
    ws_id: Uuid,
    user: Option<(&Session, Option<SystemTime>)>,
    msg: Option<&str>,
    level: Level,
    event: impl FnOnce() -> Json,
) {
    if !telemetry::enabled(LogType::WebsocketLog, level) {
        return;
    }
    let (user_vars, token_expiry) = match user {
        Some((session, expiry)) => (session.variables.to_json(), expiry.map(utc_iso8601)),
        None => (Json::Null, None),
    };
    telemetry::emit(
        LogType::WebsocketLog,
        level,
        &json!({
            "user_vars": user_vars,
            "connection_info": {
                "websocket_id": ws_id.to_string(),
                "token_expiry": token_expiry,
                "msg": msg,
            },
            "event": event(),
        }),
    );
}

/// `accepted`, `closed`.
pub(super) fn event(ty: &str) -> Json {
    json!({ "type": ty })
}

/// `rejected` (a QErr) or `conn_err` (a message).
pub(super) fn event_with(ty: &str, detail: Json) -> Json {
    json!({ "type": ty, "detail": detail })
}

/// A rejected upgrade (no connection exists yet).
pub(super) fn rejected(e: &Error) {
    log(Uuid::new_v4(), None, None, Level::Error, || {
        event_with("rejected", e.to_api_json(true))
    });
}

/// What happened to an operation (V2's `OpDetail`).
pub(super) enum OpEvent<'a> {
    Started,
    Completed,
    /// Completed after an error raised before executing: V2 logs it without the request id.
    CompletedEarly,
    Stopped,
    ProtoErr(&'a str),
    QueryErr(&'a Error),
}

/// An operation's identity for its `operation` events, captured when it starts. `None` when
/// `websocket-log` is off, so nothing is copied for it.
pub(super) struct OpLog {
    ws_id: Uuid,
    session: Option<(Session, Option<SystemTime>)>,
    operation_id: String,
    request_id: Option<String>,
    operation_name: Option<String>,
    /// `{"operationName","query"}`: only with `query-log` on (V2 never logs the variables).
    query: Option<Json>,
}

impl OpLog {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        ws_id: Uuid,
        session: Option<(&Session, Option<SystemTime>)>,
        operation_id: &str,
        request_id: Option<Option<&str>>,
        operation_name: Option<&str>,
        query: Option<&str>,
    ) -> Option<Box<OpLog>> {
        if !telemetry::type_enabled(LogType::WebsocketLog) {
            return None;
        }
        let query = query
            .filter(|_| telemetry::type_enabled(LogType::QueryLog))
            .map(|q| {
                let mut o = serde_json::Map::new();
                if let Some(name) = operation_name {
                    o.insert("operationName".into(), name.into());
                }
                o.insert("query".into(), q.into());
                Json::Object(o)
            });
        Some(Box::new(OpLog {
            ws_id,
            session: session.map(|(s, e)| (s.clone(), e)),
            operation_id: operation_id.to_owned(),
            request_id: request_id
                .map(|id| id.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned)),
            operation_name: operation_name.map(str::to_owned),
            query,
        }))
    }

    /// Log `event`. Like V2, protocol errors and stops carry no request id, and stops no
    /// query.
    pub(super) fn log(&self, event: OpEvent<'_>) {
        let (level, op_type, with_request) = match event {
            OpEvent::Started => (Level::Info, json!({"type": "started"}), true),
            OpEvent::Completed => (Level::Info, json!({"type": "completed"}), true),
            OpEvent::CompletedEarly => (Level::Info, json!({"type": "completed"}), false),
            OpEvent::Stopped => (Level::Info, json!({"type": "stopped"}), false),
            OpEvent::ProtoErr(m) => (
                Level::Error,
                json!({"type": "proto_err", "detail": m}),
                false,
            ),
            OpEvent::QueryErr(e) => (
                Level::Error,
                json!({"type": "query_err", "detail": e.to_api_json(true)}),
                true,
            ),
        };
        let stopped = matches!(op_type["type"].as_str(), Some("stopped"));
        let user = self.session.as_ref().map(|(s, e)| (s, *e));
        log(self.ws_id, user, None, level, || {
            let request_id = self.request_id.as_deref().filter(|_| with_request);
            let query = self.query.as_ref().filter(|_| !stopped);
            json!({
                "type": "operation",
                "detail": {
                    "operation_id": self.operation_id,
                    "request_id": request_id,
                    "operation_name": self.operation_name,
                    "operation_type": op_type,
                    "query": query,
                    // V2 hashes its normalised query plan; there is no equivalent here.
                    "parameterized_query_hash": null,
                },
            })
        });
    }
}
