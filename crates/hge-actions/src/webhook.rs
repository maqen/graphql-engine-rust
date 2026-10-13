//! Calling an action handler (V2's `callWebhook`), shared by synchronous actions and the
//! asynchronous action processor.

use std::collections::HashSet;

use hge_core::{Code, Error, SessionVariables};
use http::HeaderMap;
use serde_json::{Map, Value, json};

use crate::env::Environment;
use crate::registry::{ActionHandler, ConfiguredHeader, HandlerConfig};
use crate::validate;

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

fn forwarded_client_headers(
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

/// Details attached to internal errors (`extensions.internal`), V2's `ActionInternalError`.
fn internal(
    handler: &ActionHandler,
    call: &Call<'_>,
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
            "transformed_request": null,
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
        .map(|(k, v)| json!({"name": k.as_str(), "value": v.to_str().unwrap_or_default()}))
        .collect();
    json!({"status": status, "body": body, "headers": headers})
}

/// The request body: V2's payload with sorted keys.
fn payload(action: &str, call: &Call<'_>) -> String {
    sorted(&json!({
        "action": {"name": action},
        "input": call.input,
        "request_query": call.request_query,
        "session_variables": call.session.to_json(),
    }))
    .to_string()
}

/// Call the handler and validate its response.
pub async fn call(
    client: &reqwest::Client,
    handler: &ActionHandler,
    config: &HandlerConfig,
    env: &dyn Environment,
    call: &Call<'_>,
) -> Result<HandlerResponse, Error> {
    if handler.request_transform.is_some() || handler.response_transform.is_some() {
        return Err(Error::new(
            Code::NotSupported,
            "request and response transforms are not supported yet",
        ));
    }
    let headers = request_headers(handler, config, env, call.client_headers)?;
    let mut req = client.post(&handler.url).body(payload(&handler.name, call));
    for (k, v) in &headers {
        req = req.header(k, v);
    }
    // The timeout covers receiving the response headers, not the body (§7.2).
    let resp = match tokio::time::timeout(handler.timeout, req.send()).await {
        Ok(Ok(r)) => r,
        Err(_) => {
            let detail = json!({"type": "http_exception", "message": "Response timeout"});
            return Err(Error::unexpected("http exception when calling webhook")
                .with_internal(internal(handler, call, detail, None)));
        }
        Ok(Err(e)) => {
            let kind = if e.is_builder() {
                "invalid_url"
            } else {
                "http_exception"
            };
            let detail = json!({"type": kind, "message": http_error_message(&e)});
            return Err(Error::unexpected("http exception when calling webhook")
                .with_internal(internal(handler, call, detail, None)));
        }
    };
    let status = resp.status().as_u16();
    let resp_headers = resp.headers().clone();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            let detail = json!({"type": "http_exception", "message": http_error_message(&e)});
            return Err(Error::unexpected("http exception when calling webhook")
                .with_internal(internal(handler, call, detail, None)));
        }
    };
    let body: Value =
        match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                let raw = Value::String(String::from_utf8_lossy(&bytes).into_owned());
                return Err(Error::unexpected("not a valid json response from webhook")
                    .with_internal(internal(
                        handler,
                        call,
                        Value::String(format!("invalid JSON: {e}")),
                        Some(response_info(status, &resp_headers, raw)),
                    )));
            }
        };
    let info = |body: &Value| response_info(status, &resp_headers, body.clone());
    match status {
        200..=299 => {
            if let Err(message) = validate::validate(&handler.output, &body) {
                return Err(
                    Error::new(Code::Unexpected, message).with_internal(internal(
                        handler,
                        call,
                        Value::String("unexpected response".into()),
                        Some(info(&body)),
                    )),
                );
            }
            let set_cookies = resp_headers
                .get_all(reqwest::header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_owned))
                .collect();
            Ok(HandlerResponse { body, set_cookies })
        }
        400..=499 => Err(client_error(&body).with_internal(internal(
            handler,
            call,
            Value::String("unexpected response".into()),
            Some(info(&body)),
        ))),
        _ => Err(Error::unexpected("internal error").with_internal(internal(
            handler,
            call,
            Value::String(format!(
                "expecting 2xx or 4xx status code, but found {status}"
            )),
            Some(info(&body)),
        ))),
    }
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
}
