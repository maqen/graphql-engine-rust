//! The `test_webhook_transform` metadata API command (§12), minus the HTTP route.

use hge_core::{Code, Error};
use serde_json::{Map, Value, json};

use crate::apply::{RequestContext, apply_request_transform};
use crate::http::Request;
use crate::metadata::{RequestTransform, ResponseTransform, parse_optional, validate_transforms};

/// One `{{VAR}}`-or-text segment of a test URL template.
enum Segment<'a> {
    Text(&'a str),
    Var(&'a str),
}

/// V2's URL template parser for this command: `many1 (env <|> literal <|> "{")`, where
/// `env` is `{{` + text without `}` + `}}`. Only an empty template fails to parse.
fn segments(s: &str) -> Option<Vec<Segment<'_>>> {
    if s.is_empty() {
        return None;
    }
    let mut out = vec![];
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("{{") {
            let end = after.find('}').unwrap_or(after.len());
            if after[end..].starts_with("}}") {
                out.push(Segment::Var(&after[..end]));
                rest = &after[end + 2..];
                continue;
            }
        }
        if let Some(r) = rest.strip_prefix('{') {
            out.push(Segment::Text("{"));
            rest = r;
            continue;
        }
        let end = rest.find('{').unwrap_or(rest.len());
        out.push(Segment::Text(&rest[..end]));
        rest = &rest[end..];
    }
    Some(out)
}

fn missing_env(var: &str) -> Error {
    Error::new(
        Code::NotFound,
        format!(
            "Missing Env Var: {var}. For security reasons when testing request options real environment variable values are not available. Please enter a mock value for {var} in the Sample Env Variables list. See https://hasura.io/docs/latest/graphql/core/actions/rest-connectors/#action-transforms-sample-context"
        ),
    )
}

fn interpolate(template: &str, env: &Map<String, Value>) -> Result<String, Error> {
    let segments =
        segments(template).ok_or_else(|| Error::new(Code::ParseFailed, "Invalid Url Template"))?;
    let mut out = String::new();
    for s in segments {
        match s {
            Segment::Text(t) => out.push_str(t),
            Segment::Var(v) => match env.get(v) {
                Some(Value::String(val)) => out.push_str(val),
                _ => return Err(missing_env(v)),
            },
        }
    }
    Ok(out)
}

fn parse_failed(message: impl Into<String>, path: &str) -> Error {
    Error::new(Code::ParseFailed, message).with_path(path)
}

fn transform_failed(internal: Vec<Value>) -> Error {
    Error::validation("request transform validation failed").with_internal(internal.into())
}

/// The decoded request body as `test_webhook_transform` reports it: JSON if it parses,
/// else the text as a JSON string (form bodies), else `null`.
fn decode_body(body: &[u8]) -> Value {
    if let Ok(v) = serde_json::from_slice(body) {
        return v;
    }
    std::str::from_utf8(body)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&format!("\"{s}\"")).ok())
        .unwrap_or(Value::Null)
}

/// Runs `test_webhook_transform` on its `args` object: validates both transforms, applies
/// the request transform to a sample `GET` request and returns
/// `{"webhook_url", "method", "headers", "body"}`. Errors carry the path `$.args`.
pub fn test_webhook_transform(args: &Value) -> Result<Value, Error> {
    test(args).map_err(|e| {
        if e.path == "$" {
            e.with_path("$.args")
        } else {
            e
        }
    })
}

fn test(args: &Value) -> Result<Value, Error> {
    let o = args.as_object().ok_or_else(|| {
        parse_failed(
            "parsing TestWebhookTransform failed, expected Object",
            "$.args",
        )
    })?;
    let env = match o.get("env") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(parse_failed("expected Object", "$.args.env")),
    };
    let request = parse_optional(
        o.get("request_transform"),
        "args.request_transform",
        RequestTransform::from_json,
    )?
    .ok_or_else(|| parse_failed("key \"request_transform\" not found", "$.args"))?;
    let response = parse_optional(
        o.get("response_transform"),
        "args.response_transform",
        ResponseTransform::from_json,
    )?;
    let body = o
        .get("body")
        .ok_or_else(|| parse_failed("key \"body\" not found", "$.args"))?;
    validate_transforms(Some(&request), response.as_ref())?;

    let url = match o.get("webhook_url") {
        Some(Value::String(t)) => interpolate(t, &env)?,
        Some(Value::Object(w)) => match w.get("from_env") {
            Some(Value::String(var)) => match env.get(var) {
                Some(Value::String(v)) => v.clone(),
                _ => return Err(Error::new(Code::NotFound, "Missing Env Var")),
            },
            _ => {
                return Err(parse_failed(
                    "key \"from_env\" not found",
                    "$.args.webhook_url",
                ));
            }
        },
        _ => return Err(parse_failed("key \"webhook_url\" not found", "$.args")),
    };
    let mut headers = vec![];
    if let Some(hs) = o.get("request_headers").filter(|v| !v.is_null()) {
        let pairs = hs
            .as_array()
            .ok_or_else(|| parse_failed("expected Array", "$.args.request_headers"))?;
        for (i, pair) in pairs.iter().enumerate() {
            match pair.as_array().map(Vec::as_slice) {
                Some([Value::String(k), Value::String(v)]) => {
                    headers.push((k.clone(), interpolate(v, &env)?));
                }
                _ => {
                    return Err(parse_failed(
                        "expected a [name, value] pair of Strings",
                        &format!("$.args.request_headers[{i}]"),
                    ));
                }
            }
        }
    }
    let session = o.get("session_variables").filter(|v| !v.is_null()).cloned();

    let req = Request::new("GET", &url, headers, body.to_string().into_bytes()).map_err(|e| {
        transform_failed(vec![json!({
            "error_code": "Request Initialization Error",
            "message": e,
        })])
    })?;
    let ctx = RequestContext::new(&req, Some(&url), session);
    let out = apply_request_transform(&request, req, ctx).map_err(|b| transform_failed(b.0))?;
    let headers: Vec<Value> = out
        .headers
        .iter()
        .map(|(k, v)| json!([k.to_lowercase(), v]))
        .collect();
    Ok(json!({
        "webhook_url": out.full_url(),
        "method": out.method,
        "headers": headers,
        "body": decode_body(&out.body),
    }))
}
