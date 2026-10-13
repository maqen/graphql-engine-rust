//! Auth webhook mode (spec 05 §6).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

use hge_core::{Code, RoleName, SessionVariables};
use http::{HeaderMap, HeaderValue, header};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::config::{HookConfig, HookMode};
use crate::{AuthError, Authenticated, ROLE_HEADER, cache_control};

/// Request headers never forwarded in GET mode (V2's `commonClientHeadersIgnored`), plus
/// connection-level headers that cannot be forwarded on a new request.
const IGNORED: [&str; 19] = [
    "content-length",
    "content-md5",
    "user-agent",
    "host",
    "origin",
    "referer",
    "accept",
    "accept-encoding",
    "accept-language",
    "accept-datetime",
    "cache-control",
    "connection",
    "dnt",
    "content-type",
    "transfer-encoding",
    "keep-alive",
    "proxy-connection",
    "te",
    "upgrade",
];

/// Cached webhook answers are bounded; the cache is purged when full.
const CACHE_CAPACITY: usize = 10_000;

pub(crate) struct Webhook {
    config: HookConfig,
    client: reqwest::Client,
    user_agent: HeaderValue,
    /// Opt-in (`HASURA_GRAPHQL_AUTH_HOOK_CACHE`): answers with an expiry are reused until
    /// then for identical requests. V2 OSS calls the webhook on every request.
    cache: Option<Mutex<HashMap<[u8; 32], Authenticated>>>,
}

impl Webhook {
    pub(crate) fn new(config: HookConfig, client: reqwest::Client, user_agent: &str) -> Self {
        let cache = config.cache.then(|| Mutex::new(HashMap::new()));
        Webhook {
            config,
            client,
            user_agent: HeaderValue::from_str(user_agent)
                .unwrap_or_else(|_| HeaderValue::from_static("hasura-graphql-engine")),
            cache,
        }
    }

    pub(crate) fn sends_request_body(&self) -> bool {
        self.config.mode == HookMode::Post && self.config.send_request_body
    }

    pub(crate) async fn authenticate(
        &self,
        headers: &HeaderMap,
        request: Option<&Value>,
    ) -> Result<Authenticated, AuthError> {
        let (builder, cache_key) = match self.config.mode {
            HookMode::Get => {
                let mut forwarded = HeaderMap::with_capacity(headers.len() + 2);
                for (name, value) in headers {
                    if !IGNORED.contains(&name.as_str()) {
                        forwarded.append(name, value.clone());
                    }
                }
                forwarded.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                forwarded.insert(header::USER_AGENT, self.user_agent.clone());
                let key = self.cache.as_ref().map(|_| {
                    let mut h = Sha256::new();
                    for (name, value) in &forwarded {
                        h.update(name.as_str().as_bytes());
                        h.update([0]);
                        h.update(value.as_bytes());
                        h.update([0]);
                    }
                    h.finalize().into()
                });
                (self.client.get(&self.config.url).headers(forwarded), key)
            }
            HookMode::Post => {
                let mut forwarded = Map::new();
                for (name, value) in headers {
                    forwarded.insert(
                        title_case(name.as_str()),
                        Value::String(String::from_utf8_lossy(value.as_bytes()).into_owned()),
                    );
                }
                // Written by hand so the (possibly large) request is not cloned.
                let mut body = String::from("{\"headers\":");
                body.push_str(&Value::Object(forwarded).to_string());
                if self.config.send_request_body {
                    body.push_str(",\"request\":");
                    body.push_str(&request.map_or_else(|| "null".to_owned(), Value::to_string));
                }
                body.push('}');
                let key = self
                    .cache
                    .as_ref()
                    .map(|_| Sha256::digest(body.as_bytes()).into());
                let builder = self
                    .client
                    .post(&self.config.url)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, self.user_agent.clone())
                    .body(body);
                (builder, key)
            }
        };

        if let (Some(cache), Some(key)) = (&self.cache, &cache_key) {
            let cache = cache.lock().expect("webhook cache lock");
            if let Some(hit) = cache.get(key)
                && hit.expires_at.is_some_and(|t| t > SystemTime::now())
            {
                return Ok(hit.clone());
            }
        }

        let result = self.call(builder).await;
        if let (Some(cache), Some(key), Ok(auth)) = (&self.cache, cache_key, &result)
            && auth.expires_at.is_some()
            && auth.set_cookies.is_empty()
        {
            let mut cache = cache.lock().expect("webhook cache lock");
            if cache.len() >= CACHE_CAPACITY {
                let now = SystemTime::now();
                cache.retain(|_, v| v.expires_at.is_some_and(|t| t > now));
                if cache.len() >= CACHE_CAPACITY {
                    cache.clear();
                }
            }
            cache.insert(key, auth.clone());
        }
        result
    }

    async fn call(&self, request: reqwest::RequestBuilder) -> Result<Authenticated, AuthError> {
        let failed = |e: &dyn std::fmt::Display| {
            tracing::error!(target: "webhook-log", url = %self.config.url, error = %e, "auth webhook request failed");
            AuthError::internal("webhook authentication request failed")
        };
        let resp = request.send().await.map_err(|e| failed(&e))?;
        let status = resp.status();
        let set_cookies: Vec<HeaderValue> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .cloned()
            .collect();
        let body = resp.bytes().await.map_err(|e| failed(&e))?;
        match status.as_u16() {
            200 => {
                let mut auth = parse_response(&body)?;
                auth.set_cookies = set_cookies;
                Ok(auth)
            }
            401 => Err(AuthError::unauthorized(
                "Authentication hook unauthorized this request",
            )),
            other => {
                tracing::error!(target: "webhook-log", url = %self.config.url, status = other, "auth webhook returned an unexpected status");
                Err(AuthError::internal(
                    "Invalid response from authorization hook",
                ))
            }
        }
    }
}

/// A 200 body: an object of strings. `x-hasura-*` keys are the session; the role is
/// required; `Cache-Control` / `Expires` keys (case-sensitive) give the expiry.
fn parse_response(body: &[u8]) -> Result<Authenticated, AuthError> {
    let invalid = |detail: String| {
        AuthError::internal(format!(
            "Invalid response from authorization hook: {detail}"
        ))
    };
    let value: Value =
        serde_json::from_slice(body).map_err(|e| invalid(format!("Error in $: {e}")))?;
    let Value::Object(obj) = value else {
        return Err(invalid(format!(
            "Error in $: parsing HashMap ~Text failed, expected Object, but encountered {}",
            crate::jwt::config::kind(&value)
        )));
    };
    let mut vars = SessionVariables::new();
    for (k, v) in &obj {
        let Value::String(s) = v else {
            return Err(invalid(format!(
                "Error in $['{k}']: parsing Text failed, expected String, but encountered {}",
                crate::jwt::config::kind(v)
            )));
        };
        if hge_core::session::is_session_variable(k) {
            vars.insert(k, s.as_str());
        }
    }
    let role = vars
        .get(ROLE_HEADER)
        .filter(|r| !r.is_empty())
        .map(RoleName::new)
        .ok_or_else(|| {
            AuthError::bad_request(
                Code::InvalidParams,
                format!("\"{ROLE_HEADER}\" not found in session variables"),
            )
        })?;
    let expires_at = expiry(&obj, SystemTime::now());
    Ok(Authenticated {
        session: crate::make_session(role, vars, false),
        expires_at,
        set_cookies: vec![],
    })
}

fn expiry(obj: &Map<String, Value>, now: SystemTime) -> Option<SystemTime> {
    if let Some(cc) = obj.get("Cache-Control").and_then(Value::as_str) {
        match cache_control::parse(cc) {
            Ok(cc) => {
                if let Some(age) = cc.max_age {
                    return Some(cache_control::add_secs(now, age));
                }
            }
            Err(e) => {
                tracing::warn!(target: "webhook-log", error = %e, "invalid Cache-Control from auth webhook")
            }
        }
    }
    let expires = obj.get("Expires").and_then(Value::as_str)?;
    cache_control::parse_http_date(expires)
        .inspect_err(|e| tracing::warn!(target: "webhook-log", error = %e, "invalid Expires from auth webhook"))
        .ok()
}

/// HTTP/1 clients mostly send `Title-Case` names, and V2 forwards names as received; our
/// HTTP stack lower-cases them, so restore the conventional casing.
fn title_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = true;
    for c in name.chars() {
        out.push(if upper { c.to_ascii_uppercase() } else { c });
        upper = c == '-';
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(title_case("x-hasura-role"), "X-Hasura-Role");
        assert_eq!(title_case("authorization"), "Authorization");
    }

    #[test]
    fn responses() {
        let ok = parse_response(br#"{"X-Hasura-Role":"user","X-Hasura-User-Id":"1","other":"x","Cache-Control":"max-age=600"}"#).unwrap();
        assert_eq!(ok.session.role.as_str(), "user");
        assert_eq!(ok.session.variables.get("x-hasura-user-id"), Some("1"));
        assert_eq!(ok.session.variables.get("other"), None);
        assert!(ok.expires_at.is_some());

        let err = parse_response(br#"{"X-Hasura-User-Id":"1"}"#).unwrap_err();
        assert_eq!(err.status, http::StatusCode::BAD_REQUEST);
        assert_eq!(err.error.code, Code::InvalidParams);
        assert_eq!(
            err.error.message,
            "\"x-hasura-role\" not found in session variables"
        );

        let err = parse_response(br#"{"X-Hasura-Role":"user","X-Hasura-User-Id":1}"#).unwrap_err();
        assert_eq!(err.status, http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            err.error
                .message
                .starts_with("Invalid response from authorization hook: ")
        );

        let t =
            parse_response(br#"{"X-Hasura-Role":"u","Expires":"Wed, 21 Oct 2015 07:28:00 GMT"}"#)
                .unwrap();
        assert!(t.expires_at.is_some());
    }
}
