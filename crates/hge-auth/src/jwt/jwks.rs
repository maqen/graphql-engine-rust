//! `jwk_url` fetching and refresh (spec 05 §5.7).
//!
//! V2 refreshes from a 60 s ticker when the key set is empty or the expiry derived from
//! `Cache-Control` / `Expires` has passed. We do the same, and also check on the request
//! path (so an instance whose CPU is throttled between requests, as on Cloud Run, still
//! refreshes) and when a token names a `kid` we do not have (key rotation). Fetches are
//! at most one per 60 s and never block a request; the previous keys stay in use until
//! new ones arrive. Unlike V2, refresh works in every JWT mode (V2 only refreshes with an
//! admin secret set; spec index "fix" decision).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::JwtContext;
use super::keys::{self, KeySet};
use crate::cache_control;

const MIN_REFRESH_INTERVAL_MS: u64 = 60_000;
const TICK: Duration = Duration::from_secs(60);

pub(crate) struct JwkSource {
    url: String,
    client: reqwest::Client,
    user_agent: String,
    /// When the keys expire, in Unix milliseconds; 0 = unknown (never refresh).
    expires_ms: AtomicU64,
    /// When the last fetch started, in Unix milliseconds.
    last_attempt_ms: AtomicU64,
    refreshing: AtomicBool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
        .max(1)
}

impl JwkSource {
    pub(crate) fn new(url: String, client: reqwest::Client, user_agent: String) -> Self {
        JwkSource {
            url,
            client,
            user_agent,
            expires_ms: AtomicU64::new(0),
            last_attempt_ms: AtomicU64::new(0),
            refreshing: AtomicBool::new(false),
        }
    }

    /// The startup fetch: retried with exponential backoff from 200 ms, at most 5 retries;
    /// failure is fatal. Unlike V2 the result is kept rather than refetched.
    pub(crate) async fn fetch_at_startup(&self) -> Result<KeySet, String> {
        let mut delay = Duration::from_millis(200);
        let mut attempt = 0;
        loop {
            self.last_attempt_ms.store(now_ms(), Ordering::Relaxed);
            match self.fetch().await {
                Ok(keys) => return Ok(keys),
                Err(e) if attempt >= 5 => return Err(e),
                Err(e) => {
                    tracing::warn!(target: "jwk-refresh-log", url = %self.url, error = %e, "retrying JWK fetch");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                    attempt += 1;
                }
            }
        }
    }

    /// Fetch and parse the key set, recording the expiry from the response headers.
    async fn fetch(&self) -> Result<KeySet, String> {
        let resp = self
            .client
            .get(&self.url)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::USER_AGENT, &self.user_agent)
            .send()
            .await
            .map_err(|e| format!("Error fetching JWK: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "Non-2xx response on fetching JWK from: {}",
                self.url
            ));
        }
        let headers = resp.headers().clone();
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("Error fetching JWK: {e}"))?;
        let keys = keys::parse_jwk_set(&body).map_err(|e| {
            tracing::warn!(target: "jwk-refresh-log", error = %e, "invalid JWK set");
            format!("Error parsing JWK from url: {}", self.url)
        })?;
        if let Some(expiry) = expiry_from_headers(&headers, SystemTime::now()) {
            self.expires_ms.store(to_ms(expiry), Ordering::Relaxed);
        }
        Ok(keys)
    }

    /// Start a background refresh if one is due. Cheap enough for every request.
    pub(crate) fn maybe_refresh(&self, ctx: &JwtContext, kid_unknown: bool) {
        let now = now_ms();
        let expires = self.expires_ms.load(Ordering::Relaxed);
        let due = kid_unknown || ctx.keys.load().is_empty() || (expires != 0 && now >= expires);
        if !due
            || now.saturating_sub(self.last_attempt_ms.load(Ordering::Relaxed))
                < MIN_REFRESH_INTERVAL_MS
            || self.refreshing.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.last_attempt_ms.store(now, Ordering::Relaxed);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.refreshing.store(false, Ordering::Release);
            return;
        };
        let ctx = ctx.self_ref.clone();
        runtime.spawn(async move {
            let Some(ctx) = ctx.upgrade() else { return };
            let Some(jwk) = &ctx.jwk else { return };
            match jwk.fetch().await {
                Ok(keys) => {
                    ctx.keys.store(Arc::new(keys));
                    tracing::info!(target: "jwk-refresh-log", url = %jwk.url, "JWK set refreshed");
                }
                Err(e) => {
                    tracing::error!(target: "jwk-refresh-log", url = %jwk.url, error = %e, "JWK refresh failed")
                }
            }
            jwk.refreshing.store(false, Ordering::Release);
        });
    }
}

/// V2's 60 s refresh ticker; stops when the context is dropped.
pub(super) fn spawn_refresh_ticker(ctx: Weak<JwtContext>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(ctx) = ctx.upgrade() else { return };
            if let Some(jwk) = &ctx.jwk {
                jwk.maybe_refresh(&ctx, false);
            }
        }
    });
}

/// V2's `determineJwkExpiryLifetime`: `Cache-Control` first, then `Expires`. Parse
/// failures are logged and give no expiry (the previous one is kept).
pub(crate) fn expiry_from_headers(
    headers: &http::HeaderMap,
    now: SystemTime,
) -> Option<SystemTime> {
    if let Some(cc) = headers.get(http::header::CACHE_CONTROL) {
        match cc
            .to_str()
            .map_err(|e| e.to_string())
            .and_then(cache_control::parse)
        {
            Ok(cc) => {
                if let Some(t) = cache_control::jwk_expiry(&cc, now) {
                    return Some(t);
                }
            }
            Err(e) => {
                tracing::warn!(target: "jwk-refresh-log", error = %e, "Failed parsing Cache-Control header from JWK response");
                return None;
            }
        }
    }
    let expires = headers.get(http::header::EXPIRES)?;
    match expires
        .to_str()
        .map_err(|e| e.to_string())
        .and_then(cache_control::parse_http_date)
    {
        Ok(t) => Some(t),
        Err(_) => {
            tracing::warn!(target: "jwk-refresh-log", "Failed parsing Expires header from JWK response. Value of header is not a valid timestamp");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::{Value, json};

    use super::*;
    use crate::jwt::JwtConfig;

    fn jwk(kid: &str) -> Value {
        json!({"kty": "oct", "kid": kid, "k": "c2VjcmV0LXNlY3JldC1zZWNyZXQtc2VjcmV0LXNlY3JldA"})
    }

    #[tokio::test]
    async fn refresh_on_unknown_kid_and_expiry() {
        let served = Arc::new(Mutex::new(json!({"keys": [jwk("k1")]})));
        let s = served.clone();
        let app = axum::Router::new().route(
            "/jwks",
            axum::routing::get(move || {
                let s = s.clone();
                async move {
                    (
                        [("cache-control", "max-age=3600")],
                        axum::Json(s.lock().unwrap().clone()),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/jwks", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let config = JwtConfig::parse(&json!({"jwk_url": url})).unwrap();
        let ctx = JwtContext::new(config, &reqwest::Client::new(), "ua")
            .await
            .unwrap();
        let jwk_source = ctx.jwk.as_ref().unwrap();
        assert!(ctx.keys.load().has_kid("k1"));
        let expires = jwk_source.expires_ms.load(Ordering::Relaxed);
        assert!(expires > now_ms() + 3_500_000, "expiry from max-age");

        *served.lock().unwrap() = json!({"keys": [jwk("k2")]});
        // Within the minimum interval nothing happens, even for an unknown kid.
        jwk_source.maybe_refresh(&ctx, true);
        assert!(!jwk_source.refreshing.load(Ordering::Acquire));
        // Pretend the last fetch was long ago.
        jwk_source.last_attempt_ms.store(0, Ordering::Relaxed);
        jwk_source.maybe_refresh(&ctx, false);
        assert!(
            !jwk_source.refreshing.load(Ordering::Acquire),
            "not expired yet"
        );
        jwk_source.maybe_refresh(&ctx, true);
        for _ in 0..100 {
            if ctx.keys.load().has_kid("k2") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ctx.keys.load().has_kid("k2"));

        // An expired set is refreshed too.
        *served.lock().unwrap() = json!({"keys": [jwk("k3")]});
        jwk_source.expires_ms.store(1, Ordering::Relaxed);
        jwk_source.last_attempt_ms.store(0, Ordering::Relaxed);
        for _ in 0..100 {
            jwk_source.maybe_refresh(&ctx, false);
            if ctx.keys.load().has_kid("k3") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ctx.keys.load().has_kid("k3"));
    }

    #[test]
    fn expiry_headers() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let mut h = http::HeaderMap::new();
        assert_eq!(expiry_from_headers(&h, now), None);
        h.insert("expires", "Wed, 21 Oct 2099 07:28:00 GMT".parse().unwrap());
        assert!(expiry_from_headers(&h, now).is_some());
        h.insert("cache-control", "max-age=10".parse().unwrap());
        assert_eq!(
            expiry_from_headers(&h, now),
            Some(now + Duration::from_secs(10))
        );
        h.insert("cache-control", "public".parse().unwrap());
        assert!(expiry_from_headers(&h, now).unwrap() > now + Duration::from_secs(10));
        h.insert("cache-control", "max-age=x".parse().unwrap());
        assert_eq!(expiry_from_headers(&h, now), None);
    }
}
