//! Calling an action handler (V2's `callWebhook`), shared by synchronous actions and the
//! asynchronous action processor.

use std::collections::HashSet;

use bytes::Bytes;
use hge_core::{Code, Error, SessionVariables};
use hge_transforms::{
    Request as TransformRequest, RequestContext, RequestTransform, ResponseContext,
    apply_request_transform, apply_response_transform,
};
use http::HeaderMap;
use serde_json::{Map, Value, json};

use crate::env::Environment;
use crate::registry::{ActionHandler, ConfiguredHeader, HandlerConfig};
use crate::validate;
use hge_metadata::actions::{ActionKind, ActionType};

/// Client request headers: the live request's (sync) or the stored ones (async). They are
/// only read when the action forwards client headers.
pub type ClientHeaders<'a> = &'a HeaderMap;

/// Everything one handler call needs besides the handler itself.
pub struct Call<'a> {
    pub input: &'a Value,
    pub session: &'a SessionVariables,
    /// The GraphQL query text (`null` for async actions).
    pub request_query: Option<&'a str>,
    pub client_headers: ClientHeaders<'a>,
}

/// A validated 2xx response.
#[derive(Debug)]
pub struct HandlerResponse {
    pub body: Value,
    /// `Set-Cookie` headers of the response.
    pub set_cookies: Vec<String>,
}

/// Recursively sort object keys (V2 serialises through aeson's `Map`).
pub fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&m[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Header names the client may not forward, besides `ignored_client_headers`.
fn is_hasura_header(name: &str) -> bool {
    hge_core::session::is_session_variable(name)
}

/// Collect a request's headers as `(name, value)` pairs (values that are not UTF-8 are
/// dropped).
pub fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect()
}

/// The client headers forwarded by `forward_client_headers` (§7.3): `X-Forwarded-Host`,
/// `-User-Agent` and `-Origin`, then every header except `ignored` (lower-cased names) and
/// session variables. Also used by `validate_input` webhooks.
pub fn forwarded_client_headers(
    ignored: &HashSet<String>,
    headers: ClientHeaders<'_>,
) -> Vec<(String, String)> {
    let headers = header_pairs(headers);
    let find = |name: &'static str| {
        headers
            .iter()
            .filter(move |(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let mut out: Vec<(String, String)> = vec![];
    out.extend(find("Host").map(|v| ("X-Forwarded-Host".to_owned(), v)));
    out.extend(find("User-Agent").map(|v| ("X-Forwarded-User-Agent".to_owned(), v)));
    out.extend(find("Origin").map(|v| ("X-Forwarded-Origin".to_owned(), v)));
    out.extend(
        headers
            .iter()
            .filter(|(k, _)| !ignored.contains(&k.to_ascii_lowercase()) && !is_hasura_header(k))
            .cloned(),
    );
    out
}

fn configured_headers(
    handler: &ActionHandler,
    env: &dyn Environment,
) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::with_capacity(handler.headers.len());
    for h in &handler.headers {
        let value = match h {
            ConfiguredHeader::Value { value, .. } => value.resolve(env).map_err(|missing| {
                Error::new(
                    Code::NotFound,
                    format!("template cannot be resolved: {}", missing.join(", ")),
                )
            })?,
            ConfiguredHeader::FromEnv { env: var, .. } => env.get(var).ok_or_else(|| {
                Error::new(
                    Code::NotFound,
                    format!("environment variable '{var}' not set"),
                )
            })?,
        };
        out.push((h.name().to_owned(), value));
    }
    Ok(out)
}

/// The headers sent to the handler (§7.3): first occurrence of each name wins.
fn request_headers(
    handler: &ActionHandler,
    config: &HandlerConfig,
    env: &dyn Environment,
    client: ClientHeaders<'_>,
) -> Result<Vec<(String, String)>, Error> {
    let defaults = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), config.user_agent.clone()),
    ];
    let conf = configured_headers(handler, env)?;
    let client = if handler.forward_client_headers {
        forwarded_client_headers(&handler.ignored_client_headers, client)
    } else {
        vec![]
    };
    let ordered = if config.configured_header_precedence {
        [conf, defaults, client]
    } else {
        [client, defaults, conf]
    };
    let mut seen = HashSet::new();
    Ok(ordered
        .into_iter()
        .flatten()
        .filter(|(k, _)| seen.insert(k.to_ascii_lowercase()))
        .collect())
}

/// The transformed request that was sent (its body moved out to the transport), if a
/// request transform ran. Only rendered to JSON on error paths.
type Transformed = Option<(TransformRequest, Bytes)>;

/// Details attached to internal errors (`extensions.internal`), V2's `ActionInternalError`.
fn internal(
    handler: &ActionHandler,
    call: &Call<'_>,
    transformed: &Transformed,
    error: Value,
    response: Option<Value>,
) -> Value {
    let mut out = Map::new();
    out.insert("error".into(), error);
    out.insert(
        "request".into(),
        json!({
            "url": handler.handler_template,
            "action": {"name": handler.name},
            "input": call.input,
            "request_query": call.request_query,
            "transformed_request": transformed.as_ref().map(|(r, body)| {
                r.to_json_with_body(body, Some(handler.timeout.as_micros()))
            }),
        }),
    );
    if let Some(r) = response {
        out.insert("response".into(), r);
    }
    Value::Object(out)
}

fn response_info(status: u16, headers: &reqwest::header::HeaderMap, body: Value) -> Value {
    let headers: Vec<Value> = headers
        .iter()
        .map(|(k, v)| json!({"name": title_case(k.as_str()), "value": v.to_str().unwrap_or_default()}))
        .collect();
    json!({"status": status, "body": body, "headers": headers})
}

/// V2 reports response header names as the handler sent them. Our HTTP client lowercases
/// them, so we title-case each `-`-separated word (`Content-Type`, `Set-Cookie`), which is how
/// most servers write them.
fn title_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = true;
    for c in name.chars() {
        out.push(if upper { c.to_ascii_uppercase() } else { c });
        upper = c == '-';
    }
    out
}

/// The request body: V2's payload with sorted keys.
fn payload(action: &str, call: &Call<'_>) -> String {
    payload_value(action, call).to_string()
}

fn payload_value(action: &str, call: &Call<'_>) -> Value {
    sorted(&json!({
        "action": {"name": action},
        "input": call.input,
        "request_query": call.request_query,
        "session_variables": call.session.to_json(),
    }))
}

/// A failed handler call, plus the `code` of V2's `QErr`: a 4xx handler's own `code` is only
/// in [`Error::extensions`] (or dropped, when the handler sent `extensions`).
#[derive(Debug)]
pub struct CallError {
    pub error: Error,
    pub code: String,
}

impl CallError {
    fn new(error: Error) -> Self {
        let code = error.code.as_str().to_owned();
        CallError { error, code }
    }

    /// The error as stored in `hdb_action_log.errors` (V2's `ToJSON QErr` with internal
    /// errors hidden): the handler's `extensions` are dropped.
    pub fn log_json(&self) -> Value {
        json!({"error": self.error.message, "path": self.error.path, "code": self.code})
    }
}

impl From<Error> for CallError {
    fn from(error: Error) -> Self {
        CallError::new(error)
    }
}

/// Call the handler and validate its response.
pub async fn call(
    client: &reqwest::Client,
    handler: &ActionHandler,
    config: &HandlerConfig,
    env: &dyn Environment,
    call: &Call<'_>,
) -> Result<HandlerResponse, Error> {
    call_detailed(client, handler, config, env, call)
        .await
        .map_err(|e| e.error)
}

/// [`call`], keeping the error code a 4xx handler sent (for the async action log).
pub async fn call_detailed(
    client: &reqwest::Client,
    handler: &ActionHandler,
    config: &HandlerConfig,
    env: &dyn Environment,
    call: &Call<'_>,
) -> Result<HandlerResponse, CallError> {
    let headers = request_headers(handler, config, env, call.client_headers)?;
    // `action-handler-log` logs the request as built before any transform.
    let mut log_request =
        tracing::event_enabled!(target: "action-handler-log", tracing::Level::INFO)
            .then(|| logged_request(handler, call, &headers))
            .flatten();
    let (req, transformed, session) = match &handler.request_transform {
        None => {
            let body = match &log_request {
                Some(r) => Bytes::from(r.body.clone()),
                None => Bytes::from(payload(&handler.name, call)),
            };
            let mut req = client.post(&handler.url).body(body);
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            (req, None, None)
        }
        Some(t) => {
            let session = call.session.to_json();
            let mut request = transform_request(handler, t, call, headers, session.clone())?;
            // Validated by the transform.
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|e| Error::unexpected(e.to_string()))?;
            let mut req = client.request(method, request.full_url());
            for (k, v) in &request.headers {
                req = req.header(k, v);
            }
            let body = Bytes::from(std::mem::take(&mut request.body));
            (req.body(body.clone()), Some((request, body)), Some(session))
        }
    };
    // The timeout covers receiving the response headers, not the body (§7.2).
    let resp = match tokio::time::timeout(handler.timeout, req.send()).await {
        Ok(Ok(r)) => r,
        Err(_) => {
            let detail = json!({"type": "http_exception", "message": "Response timeout"});
            return Err(Error::unexpected("http exception when calling webhook")
                .with_internal(internal(handler, call, &transformed, detail, None))
                .into());
        }
        Ok(Err(e)) => {
            let kind = if e.is_builder() {
                "invalid_url"
            } else {
                "http_exception"
            };
            let detail = json!({"type": kind, "message": http_error_message(&e)});
            return Err(Error::unexpected("http exception when calling webhook")
                .with_internal(internal(handler, call, &transformed, detail, None))
                .into());
        }
    };
    let status = resp.status().as_u16();
    let resp_headers = resp.headers().clone();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            let detail = json!({"type": "http_exception", "message": http_error_message(&e)});
            return Err(Error::unexpected("http exception when calling webhook")
                .with_internal(internal(handler, call, &transformed, detail, None))
                .into());
        }
    };
    let mut log = || {
        if let Some(request) = log_request.take() {
            let detail = handler_log(handler, config, request, &transformed, bytes.len());
            tracing::info!(target: "action-handler-log", detail = %detail);
        }
    };
    let body: Value = match &handler.response_transform {
        Some(t) => {
            let session = session.unwrap_or_else(|| call.session.to_json());
            // `$request` is the context of the transformed request, if any (§10.5).
            let request = transformed.as_ref().map_or(Value::Null, |(r, body)| {
                let body = serde_json::from_slice(body).unwrap_or(Value::Null);
                RequestContext::from_parts(
                    body,
                    &r.query,
                    Some(&handler.url),
                    Some(session.clone()),
                )
                .to_response_json()
            });
            let ctx = ResponseContext {
                request,
                status,
                session_variables: session,
            };
            let body = apply_response_transform(t, &bytes, ctx)
                .map_err(|bundle| logged(bundle.response_failed()))?;
            log();
            body
        }
        None => {
            log();
            match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    let raw = Value::String(String::from_utf8_lossy(&bytes).into_owned());
                    return Err(Error::unexpected("not a valid json response from webhook")
                        .with_internal(internal(
                            handler,
                            call,
                            &transformed,
                            Value::String(format!("invalid JSON: {e}")),
                            Some(response_info(status, &resp_headers, raw)),
                        ))
                        .into());
                }
            }
        }
    };
    let info = |body: &Value| response_info(status, &resp_headers, body.clone());
    match status {
        200..=299 => {
            if let Err(message) = validate::validate(&handler.output, &body) {
                // V2's `throwUnexpected` here is a 400.
                return Err(Error::new(Code::Unexpected, message)
                    .with_status(400)
                    .with_internal(internal(
                        handler,
                        call,
                        &transformed,
                        Value::String("unexpected response".into()),
                        Some(info(&body)),
                    ))
                    .into());
            }
            let set_cookies = resp_headers
                .get_all(reqwest::header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_owned))
                .collect();
            Ok(HandlerResponse { body, set_cookies })
        }
        400..=499 => {
            let mut error = client_error(&body).with_status(status);
            // V2 attaches the request and response only when the error body does not decode
            // (`modifyQErr addInternalToErr`); a handler's own error goes out as it is.
            if error.code == Code::ParseFailed {
                error = error.with_internal(internal(
                    handler,
                    call,
                    &transformed,
                    Value::String("unexpected response".into()),
                    Some(info(&body)),
                ));
            }
            let code = match body.get("code") {
                Some(Value::String(c)) if error.code != Code::ParseFailed => c.clone(),
                _ => error.code.as_str().to_owned(),
            };
            Err(CallError { error, code })
        }
        _ => Err(CallError::new(
            Error::unexpected("internal error").with_internal(internal(
                handler,
                call,
                &transformed,
                Value::String(format!(
                    "expecting 2xx or 4xx status code, but found {status}"
                )),
                Some(info(&body)),
            )),
        )),
    }
}

/// The request `action-handler-log` shows: V2's request before any transform, with the
/// handler URL parsed like http-client does (no userinfo, default port dropped) and exactly
/// the computed headers. `None` for an unparsable URL, which fails before any response.
fn logged_request(
    handler: &ActionHandler,
    call: &Call<'_>,
    headers: &[(String, String)],
) -> Option<TransformRequest> {
    let mut request = TransformRequest::new(
        "POST",
        &handler.url,
        vec![],
        payload(&handler.name, call).into_bytes(),
    )
    .ok()?;
    request.headers = headers.to_vec();
    Some(request)
}

/// V2's `ActionHandlerLog`: one per handler response (not for HTTP errors or failed
/// transforms), keys in aeson's sorted order. `request` is the request before any transform;
/// `response_size` counts the raw response body.
fn handler_log(
    handler: &ActionHandler,
    config: &HandlerConfig,
    request: TransformRequest,
    transformed: &Transformed,
    response_size: usize,
) -> Value {
    let action_type = match handler.action_type {
        ActionType::Query => "query",
        ActionType::Mutation(ActionKind::Synchronous) => "mutation-synchronous",
        ActionType::Mutation(ActionKind::Asynchronous) => "mutation-asynchronous",
    };
    let timeout = Some(handler.timeout.as_micros());
    let request_size = request.body.len();
    let mut request = sorted(&request.to_json(timeout));
    if config.redact_logs {
        request["body"] = Value::Null;
    }
    let mut out = Map::new();
    out.insert("action_name".into(), handler.name.as_str().into());
    out.insert("action_type".into(), action_type.into());
    out.insert("request".into(), request);
    out.insert("request_size".into(), request_size.into());
    if let Some((r, body)) = transformed {
        let mut trans = sorted(&r.to_json_with_body(body, timeout));
        if config.redact_logs {
            trans["body"] = Value::Null;
        }
        out.insert("request_trans".into(), trans);
    }
    out.insert("response_size".into(), response_size.into());
    if let Some((_, body)) = transformed {
        out.insert("transformed_request_size".into(), body.len().into());
    }
    Value::Object(out)
}

/// Log a transform failure at error level (§7.4) and pass it on.
fn logged(e: Error) -> Error {
    let errors = e.internal.as_deref().cloned().unwrap_or_default();
    tracing::error!(%errors, "{}", e.message);
    e
}

/// Apply the request transform to V2's request (§7.4). The transform sees the original
/// payload, the resolved handler URL and its query parameters.
fn transform_request(
    handler: &ActionHandler,
    transform: &RequestTransform,
    call: &Call<'_>,
    headers: Vec<(String, String)>,
    session: Value,
) -> Result<TransformRequest, Error> {
    let parsed;
    let base = match &handler.transform_base {
        Some(base) => base,
        None => {
            parsed = TransformRequest::new("POST", &handler.url, vec![], vec![]);
            &parsed
        }
    };
    let mut request = base
        .as_ref()
        .map_err(|message| {
            let detail = json!({"type": "invalid_url", "message": message});
            Error::unexpected("http exception when calling webhook")
                .with_internal(internal(handler, call, &None, detail, None))
        })?
        .clone();
    // URL credentials (if any) come first, as in http-client's `applyBasicAuth`.
    request.headers.extend(headers);
    let payload = payload_value(&handler.name, call);
    request.body = payload.to_string().into_bytes();
    let ctx =
        RequestContext::from_parts(payload, &request.query, Some(&handler.url), Some(session));
    apply_request_transform(transform, request, ctx)
        .map_err(|bundle| logged(bundle.request_failed()))
}

fn http_error_message(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "Response timeout".to_owned()
    } else if e.is_connect() {
        format!("Connection failure: {e}")
    } else {
        e.to_string()
    }
}

/// A 4xx body `{message, code?, extensions?}` as a GraphQL error (§8.5).
fn client_error(body: &Value) -> Error {
    let Some(obj) = body.as_object() else {
        return Error::new(
            Code::ParseFailed,
            format!(
                "expected Object, but encountered {}",
                crate::json_type_name(body)
            ),
        );
    };
    let message = match obj.get("message") {
        Some(Value::String(m)) => m.clone(),
        None => return Error::new(Code::ParseFailed, "the key 'message' was not present"),
        Some(other) => {
            return Error::new(
                Code::ParseFailed,
                format!(
                    "expected String, but encountered {}",
                    crate::json_type_name(other)
                ),
            )
            .with_path("$.message");
        }
    };
    // aeson decodes the whole body first, so a bad `code` fails even next to `extensions`.
    let code = match obj.get("code") {
        None | Some(Value::Null) => None,
        Some(Value::String(c)) => Some(c),
        Some(other) => {
            return Error::new(
                Code::ParseFailed,
                format!(
                    "expected String, but encountered {}",
                    crate::json_type_name(other)
                ),
            )
            .with_path("$.code");
        }
    };
    match (obj.get("extensions"), code) {
        (Some(ext), _) if !ext.is_null() => {
            Error::new(Code::Unexpected, message).with_extensions(ext.clone())
        }
        (_, Some(code)) => Error::new(Code::Unexpected, message)
            .with_extensions(json!({"path": "$", "code": code})),
        _ => Error::new(Code::Unexpected, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_errors_map_like_v2() {
        let e = client_error(&json!({"message": "m"}));
        assert_eq!(
            e.to_graphql_json(false),
            json!({"message": "m", "extensions": {"path": "$", "code": "unexpected"}})
        );
        let e = client_error(&json!({"message": "m", "code": "c"}));
        assert_eq!(
            e.to_graphql_json(false),
            json!({"message": "m", "extensions": {"path": "$", "code": "c"}})
        );
        let e = client_error(&json!({"message": "m", "code": "c", "extensions": {"foo": "bar"}}));
        assert_eq!(
            e.to_graphql_json(false),
            json!({"message": "m", "extensions": {"foo": "bar"}})
        );
        let e = client_error(&json!({"message": "m", "extensions": null}));
        assert_eq!(e.extensions, None);
        let e = client_error(&json!({"error": "x"}));
        assert_eq!(
            (e.code, e.message.as_str()),
            (Code::ParseFailed, "the key 'message' was not present")
        );
        let e = client_error(&json!({"message": "m", "code": 1, "extensions": {}}));
        assert_eq!(
            e.to_graphql_json(false),
            json!({"message": "expected String, but encountered Number",
                "extensions": {"path": "$.code", "code": "parse-failed"}})
        );
    }

    #[test]
    fn payload_keys_are_sorted() {
        let mut session = SessionVariables::new();
        session.insert("x-hasura-role", "user");
        session.insert("x-hasura-a", "1");
        let input = json!({"z": 1, "a": {"y": [{"b": 1, "a": 2}], "x": null}});
        let call = Call {
            input: &input,
            session: &session,
            request_query: Some("q"),
            client_headers: &HeaderMap::new(),
        };
        assert_eq!(
            payload("act", &call),
            r#"{"action":{"name":"act"},"input":{"a":{"x":null,"y":[{"a":2,"b":1}]},"z":1},"request_query":"q","session_variables":{"x-hasura-a":"1","x-hasura-role":"user"}}"#
        );
    }

    #[test]
    fn forwards_client_headers() {
        let ignored: HashSet<String> = hge_metadata::actions::DEFAULT_IGNORED_CLIENT_HEADERS
            .iter()
            .map(|h| h.to_ascii_lowercase())
            .collect();
        let mut client = HeaderMap::new();
        for (k, v) in [
            ("host", "example.com"),
            ("user-agent", "curl"),
            ("x-hasura-admin-secret", "s"),
            ("authorization", "Bearer t"),
            ("content-type", "application/json"),
        ] {
            client.insert(k, v.parse().unwrap());
        }
        assert_eq!(
            forwarded_client_headers(&ignored, &client),
            vec![
                ("X-Forwarded-Host".to_owned(), "example.com".to_owned()),
                ("X-Forwarded-User-Agent".to_owned(), "curl".to_owned()),
                ("authorization".to_owned(), "Bearer t".to_owned()),
            ]
        );
    }

    /// A handler that reports what it received; paths containing `fail` answer 500.
    async fn start_handler() -> std::net::SocketAddr {
        async fn any(
            method: http::Method,
            uri: http::Uri,
            headers: HeaderMap,
            body: String,
        ) -> (http::StatusCode, axum::Json<Value>) {
            let mut names: Vec<(String, String)> = header_pairs(&headers)
                .into_iter()
                .filter(|(k, _)| !matches!(k.as_str(), "content-length" | "host" | "accept"))
                .collect();
            names.sort();
            let status = if uri.path().contains("fail") {
                http::StatusCode::INTERNAL_SERVER_ERROR
            } else {
                http::StatusCode::OK
            };
            let (method, uri) = (method.to_string(), uri.to_string());
            let out = json!({"method": method, "uri": uri, "headers": names, "body": body});
            (status, axum::Json(out))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().fallback(any)).await });
        addr
    }

    /// Calls a `json`-typed action with the given definition (handler URL included).
    async fn run(definition: Value) -> Result<HandlerResponse, Error> {
        let mut definition = definition;
        definition["output_type"] = json!("json");
        let metadata = hge_metadata::from_json(json!({
            "version": 3, "sources": [],
            "actions": [{"name": "act", "definition": definition}]
        }))
        .unwrap();
        let output = std::sync::Arc::new(hge_ir::action::ActionOutput {
            ty: hge_ir::action::OutputType::Named("json".into()),
            base: hge_ir::action::OutputBase::CustomScalar,
        });
        let outputs = [("act".to_owned(), output)].into_iter().collect();
        let config = HandlerConfig::default();
        let built = crate::ActionRegistry::build(
            &metadata,
            &outputs,
            std::sync::Arc::new(crate::ProcessEnv),
            config.clone(),
        );
        assert!(
            built.inconsistencies.is_empty(),
            "{:?}",
            built.inconsistencies
        );
        let handler = built.registry.get("act").unwrap();
        let mut session = SessionVariables::new();
        session.insert("x-hasura-role", "admin");
        let input = json!({"id": 1});
        let call_args = Call {
            input: &input,
            session: &session,
            request_query: Some("q"),
            client_headers: &HeaderMap::new(),
        };
        call(
            &config.client,
            handler,
            &config,
            &crate::ProcessEnv,
            &call_args,
        )
        .await
    }

    fn internal_of(e: &Error) -> &Value {
        e.internal.as_deref().unwrap()
    }

    #[tokio::test]
    async fn without_transforms_requests_are_unchanged() {
        let addr = start_handler().await;
        let resp = run(json!({"handler": format!("http://{addr}/plain")}))
            .await
            .unwrap();
        assert_eq!(resp.body["method"], "POST");
        assert_eq!(
            resp.body["body"],
            r#"{"action":{"name":"act"},"input":{"id":1},"request_query":"q","session_variables":{"x-hasura-role":"admin"}}"#
        );
        let e = run(json!({"handler": format!("http://{addr}/fail")}))
            .await
            .unwrap_err();
        assert_eq!(
            internal_of(&e)["request"]["transformed_request"],
            Value::Null
        );
    }

    #[tokio::test]
    async fn transformed_request_is_reported_in_internal() {
        let addr = start_handler().await;
        let e = run(json!({
            "handler": format!("http://{addr}/base"),
            "timeout": 5,
            "request_transform": {"version": 2, "method": "PUT", "url": "{{$base_url}}/fail",
                "query_params": {"id": "{{$body.input.id}}"},
                "request_headers": {"add_headers": {"X-A": "b"}, "remove_headers": ["user-agent"]},
                "body": {"action": "transform", "template": "{\"id\": {{$body.input.id}}}"}}
        }))
        .await
        .unwrap_err();
        assert_eq!(e.message, "internal error");
        assert_eq!(
            internal_of(&e)["request"]["transformed_request"],
            // http-client's `getUri` keeps the query string.
            json!({
                "url": format!("http://{addr}/base/fail?id=1"),
                "method": "PUT",
                "headers": [["Content-Type", "application/json"], ["X-A", "b"]],
                "body": r#"{"id":1}"#,
                "query_string": "?id=1",
                "response_timeout": "5000000",
            })
        );
    }

    #[tokio::test]
    async fn handler_url_credentials_survive_a_transform() {
        let addr = start_handler().await;
        let resp = run(json!({
            "handler": format!("http://user:pa%20ss@{addr}/x"),
            "request_transform": {"method": "PATCH"}
        }))
        .await
        .unwrap();
        assert_eq!(resp.body["method"], "PATCH");
        assert_eq!(resp.body["uri"], "/x");
        // base64("user:pa ss")
        assert!(
            resp.body["headers"]
                .as_array()
                .unwrap()
                .contains(&json!(["authorization", "Basic dXNlcjpwYSBzcw=="]))
        );
    }

    #[tokio::test]
    async fn unsafe_or_unsendable_transform_results_fail_the_transform() {
        let addr = start_handler().await;
        let handler = format!("http://{addr}/x");
        for (transform, message) in [
            (
                json!({"url": "http://h@evil.com/"}),
                "Invalid URL: http://h@evil.com/",
            ),
            (
                json!({"url": format!("http://{}:70000/", addr.ip())}),
                "Invalid URL: http://127.0.0.1:70000/",
            ),
            (
                json!({"request_headers": {"add_headers": {"X-A": "a\r\nX-Evil: 1"}}}),
                "Invalid header: X-A",
            ),
            (json!({"method": "G T"}), "Invalid HTTP method"),
        ] {
            let e = run(json!({"handler": handler, "request_transform": transform}))
                .await
                .unwrap_err();
            assert_eq!(
                (e.code, e.message.as_str()),
                (Code::Unexpected, "Request Transformation Failed")
            );
            assert_eq!(internal_of(&e)[0]["message"], message);
        }
    }
}
