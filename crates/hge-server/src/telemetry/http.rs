//! `http-log`: one log per HTTP request (spec 11 §5.5).
//!
//! The outermost middleware ([`http_log`]) times the request and wraps the response body to
//! count the bytes sent; the log is written when the body is finished (or dropped, when the
//! client goes away). Handlers that know more (the session, the request body, the error)
//! attach an [`HttpLogInfo`] to their response with [`attach`]. Nothing is collected when
//! `http-log` is disabled.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, Uri, Version, header};
use axum::middleware::Next;
use axum::response::Response;
use http_body::{Frame, SizeHint};
use serde::Serialize;
use serde_json::Value;
use serde_json::value::RawValue;

use super::{Level, LogType};
use crate::state::Engine;

/// Whether handlers should collect an [`HttpLogInfo`] (the log is enabled at some level).
#[inline]
pub fn wanted() -> bool {
    super::enabled(LogType::HttpLog, Level::Error)
}

/// How a request was classified (`operation.request_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RequestMode {
    Single,
    Batched,
    #[default]
    NonGraphql,
    Error,
}

/// What a handler knows about its request, attached to the response.
#[derive(Debug, Clone)]
pub struct HttpLogInfo {
    started: Instant,
    pub request_id: Option<String>,
    /// The session variables, when the request was authenticated.
    pub user_vars: Option<Value>,
    /// The raw request body (the `query` field is derived from it).
    pub body: Option<Bytes>,
    /// The error (QErr JSON) when the request failed.
    pub error: Option<Value>,
    pub request_mode: RequestMode,
    /// `batched_operations` for a batched GraphQL request.
    pub batched_operations: Option<Vec<Value>>,
    query_execution_time: Option<f64>,
}

impl HttpLogInfo {
    /// Start collecting; call when the handler starts (the body has been read by then).
    pub fn start(request_id: &str) -> HttpLogInfo {
        HttpLogInfo {
            started: Instant::now(),
            request_id: Some(request_id.to_owned()),
            user_vars: None,
            body: None,
            error: None,
            request_mode: RequestMode::NonGraphql,
            batched_operations: None,
            query_execution_time: None,
        }
    }

    pub fn session(mut self, session: &hge_core::Session) -> Self {
        self.user_vars = Some(session.variables.to_json());
        self
    }

    pub fn body(mut self, body: Bytes) -> Self {
        self.body = Some(body);
        self
    }

    /// The request failed with `error`.
    pub fn failed(mut self, error: &hge_core::Error, mode: RequestMode) -> Self {
        self.error = Some(error.to_api_json(true));
        self.request_mode = mode;
        self
    }
}

/// Attach `info` to `resp`, recording the execution time.
pub fn attach(resp: &mut Response, info: Option<HttpLogInfo>) {
    if let Some(mut info) = info {
        info.query_execution_time = Some(info.started.elapsed().as_secs_f64());
        resp.extensions_mut().insert(info);
    }
}

/// The response size before compression, recorded inside the compression layer.
#[derive(Debug, Clone, Copy)]
struct UncompressedSize(u64);

/// Middleware inside the compression layer: remember the uncompressed body size.
pub async fn record_uncompressed_size(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    if let Some(n) = wanted().then(|| resp.body().size_hint().exact()).flatten() {
        resp.extensions_mut().insert(UncompressedSize(n));
    }
    resp
}

/// The outermost middleware: write an `http-log` for every request.
pub async fn http_log(State(engine): State<Arc<Engine>>, req: Request, next: Next) -> Response {
    // V2 writes no http-log for WebSocket traffic or CORS preflights (both are handled
    // outside its request logging).
    let headers = req.headers();
    let upgrade = headers
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let preflight = req.method() == Method::OPTIONS
        && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    if !wanted() || upgrade || preflight {
        return next.run(req).await;
    }
    let start = Instant::now();
    let request = RequestFacts {
        method: req.method().clone(),
        uri: req.uri().clone(),
        version: req.version(),
        forwarded: forwarded_for(headers),
        peer: req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0),
        request_id: headers.get("x-request-id").cloned(),
    };
    let resp = next.run(req).await;
    let (mut parts, body) = resp.into_parts();
    let info = parts.extensions.remove::<HttpLogInfo>();
    let uncompressed = parts.extensions.remove::<UncompressedSize>().map(|s| s.0);
    let pending = PendingLog {
        options: QueryOptions {
            metadata_query_logging: engine.config.enable_metadata_query_logging,
            only_on_error: engine.config.http_log_query_only_on_error,
            query_log: super::type_enabled(LogType::QueryLog),
        },
        status: parts.status.as_u16(),
        content_encoding: parts.headers.get(header::CONTENT_ENCODING).cloned(),
        response_request_id: parts.headers.get("x-request-id").cloned(),
        start,
        request,
        info,
        uncompressed,
    };
    let body = LoggedBody {
        inner: body,
        sent: 0,
        pending: Some(pending),
    };
    Response::from_parts(parts, Body::new(body))
}

/// The first `x-real-ip` / `x-forwarded-for` header (V2's client address, taken whole).
fn forwarded_for(headers: &HeaderMap) -> Option<HeaderValue> {
    headers
        .iter()
        .find(|(name, _)| *name == "x-real-ip" || *name == "x-forwarded-for")
        .map(|(_, v)| v.clone())
}

/// V2's client address: the forwarded header, else the peer address.
fn client_ip(forwarded: Option<&HeaderValue>, peer: Option<SocketAddr>) -> String {
    forwarded
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| peer.map(|p| p.ip().to_string()))
        .unwrap_or_default()
}

fn header_str(v: Option<&HeaderValue>) -> Option<&str> {
    v.and_then(|v| v.to_str().ok())
}

struct RequestFacts {
    method: Method,
    uri: Uri,
    version: Version,
    forwarded: Option<HeaderValue>,
    peer: Option<SocketAddr>,
    request_id: Option<HeaderValue>,
}

#[derive(Clone, Copy)]
struct QueryOptions {
    metadata_query_logging: bool,
    only_on_error: bool,
    query_log: bool,
}

struct PendingLog {
    options: QueryOptions,
    status: u16,
    content_encoding: Option<HeaderValue>,
    response_request_id: Option<HeaderValue>,
    start: Instant,
    request: RequestFacts,
    info: Option<HttpLogInfo>,
    uncompressed: Option<u64>,
}

/// The response body, counting bytes and logging when done.
struct LoggedBody {
    inner: Body,
    sent: u64,
    pending: Option<PendingLog>,
}

impl LoggedBody {
    fn finish(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.write(self.sent);
        }
    }
}

impl HttpBody for LoggedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.sent += data.len() as u64;
                }
                if this.inner.is_end_stream() {
                    this.finish();
                }
            }
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => this.finish(),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for LoggedBody {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Serialize)]
struct Detail<'a> {
    http_info: HttpInfo<'a>,
    operation: Operation<'a>,
    request_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    batched_operations: Option<&'a [Value]>,
}

#[derive(Serialize)]
struct HttpInfo<'a> {
    status: u16,
    method: &'a str,
    ip: &'a str,
    url: &'a str,
    http_version: &'static str,
    content_encoding: Option<&'a str>,
}

#[derive(Serialize)]
struct Operation<'a> {
    request_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_vars: Option<&'a Value>,
    response_size: u64,
    uncompressed_response_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_read_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query_execution_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<Query<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_query: Option<std::borrow::Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a Value>,
    request_mode: RequestMode,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Query<'a> {
    Full(&'a RawValue),
    Summary(Value),
}

impl PendingLog {
    fn write(self, sent: u64) {
        let info = self.info.as_ref();
        let error = info.and_then(|i| i.error.as_ref());
        let failed = error.is_some() || self.status >= 400;
        let level = if failed { Level::Error } else { Level::Info };
        let Some(logger) = super::logger().filter(|l| l.enabled(LogType::HttpLog, level)) else {
            return;
        };
        let generated;
        let request_id = match info
            .and_then(|i| i.request_id.as_deref())
            .or(header_str(self.response_request_id.as_ref()))
            .or(header_str(self.request.request_id.as_ref()))
        {
            Some(id) => id,
            None => {
                generated = uuid::Uuid::new_v4().to_string();
                &generated
            }
        };
        let body = info.and_then(|i| i.body.as_deref());
        let url = self.request.uri.path();
        let included = included(self.options, url, failed);
        // One parse of the body: whole when it is logged, else just the summary keys.
        let (query, raw_query) = if included {
            match body.map(serde_json::from_slice::<&RawValue>) {
                Some(Ok(raw)) => (Some(Query::Full(raw)), None),
                Some(Err(_)) if failed && body.is_some_and(|b| !b.is_empty()) => {
                    (None, body.map(String::from_utf8_lossy))
                }
                _ => (None, None),
            }
        } else {
            (Some(Query::Summary(summary(self.options, body))), None)
        };
        let request_read_time = info.map(|i| {
            i.started
                .saturating_duration_since(self.start)
                .as_secs_f64()
        });
        // V2 writes every error log with `request_mode: "error"`.
        let request_mode = match info {
            _ if error.is_some() => RequestMode::Error,
            Some(i) => i.request_mode,
            None if failed => RequestMode::Error,
            None => RequestMode::NonGraphql,
        };
        let ip = client_ip(self.request.forwarded.as_ref(), self.request.peer);
        let detail = Detail {
            http_info: HttpInfo {
                status: self.status,
                method: self.request.method.as_str(),
                ip: &ip,
                url,
                http_version: http_version(self.request.version),
                content_encoding: header_str(self.content_encoding.as_ref()),
            },
            operation: Operation {
                request_id,
                user_vars: info.and_then(|i| i.user_vars.as_ref()),
                response_size: sent,
                uncompressed_response_size: self.uncompressed.unwrap_or(sent),
                request_read_time,
                query_execution_time: info.and_then(|i| i.query_execution_time),
                query,
                raw_query,
                error,
                request_mode,
            },
            request_id,
            batched_operations: info.and_then(|i| i.batched_operations.as_deref()),
        };
        logger.write(LogType::HttpLog, level, &detail);
    }
}

/// Whether the parsed request body is logged as `query` (spec 11 §5.5 rules 1-2).
fn included(options: QueryOptions, url: &str, failed: bool) -> bool {
    if options.only_on_error {
        failed
    } else if options.query_log {
        !matches!(url, "/v1/metadata" | "/v1/query") || options.metadata_query_logging
    } else {
        false
    }
}

/// The `query` summary when the body is not logged: `{"operationName": …}` under
/// `HTTP_LOG_QUERY_ONLY_ON_ERROR`, else `{"type": …}`.
fn summary(options: QueryOptions, body: Option<&[u8]>) -> Value {
    #[derive(serde::Deserialize)]
    struct Keys {
        #[serde(rename = "type")]
        ty: Option<Value>,
        #[serde(rename = "operationName")]
        operation_name: Option<Value>,
    }
    let keys = body.and_then(|b| serde_json::from_slice::<Keys>(b).ok());
    let string = |v: Option<Value>| match v {
        Some(Value::String(s)) => Value::String(s),
        _ => Value::Null,
    };
    if options.only_on_error {
        serde_json::json!({"operationName": string(keys.and_then(|k| k.operation_name))})
    } else {
        serde_json::json!({"type": string(keys.and_then(|k| k.ty))})
    }
}

fn http_version(v: Version) -> &'static str {
    match v {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_2 => "HTTP/2.0",
        Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/1.1",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(only_on_error: bool, query_log: bool, metadata: bool) -> QueryOptions {
        QueryOptions {
            metadata_query_logging: metadata,
            only_on_error,
            query_log,
        }
    }

    #[test]
    fn query_inclusion_rules() {
        // Rule 1: only on error, whatever the path.
        assert!(included(opts(true, false, false), "/v1/metadata", true));
        assert!(!included(opts(true, true, true), "/v1/graphql", false));
        // Rule 2: with query-log, metadata bodies need the metadata flag; /v2/query does not.
        assert!(included(opts(false, true, false), "/v1/graphql", false));
        assert!(included(opts(false, true, false), "/v2/query", false));
        assert!(!included(opts(false, true, false), "/v1/metadata", false));
        assert!(!included(opts(false, true, false), "/v1/query", false));
        assert!(included(opts(false, true, true), "/v1/query", false));
        // Default: never.
        assert!(!included(opts(false, false, true), "/v1/graphql", true));
    }

    #[test]
    fn query_summaries() {
        let raw = |s: &'static str| s.as_bytes();
        assert_eq!(
            summary(
                opts(false, false, false),
                Some(raw(r#"{"type":"bulk","args":[]}"#))
            ),
            serde_json::json!({"type": "bulk"})
        );
        assert_eq!(
            summary(opts(false, false, false), Some(raw(r#"{"query":"{a}"}"#))),
            serde_json::json!({"type": null})
        );
        assert_eq!(
            summary(opts(false, false, false), Some(raw(r#"[{"type":"x"}]"#))),
            serde_json::json!({"type": null})
        );
        assert_eq!(
            summary(
                opts(true, false, false),
                Some(raw(r#"{"operationName":"Q"}"#))
            ),
            serde_json::json!({"operationName": "Q"})
        );
        assert_eq!(
            summary(opts(true, false, false), None),
            serde_json::json!({"operationName": null})
        );
    }

    #[test]
    fn ip_from_headers() {
        let mut h = HeaderMap::new();
        let peer: SocketAddr = "10.0.0.9:5555".parse().unwrap();
        assert_eq!(
            client_ip(forwarded_for(&h).as_ref(), Some(peer)),
            "10.0.0.9"
        );
        h.insert("x-forwarded-for", "1.2.3.4, 10.0.0.1".parse().unwrap());
        h.insert("x-real-ip", "5.6.7.8".parse().unwrap());
        assert_eq!(
            client_ip(forwarded_for(&h).as_ref(), Some(peer)),
            "1.2.3.4, 10.0.0.1"
        );
    }
}
