//! `validate_input` webhooks (spec 04 §10): before a mutation runs, the role's insert,
//! update or delete permission may send its input to an HTTP endpoint, which accepts it with
//! a 2xx or rejects it with a 400 `{"message": …}`.

use std::collections::HashSet;
use std::time::Duration;

use hge_actions::ActionRegistry;
use hge_actions::env::Template;
use hge_core::{Code, Error, Session};
use hge_ir::{EnvValue, InputValidationCall};
use http::HeaderMap;
use serde_json::{Value, json};

/// Make the calls in order; the first rejection or failure is the mutation's error.
pub async fn validate(
    registry: &ActionRegistry,
    calls: &[InputValidationCall],
    session: &Session,
    client_headers: &HeaderMap,
) -> Result<(), Error> {
    for call in calls {
        validate_one(registry, call, session, client_headers).await?;
    }
    Ok(())
}

fn resolve(value: &EnvValue, registry: &ActionRegistry) -> Result<String, Error> {
    match value {
        EnvValue::Template(t) => Template::parse(t)
            .map_err(Error::unexpected)?
            .resolve(&*registry.env)
            .map_err(|missing| {
                Error::new(
                    Code::NotFound,
                    format!("template cannot be resolved: {}", missing.join(", ")),
                )
            }),
        EnvValue::FromEnv(var) => registry.env.get(var).ok_or_else(|| {
            Error::new(
                Code::NotFound,
                format!("environment variable '{var}' not set"),
            )
        }),
    }
}

/// Configured, default and forwarded client headers; the first occurrence of a name wins,
/// with configured or client headers first per `HASURA_GRAPHQL_CONFIGURED_HEADER_PRECEDENCE`.
fn headers(
    registry: &ActionRegistry,
    call: &InputValidationCall,
    client: &HeaderMap,
) -> Result<Vec<(String, String)>, Error> {
    let config = &registry.config;
    let mut configured = Vec::with_capacity(call.webhook.headers.len());
    for (name, value) in &call.webhook.headers {
        configured.push((name.clone(), resolve(value, registry)?));
    }
    let defaults = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), config.user_agent.clone()),
    ];
    let client = if call.webhook.forward_client_headers {
        let ignored: HashSet<String> = hge_metadata::actions::DEFAULT_IGNORED_CLIENT_HEADERS
            .iter()
            .map(|h| h.to_ascii_lowercase())
            .collect();
        hge_actions::webhook::forwarded_client_headers(&ignored, client)
    } else {
        vec![]
    };
    let ordered = if config.configured_header_precedence {
        [configured, defaults, client]
    } else {
        [client, defaults, configured]
    };
    let mut seen = HashSet::new();
    Ok(ordered
        .into_iter()
        .flatten()
        .filter(|(k, _)| seen.insert(k.to_ascii_lowercase()))
        .collect())
}

async fn validate_one(
    registry: &ActionRegistry,
    call: &InputValidationCall,
    session: &Session,
    client_headers: &HeaderMap,
) -> Result<(), Error> {
    let url = resolve(&call.webhook.url, registry)?;
    let body = json!({
        "version": 1,
        "role": session.role.as_str(),
        "session_variables": session.variables.to_json(),
        "data": {"input": call.input},
    });
    let headers = headers(registry, call, client_headers)?;
    let request_info = || {
        json!({
            "url": url,
            "headers": headers.iter().map(|(k, v)| json!({"name": k, "value": v})).collect::<Vec<_>>(),
            "body": body,
        })
    };
    let mut req = registry.config.client.post(&url).body(body.to_string());
    for (k, v) in &headers {
        req = req.header(k, v);
    }
    let http_error = |message: String| {
        Error::unexpected("http exception when validating input data").with_internal(json!({
            "error": {"type": "http_exception", "message": message},
            "request": request_info(),
        }))
    };
    let timeout = Duration::from_secs(call.webhook.timeout_secs);
    let resp = match tokio::time::timeout(timeout, req.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(http_error(e.to_string())),
        Err(_) => return Err(http_error("Response timeout".to_owned())),
    };
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|e| http_error(e.to_string()))?;
    if (200..300).contains(&status) {
        return Ok(());
    }
    let response_info = || {
        json!({
            "status": status,
            "body": String::from_utf8_lossy(&bytes),
        })
    };
    if status == 400 {
        let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
        return match parsed.as_ref().and_then(|b| b.get("message")) {
            Some(Value::String(message)) => {
                Err(Error::new(Code::ValidationFailed, message.clone()).with_path("$"))
            }
            _ => Err(
                Error::unexpected("received invalid response from input validation webhook")
                    .with_internal(json!({
                        "error": "invalid response: expected an object with a string \"message\"",
                        "request": request_info(),
                        "response": response_info(),
                    })),
            ),
        };
    }
    Err(
        Error::unexpected("internal error when validating input data").with_internal(json!({
            "error": format!("expecting 200 or 400 status code, but found {status}"),
            "request": request_info(),
            "response": response_info(),
        })),
    )
}
