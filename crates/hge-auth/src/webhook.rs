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

    fn method(&self) -> &'static str {
        match self.config.mode {
            HookMode::Get => "GET",
            HookMode::Post => "POST",
        }
    }

    /// A V2 `webhook-log` (`Hasura.Server.Logging.WebHookLog`):
    /// `{"status_code","url","method","http_error","response","message"}`.
    fn log(
        &self,
        level: tracing::Level,
        status: Option<u16>,
        http_error: Option<Value>,
        response: Option<&[u8]>,
        message: Option<&str>,
    ) {
        let detail = || {
            serde_json::json!({
            "status_code": status,
            "url": self.config.url,
            "method": self.method(),
            "http_error": http_error,
            "response": response.map(String::from_utf8_lossy),
            "message": message,
            })
        };
        // Built only when the log is enabled.
        match level {
            tracing::Level::ERROR => tracing::error!(target: "webhook-log", detail = %detail()),
            tracing::Level::WARN => tracing::warn!(target: "webhook-log", detail = %detail()),
            _ => tracing::info!(target: "webhook-log", detail = %detail()),
        }
    }

    async fn call(&self, request: reqwest::RequestBuilder) -> Result<Authenticated, AuthError> {
        // V2's `logAndThrow`: an http-client exception, reading the body included.
        let failed = |e: reqwest::Error| {
            let http_error = crate::http_exception::to_json(&e, self.method(), &self.config.url);
            self.log(tracing::Level::ERROR, None, Some(http_error), None, None);
            AuthError::internal("webhook authentication request failed")
        };
        let resp = request.send().await.map_err(failed)?;
        let status = resp.status();
        let set_cookies: Vec<HeaderValue> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .cloned()
            .collect();
        let body = resp.bytes().await.map_err(failed)?;
        let log_error =
            |status| self.log(tracing::Level::ERROR, Some(status), None, Some(&body), None);
        match status.as_u16() {
            200 => {
                let (mut auth, obj) = match parse_response(&body) {
                    Ok(parsed) => parsed,
                    Err(Rejected::Decode(e)) => {
                        log_error(200);
                        return Err(e);
                    }
                    Err(Rejected::Session(e)) => return Err(e),
                };
                self.log(tracing::Level::INFO, Some(200), None, None, None);
                auth.expires_at = expiry(&obj, SystemTime::now(), |message| {
                    self.log(
                        tracing::Level::WARN,
                        Some(200),
                        None,
                        Some(&body),
                        Some(message),
                    )
                });
                auth.set_cookies = set_cookies;
                Ok(auth)
            }
            401 => {
                log_error(401);
                Err(AuthError::unauthorized(
                    "Authentication hook unauthorized this request",
                ))
            }
            other => {
                log_error(other);
                Err(AuthError::internal(
                    "Invalid response from authorization hook",
                ))
            }
        }
    }
}

/// Why a 200 answer was refused: V2 logs a body it cannot decode, not a bad session.
enum Rejected {
    Decode(AuthError),
    Session(AuthError),
}

/// A 200 body: an object of strings. `x-hasura-*` keys are the session; the role is
/// required. The expiry is left to [`expiry`].
fn parse_response(body: &[u8]) -> Result<(Authenticated, Map<String, Value>), Rejected> {
    let invalid = |detail: String| {
        Rejected::Decode(AuthError::internal(format!(
            "Invalid response from authorization hook: {detail}"
        )))
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
            Rejected::Session(AuthError::bad_request(
                Code::InvalidParams,
                format!("\"{ROLE_HEADER}\" not found in session variables"),
            ))
        })?;
    let auth = Authenticated {
        session: crate::make_session(role, vars, false),
        expires_at: None,
        set_cookies: vec![],
    };
    Ok((auth, obj))
}

/// V2's webhook expiry: `Cache-Control` (keys are case-sensitive) through `parseMaxAge`,
/// else `Expires`. Each parse failure is passed to `warn` with V2's text, and the next
/// source is tried.
fn expiry(
    obj: &Map<String, Value>,
    now: SystemTime,
    mut warn: impl FnMut(&str),
) -> Option<SystemTime> {
    if let Some(cc) = obj.get("Cache-Control").and_then(Value::as_str) {
        match cache_control::parse(cc).map(|cc| cc.max_age.or(cc.s_max_age)) {
            Ok(Some(age)) => return Some(cache_control::add_secs(now, age)),
            Ok(None) => warn("could not find max-age/s-maxage"),
            Err(e) => warn(&e),
        }
    }
    let expires = obj.get("Expires").and_then(Value::as_str)?;
    cache_control::parse_http_date(expires)
        .inspect_err(|_| warn("Value of Expires header is not a valid timestamp"))
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
        let parse = |body: &[u8]| parse_response(body).map(|(auth, _)| auth);
        let ok = parse(br#"{"X-Hasura-Role":"user","X-Hasura-User-Id":"1","other":"x"}"#)
            .ok()
            .unwrap();
        assert_eq!(ok.session.role.as_str(), "user");
        assert_eq!(ok.session.variables.get("x-hasura-user-id"), Some("1"));
        assert_eq!(ok.session.variables.get("other"), None);

        let Err(Rejected::Session(err)) = parse(br#"{"X-Hasura-User-Id":"1"}"#) else {
            panic!("a missing role is a session error")
        };
        assert_eq!(err.status, http::StatusCode::BAD_REQUEST);
        assert_eq!(err.error.code, Code::InvalidParams);
        assert_eq!(
            err.error.message,
            "\"x-hasura-role\" not found in session variables"
        );

        let Err(Rejected::Decode(err)) = parse(br#"{"X-Hasura-Role":"user","X-Hasura-User-Id":1}"#)
        else {
            panic!("a non-string value is a decode error")
        };
        assert_eq!(err.status, http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            err.error
                .message
                .starts_with("Invalid response from authorization hook: ")
        );
    }

    #[test]
    fn expiries() {
        let now = SystemTime::UNIX_EPOCH;
        let at = |body: Value| {
            let mut warnings = vec![];
            let t = expiry(body.as_object().unwrap(), now, |w| {
                warnings.push(w.to_owned())
            });
            (t, warnings)
        };
        let (t, w) = at(serde_json::json!({"Cache-Control": "max-age=600"}));
        assert_eq!(t, Some(now + std::time::Duration::from_secs(600)));
        assert!(w.is_empty());
        let (t, w) = at(serde_json::json!({"Expires": "Wed, 21 Oct 2015 07:28:00 GMT"}));
        assert!(t.is_some() && w.is_empty());
        let (t, w) = at(serde_json::json!({"Cache-Control": "public", "Expires": "nope"}));
        assert_eq!(t, None);
        assert_eq!(
            w,
            [
                "could not find max-age/s-maxage",
                "Value of Expires header is not a valid timestamp"
            ]
        );
        let (_, w) = at(serde_json::json!({"Cache-Control": "max-age=x"}));
        assert_eq!(w, ["could not parse max-age/s-maxage value"]);
    }
}
