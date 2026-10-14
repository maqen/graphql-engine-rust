//! One webhook attempt and the V2 bookkeeping around it, shared by event triggers and
//! scheduled events: request headers, invocation-log JSON and the retry decision.

use std::time::Duration;

use hge_transforms::{
    ErrorBundle, Request, RequestContext, RequestTransform, ResponseContext, ResponseTransform,
    apply_request_transform, apply_response_transform,
};
use rand::Rng;
use serde_json::Value;

use crate::json::Json;
use crate::model::{HeaderConf, ResolvedHeader};
use crate::store::{AfterFailure, InvocationRecord};
use crate::webhook::{WebhookClient, WebhookError, WebhookRequest, WebhookResponse};

/// Upper bound on processing one event (`upperBoundEventTriggerTimeout`).
pub const MAX_PROCESSING_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Invocation-log `version` for event triggers.
pub const INVOCATION_VERSION_EVENT: &str = "2";
/// Invocation-log `version` for cron and one-off scheduled events.
pub const INVOCATION_VERSION_SCHEDULED: &str = "1";

/// The result of one attempt.
#[derive(Debug, Clone)]
pub enum AttemptResult {
    /// 2xx.
    Success(WebhookResponse),
    /// Any other status.
    Status(WebhookResponse),
    /// No response (connection failure, timeout, invalid URL).
    Client(WebhookError),
    /// Engine-side failure, e.g. the processing timeout (`HOther`).
    Internal(String),
}

impl AttemptResult {
    pub fn is_success(&self) -> bool {
        matches!(self, AttemptResult::Success(_))
    }

    /// `Retry-After` in whole seconds, only on non-2xx responses and only when > 0.
    pub fn retry_after(&self) -> Option<i64> {
        match self {
            AttemptResult::Status(r) => r
                .header("retry-after")
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|s| *s > 0),
            _ => None,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            AttemptResult::Success(r) | AttemptResult::Status(r) => Some(r.status),
            AttemptResult::Internal(_) => Some(500),
            AttemptResult::Client(_) => None,
        }
    }
}

/// Trace ids propagated to the webhook as B3 headers (V2's default propagator).
#[derive(Debug, Clone)]
pub struct TraceIds {
    pub trace_id: String,
    pub parent_span_id: String,
}

impl TraceIds {
    /// Continue the trace recorded with the event (its `trace_context.trace_id`) or start a
    /// new one.
    pub fn for_event(trace_context: Option<&Json>) -> TraceIds {
        let trace_id = trace_context
            .and_then(|t| t.get("trace_id"))
            .and_then(Json::as_str)
            .filter(|t| t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned)
            .unwrap_or_else(|| random_hex(16));
        TraceIds {
            trace_id,
            parent_span_id: random_hex(8),
        }
    }

    fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("X-B3-TraceId".to_owned(), self.trace_id.clone()),
            ("X-B3-SpanId".to_owned(), random_hex(8)),
            ("X-B3-ParentSpanId".to_owned(), self.parent_span_id.clone()),
            ("X-B3-Sampled".to_owned(), "1".to_owned()),
        ]
    }
}

/// A new `hasura.tracecontext` value for events created by an API request (V2 records the
/// request's trace so the delivery continues it).
pub fn new_trace_context() -> String {
    format!(
        r#"{{"trace_id":"{}","span_id":"{}","sampling_state":"1"}}"#,
        random_hex(16),
        random_hex(8)
    )
}

fn random_hex(bytes: usize) -> String {
    let mut rng = rand::rng();
    (0..bytes)
        .map(|_| format!("{:02x}", rng.random::<u8>()))
        .collect()
}

fn is_default_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-type") || name.eq_ignore_ascii_case("user-agent")
}

/// V2's `prepareHeaders` + `addDefaultHeaders`: `Content-Type` and `User-Agent` first (they
/// win over configured headers of the same name), then the configured headers. Returns the
/// headers to send and their invocation-log form.
pub fn prepare_headers(
    user_agent: &str,
    configured: &[ResolvedHeader],
) -> (Vec<(String, String)>, Vec<HeaderConf>) {
    let mut send = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), user_agent.to_owned()),
    ];
    let mut log = vec![
        HeaderConf::plain("Content-Type", "application/json"),
        HeaderConf::plain("User-Agent", user_agent),
    ];
    for h in configured {
        if is_default_header(&h.conf.name) {
            continue;
        }
        send.push((h.conf.name.clone(), h.value.clone()));
        log.push(h.conf.clone());
    }
    (send, log)
}

/// A trigger's request and response transforms, parsed.
#[derive(Debug, Clone, Default)]
pub struct Transforms {
    pub request: Option<RequestTransform>,
    pub response: Option<ResponseTransform>,
}

impl Transforms {
    /// Parse the metadata JSON; the error is the inconsistency message. Kriti templates are
    /// only checked when an event is delivered (V2 checks them in the metadata API, not when
    /// metadata is loaded), so a bad template fails deliveries instead of the trigger.
    pub fn parse(request: Option<&Value>, response: Option<&Value>) -> Result<Self, String> {
        let request = match request {
            None | Some(Value::Null) => None,
            Some(v) => Some(RequestTransform::from_json(v).map_err(|e| e.to_string())?),
        };
        let response = match response {
            None | Some(Value::Null) => None,
            Some(v) => Some(ResponseTransform::from_json(v).map_err(|e| e.to_string())?),
        };
        Ok(Transforms { request, response })
    }
}

/// The request one attempt sends.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The invocation log's `payload`: the body sent, as JSON (`null` when it is not).
    pub logged_payload: Json,
    /// `$request` of the response transform: the context the request transform saw, or
    /// `null` without one.
    request_context: Value,
}

/// V2's `mkRequest`: a `POST` of `body` to `url`, put through the request transform if
/// there is one. The transform sees the body, the resolved URL as `$base_url` and
/// `session_variables`. A failing transform is not an attempt: V2 marks the event as an
/// error without logging an invocation.
pub fn build_request(
    transforms: &Transforms,
    url: &str,
    headers: Vec<(String, String)>,
    body: Json,
    session_variables: Value,
) -> Result<Outgoing, ErrorBundle> {
    let text = body.encode().into_bytes();
    let untransformed = |headers, text, body| Outgoing {
        method: "POST".to_owned(),
        url: url.to_owned(),
        headers,
        body: text,
        logged_payload: body,
        request_context: Value::Null,
    };
    let Some(t) = &transforms.request else {
        return Ok(untransformed(headers, text, body));
    };
    // An unusable URL fails the attempt itself, as without a transform.
    let request = match Request::new("POST", url, headers.clone(), text.clone()) {
        Ok(r) => r,
        Err(_) => return Ok(untransformed(headers, text, body)),
    };
    let ctx = RequestContext::new(&request, Some(url), Some(session_variables));
    let request_context = ctx.to_response_json();
    let sent = apply_request_transform(t, request, ctx)?;
    let logged_payload = std::str::from_utf8(&sent.body)
        .ok()
        .and_then(|s| Json::parse(s).ok())
        .unwrap_or(Json::Null);
    Ok(Outgoing {
        url: sent.full_url(),
        method: sent.method,
        headers: sent.headers,
        body: sent.body,
        logged_payload,
        request_context,
    })
}

/// Apply the response transform to a 2xx response. The new body is the transform's result,
/// encoded like aeson; a failure is an engine-side error carrying the error bundle.
pub fn transform_response(
    transforms: &Transforms,
    result: AttemptResult,
    sent: &Outgoing,
    session_variables: &Value,
) -> AttemptResult {
    let Some(t) = &transforms.response else {
        return result;
    };
    let mut resp = match result {
        AttemptResult::Success(resp) => resp,
        other => return other,
    };
    let ctx = ResponseContext {
        request: sent.request_context.clone(),
        status: resp.status,
        session_variables: session_variables.clone(),
    };
    match apply_response_transform(t, &resp.body, ctx) {
        Ok(body) => {
            resp.body = Json::from(&body).encode().into_bytes();
            AttemptResult::Success(resp)
        }
        Err(bundle) => AttemptResult::Internal(Json::from(&bundle.to_json()).encode()),
    }
}

/// Everything needed for one attempt.
pub struct Attempt {
    pub request: Outgoing,
    /// HTTP response timeout.
    pub http_timeout: Duration,
    /// Bound on the whole attempt (`min(30 min, timeout)`).
    pub processing_timeout: Duration,
    /// Message used when `processing_timeout` elapses.
    pub timeout_message: String,
    pub trace: TraceIds,
}

/// Perform the attempt. Never fails: every failure is an [`AttemptResult`].
pub async fn attempt(client: &dyn WebhookClient, a: Attempt) -> AttemptResult {
    let mut headers = a.trace.headers();
    headers.extend(a.request.headers);
    let req = WebhookRequest {
        method: a.request.method,
        url: a.request.url,
        headers,
        body: a.request.body,
        timeout: a.http_timeout,
    };
    match tokio::time::timeout(a.processing_timeout, client.post(req)).await {
        Err(_) => AttemptResult::Internal(a.timeout_message),
        Ok(Ok(resp)) if (200..300).contains(&resp.status) => AttemptResult::Success(resp),
        Ok(Ok(resp)) => AttemptResult::Status(resp),
        Ok(Err(e)) => AttemptResult::Client(e),
    }
}

/// `SerializableBlob`: a string when the bytes are UTF-8, else `["Base64", "<b64>"]`.
fn blob(bytes: &[u8]) -> Json {
    match std::str::from_utf8(bytes) {
        Ok(s) => Json::str(s),
        Err(_) => Json::Array(vec![Json::str("Base64"), Json::String(base64(bytes))]),
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The `client_error.message` for an attempt without a response.
fn client_error_message(e: &WebhookError, url: &str, timeout: Duration) -> String {
    match e {
        WebhookError::InvalidUrl(msg) => Json::object([
            ("message", Json::str(msg)),
            ("type", Json::str("invalid_url")),
        ])
        .encode(),
        WebhookError::Http { message, .. } => {
            let request = request_json(url, timeout);
            Json::object([
                ("message", Json::str(message)),
                ("request", request),
                ("type", Json::str("http_exception")),
            ])
            .encode()
        }
    }
}

/// A description of the request, shaped like V2's `encodeHTTPRequestJSON`.
fn request_json(url: &str, timeout: Duration) -> Json {
    let parsed = reqwest::Url::parse(url).ok();
    let secure = parsed.as_ref().is_some_and(|u| u.scheme() == "https");
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    let port = parsed
        .as_ref()
        .and_then(|u| u.port_or_known_default())
        .unwrap_or(0);
    let path = parsed
        .as_ref()
        .map(|u| u.path().to_owned())
        .unwrap_or_default();
    let query = parsed
        .as_ref()
        .and_then(|u| u.query().map(|q| format!("?{q}")))
        .unwrap_or_default();
    Json::object([
        ("host", Json::String(host)),
        ("method", Json::str("POST")),
        ("path", Json::String(path)),
        ("port", Json::int(i64::from(port))),
        ("queryString", Json::String(query)),
        (
            "responseTimeout",
            Json::String(format!("ResponseTimeoutMicro {}", timeout.as_micros())),
        ),
        ("secure", Json::Bool(secure)),
    ])
}

/// Build the invocation-log row for an attempt.
///
/// `request_payload` is the body that was sent (`null` when the attempt timed out).
pub fn invocation(
    version: &str,
    request_payload: Json,
    log_headers: &[HeaderConf],
    result: &AttemptResult,
    url: &str,
    http_timeout: Duration,
) -> InvocationRecord {
    let request = Json::object([
        (
            "headers",
            Json::Array(log_headers.iter().map(HeaderConf::to_json).collect()),
        ),
        ("payload", request_payload),
        ("version", Json::str(version)),
    ]);
    let client_error = |message: Json| {
        Json::object([
            ("data", Json::object([("message", message)])),
            ("type", Json::str("client_error")),
            ("version", Json::str(version)),
        ])
    };
    let response = match result {
        AttemptResult::Success(r) => Json::object([
            (
                "data",
                Json::object([
                    ("body", blob(&r.body)),
                    (
                        "headers",
                        Json::Array(
                            r.headers
                                .iter()
                                .map(|(k, v)| HeaderConf::plain(k, v).to_json())
                                .collect(),
                        ),
                    ),
                    ("status", Json::int(i64::from(r.status))),
                ]),
            ),
            ("type", Json::str("webhook_response")),
            ("version", Json::str(version)),
        ]),
        AttemptResult::Status(r) => client_error(blob(&r.body)),
        AttemptResult::Client(e) => {
            client_error(Json::String(client_error_message(e, url, http_timeout)))
        }
        // `J.encode` of a Haskell String: the message as a JSON string literal.
        AttemptResult::Internal(msg) => client_error(Json::String(Json::str(msg).encode())),
    };
    InvocationRecord {
        status: result.status().map(i32::from),
        request: request.encode(),
        response: response.encode(),
    }
}

/// V2's `retryOrSetError`: retry while tries remain or the webhook asked for a retry with
/// `Retry-After`; `tries` is the count before this attempt.
pub fn after_failure(
    tries: i64,
    num_retries: i64,
    interval_secs: i64,
    result: &AttemptResult,
) -> AfterFailure {
    let retry_after = result.retry_after();
    if tries >= num_retries && retry_after.is_none() {
        AfterFailure::SetError
    } else {
        AfterFailure::Retry(retry_after.unwrap_or(interval_secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::HeaderValue;

    fn resp(status: u16, headers: &[(&str, &str)], body: &str) -> WebhookResponse {
        WebhookResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn bad_template_fails_delivery_not_loading() {
        let bad = serde_json::json!({"version": 2, "url": "{{ unparsed"});
        let t = Transforms::parse(Some(&bad), None).expect("loads like V2");
        let body = Json::from(&serde_json::json!({"a": 1}));
        let r = build_request(&t, "http://h/x", vec![], body, Value::Null);
        assert!(r.is_err());
    }

    #[test]
    fn retry_rules() {
        let fail = AttemptResult::Status(resp(500, &[], "x"));
        assert_eq!(after_failure(0, 0, 10, &fail), AfterFailure::SetError);
        assert_eq!(after_failure(0, 2, 10, &fail), AfterFailure::Retry(10));
        assert_eq!(after_failure(2, 2, 10, &fail), AfterFailure::SetError);
        let ra = AttemptResult::Status(resp(503, &[("Retry-After", "7")], ""));
        assert_eq!(after_failure(5, 0, 10, &ra), AfterFailure::Retry(7));
        for bad in ["0", "-1", "+7", "1.5", "Wed, 21 Oct 2015 07:28:00 GMT"] {
            let r = AttemptResult::Status(resp(503, &[("retry-after", bad)], ""));
            assert_eq!(after_failure(0, 0, 10, &r), AfterFailure::SetError, "{bad}");
        }
        // Retry-After on a 2xx is ignored (and success never gets here anyway).
        assert_eq!(
            AttemptResult::Success(resp(200, &[("retry-after", "5")], "")).retry_after(),
            None
        );
    }

    #[test]
    fn default_headers_win() {
        let configured = vec![
            ResolvedHeader {
                conf: HeaderConf::plain("content-type", "text/plain"),
                value: "text/plain".into(),
            },
            ResolvedHeader {
                conf: HeaderConf {
                    name: "X-Key".into(),
                    value: HeaderValue::Env("KEY".into()),
                },
                value: "secret".into(),
            },
        ];
        let (send, log) = prepare_headers("hasura-graphql-engine/v2", &configured);
        assert_eq!(
            send,
            vec![
                ("Content-Type".to_owned(), "application/json".to_owned()),
                (
                    "User-Agent".to_owned(),
                    "hasura-graphql-engine/v2".to_owned()
                ),
                ("X-Key".to_owned(), "secret".to_owned()),
            ]
        );
        assert_eq!(log.len(), 3);
        assert_eq!(
            log[2].to_json().encode(),
            r#"{"name":"X-Key","value_from_env":"KEY"}"#
        );
    }

    #[test]
    fn invocation_json() {
        let (_, log) = prepare_headers("ua", &[]);
        let ok = invocation(
            "2",
            Json::parse(r#"{"b":1,"a":2}"#).unwrap(),
            &log,
            &AttemptResult::Success(resp(200, &[("content-type", "text/plain")], "ok")),
            "http://x",
            Duration::from_secs(60),
        );
        assert_eq!(ok.status, Some(200));
        assert_eq!(
            ok.request,
            r#"{"headers":[{"name":"Content-Type","value":"application/json"},{"name":"User-Agent","value":"ua"}],"payload":{"a":2,"b":1},"version":"2"}"#
        );
        assert_eq!(
            ok.response,
            r#"{"data":{"body":"ok","headers":[{"name":"content-type","value":"text/plain"}],"status":200},"type":"webhook_response","version":"2"}"#
        );
        let failed = invocation(
            "2",
            Json::Null,
            &log,
            &AttemptResult::Status(resp(404, &[], "nope")),
            "http://x",
            Duration::from_secs(60),
        );
        assert_eq!(
            failed.response,
            r#"{"data":{"message":"nope"},"type":"client_error","version":"2"}"#
        );
        let timeout = invocation(
            "2",
            Json::Null,
            &log,
            &AttemptResult::Internal("Event Trigger \"t\" timed out while processing.".into()),
            "http://x",
            Duration::from_secs(60),
        );
        assert_eq!(timeout.status, Some(500));
        assert_eq!(
            timeout.response,
            r#"{"data":{"message":"\"Event Trigger \\\"t\\\" timed out while processing.\""},"type":"client_error","version":"2"}"#
        );
        let conn = invocation(
            "2",
            Json::Null,
            &log,
            &AttemptResult::Client(WebhookError::Http {
                message: "Connection failure".into(),
                timeout: false,
            }),
            "http://localhost:9/hook?a=1",
            Duration::from_secs(60),
        );
        assert_eq!(conn.status, None);
        assert!(conn.response.contains(r#"\"type\":\"http_exception\""#));
        assert!(conn.response.contains(r#"\"queryString\":\"?a=1\""#));
    }

    #[test]
    fn base64_blob() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"\xff\xfe"), "//4=");
        assert_eq!(blob(b"\xff").encode(), r#"["Base64","/w=="]"#);
    }

    #[test]
    fn trace_ids() {
        let ctx = Json::parse(r#"{"trace_id":"0123456789abcdef0123456789abcdef","span_id":"x"}"#)
            .unwrap();
        assert_eq!(
            TraceIds::for_event(Some(&ctx)).trace_id,
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(TraceIds::for_event(None).trace_id.len(), 32);
    }
}
