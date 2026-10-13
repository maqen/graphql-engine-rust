//! JWT mode (spec 05 §5): token location, issuer pre-match, verification, claims.

mod claims;
pub mod config;
mod jwks;
pub mod keys;

use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use hge_core::{Code, RoleName};
use http::HeaderMap;
use serde_json::{Map, Value};

use crate::{AuthError, Authenticated};
pub use config::JwtConfig;
use config::{KeySource, TokenLocation};
pub(crate) use jwks::JwkSource;
use keys::{Alg, KeySet};

/// A JWT config with its current keys.
pub struct JwtContext {
    pub config: JwtConfig,
    keys: ArcSwap<KeySet>,
    jwk: Option<JwkSource>,
    self_ref: Weak<JwtContext>,
}

impl JwtContext {
    /// Build the context; a `jwk_url` is fetched now (V2 fails startup if that fails).
    pub(crate) async fn new(
        config: JwtConfig,
        client: &reqwest::Client,
        user_agent: &str,
    ) -> Result<Arc<Self>, String> {
        let (keys, jwk) = match &config.keys {
            // Static keys stay in the config; see `with_keys`.
            KeySource::Static(_) => (KeySet::default(), None),
            KeySource::Url(url) => {
                let source = JwkSource::new(url.clone(), client.clone(), user_agent.to_owned());
                let keys = source.fetch_at_startup().await?;
                (keys, Some(source))
            }
        };
        let ctx = Arc::new_cyclic(|self_ref| JwtContext {
            config,
            keys: ArcSwap::from_pointee(keys),
            jwk,
            self_ref: self_ref.clone(),
        });
        if ctx.jwk.is_some() {
            jwks::spawn_refresh_ticker(Arc::downgrade(&ctx));
        }
        Ok(ctx)
    }

    /// Call `f` with each key that can check this token.
    fn with_keys<R>(&self, f: impl FnOnce(&[keys::Key]) -> R) -> R {
        match &self.config.keys {
            KeySource::Static(key) => f(std::slice::from_ref(key)),
            KeySource::Url(_) => f(&self.keys.load().0),
        }
    }
}

/// A decoded (not yet verified) compact JWS.
struct Token<'a> {
    signing_input: &'a str,
    alg: Option<Alg>,
    kid: Option<String>,
    claims: Map<String, Value>,
    signature: Vec<u8>,
}

impl<'a> Token<'a> {
    /// Errors are rendered like jose's `JWSError` (best effort; spec 05 §5.4).
    fn decode(token: &'a str) -> Result<Self, String> {
        let parts = token.split('.').count();
        if parts != 3 {
            return Err(format!(
                "JWSError (CompactDecodeError Invalid number of parts: Expected 3 parts; got {parts})"
            ));
        }
        let mut it = token.splitn(3, '.');
        let (h, p, s) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let decode_json = |part: &str| -> Result<Map<String, Value>, String> {
            let bytes = keys::b64_lenient(part)
                .map_err(|e| format!("JWSError (CompactDecodeError Invalid base64url: {e})"))?;
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(Value::Object(o)) => Ok(o),
                Ok(v) => Err(format!(
                    "JWSError (JSONDecodeError \"parsing object failed, expected Object, but encountered {}\")",
                    config::kind(&v)
                )),
                Err(e) => Err(format!("JWSError (JSONDecodeError \"{e}\")")),
            }
        };
        let header = decode_json(h)?;
        let claims = decode_json(p)?;
        let signature = keys::b64_lenient(s)
            .map_err(|e| format!("JWSError (CompactDecodeError Invalid base64url: {e})"))?;
        Ok(Token {
            signing_input: &token[..h.len() + 1 + p.len()],
            alg: header
                .get("alg")
                .and_then(Value::as_str)
                .and_then(Alg::parse),
            kid: header.get("kid").and_then(Value::as_str).map(str::to_owned),
            claims,
            signature,
        })
    }
}

fn invalid_jwt(reason: &str) -> AuthError {
    AuthError::bad_request(Code::InvalidJwt, format!("Could not verify JWT: {reason}"))
}

/// JWT mode for one request.
pub(crate) fn authenticate(
    contexts: &[Arc<JwtContext>],
    headers: &HeaderMap,
    unauthorized_role: Option<&RoleName>,
) -> Result<Authenticated, AuthError> {
    let mut found = false;
    let mut candidate: Option<(&JwtContext, Result<Token<'_>, String>)> = None;
    let mut multiple = false;
    for ctx in contexts {
        for raw in tokens_at(&ctx.config.header, headers)? {
            found = true;
            let token = Token::decode(raw);
            if !issuer_matches(&ctx.config, token.as_ref().ok()) {
                continue;
            }
            if candidate.is_some() {
                multiple = true;
            } else {
                candidate = Some((ctx, token));
            }
        }
    }
    if !found {
        return match unauthorized_role {
            Some(role) => Ok(Authenticated::new(crate::unauthorized(role, headers))),
            None => Err(AuthError::bad_request(
                Code::InvalidHeaders,
                "Missing 'Authorization' or 'Cookie' header in JWT authentication mode",
            )),
        };
    }
    if multiple {
        return Err(AuthError::bad_request(
            Code::InvalidHeaders,
            "Could not verify JWT: Multiple JWTs found",
        ));
    }
    let Some((ctx, token)) = candidate else {
        return Err(invalid_jwt("JWTNotInIssuer"));
    };
    let token = token.map_err(|e| invalid_jwt(&e))?;
    verify(ctx, &token)?;
    let expires_at = token
        .claims
        .get("exp")
        .and_then(Value::as_f64)
        .filter(|e| *e >= 0.0)
        .and_then(|e| UNIX_EPOCH.checked_add(Duration::from_secs_f64(e)));
    let session = claims::session(&ctx.config, &token.claims, headers)?;
    Ok(Authenticated {
        session,
        expires_at,
        set_cookies: vec![],
    })
}

/// The tokens at a config's location, in header order.
fn tokens_at<'h>(
    location: &TokenLocation,
    headers: &'h HeaderMap,
) -> Result<Vec<&'h str>, AuthError> {
    let mut out = vec![];
    match location {
        TokenLocation::Authorization => {
            for value in headers.get_all(http::header::AUTHORIZATION) {
                let mut words = value.to_str().unwrap_or_default().split_ascii_whitespace();
                match (words.next(), words.next(), words.next()) {
                    (Some("Bearer"), Some(token), None) => out.push(token),
                    _ => {
                        return Err(AuthError::bad_request(
                            Code::InvalidHeaders,
                            "Malformed Authorization header",
                        ));
                    }
                }
            }
        }
        TokenLocation::Cookie(name) => {
            for value in headers.get_all(http::header::COOKIE) {
                for cookie in value.to_str().unwrap_or_default().split(';') {
                    if let Some((n, v)) = cookie.trim_start().split_once('=')
                        && n == name
                    {
                        out.push(v);
                    }
                }
            }
        }
        TokenLocation::Header(name) => {
            for value in headers.get_all(name.as_str()) {
                if let Ok(v) = value.to_str() {
                    out.push(v);
                }
            }
        }
    }
    Ok(out)
}

/// V2's pre-verification issuer match used to pick a config.
fn issuer_matches(config: &JwtConfig, token: Option<&Token<'_>>) -> bool {
    let Some(want) = &config.issuer else {
        return true;
    };
    match token.and_then(|t| t.claims.get("iss")) {
        None => !config.require_issuer,
        Some(iss) => iss.as_str() == Some(want.as_str()),
    }
}

/// Signature, then claims (`exp`, `nbf`, `iss`, `aud`), as jose's `verifyClaimsAt`.
fn verify(ctx: &JwtContext, token: &Token<'_>) -> Result<(), AuthError> {
    if let Some(jwk) = &ctx.jwk {
        let kid_unknown = token
            .kid
            .as_deref()
            .is_some_and(|k| !ctx.keys.load().has_kid(k));
        jwk.maybe_refresh(ctx, kid_unknown);
    }
    let alg = token
        .alg
        .ok_or_else(|| invalid_jwt("JWSError NoUsableKeys"))?;
    let message = token.signing_input.as_bytes();
    let result = ctx.with_keys(|keys| {
        let mut usable = keys
            .iter()
            .filter(|k| k.usable_for(alg, token.kid.as_deref()))
            .peekable();
        if usable.peek().is_none() {
            return Err("JWSError NoUsableKeys");
        }
        if usable.any(|k| k.verify(alg, message, &token.signature)) {
            Ok(())
        } else {
            Err("JWSError JWSInvalidSignature")
        }
    });
    result.map_err(invalid_jwt)?;
    validate_claims(&ctx.config, &token.claims, now_secs()).map_err(|e| invalid_jwt(&e))
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

fn numeric_date(claims: &Map<String, Value>, name: &str) -> Result<Option<f64>, String> {
    match claims.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(v) => Err(format!(
            "JWTClaimsSetDecodeError \"Error in $.{name}: parsing NumericDate failed, expected Number, but encountered {}\"",
            config::kind(v)
        )),
    }
}

pub(crate) fn validate_claims(
    config: &JwtConfig,
    claims: &Map<String, Value>,
    now: f64,
) -> Result<(), String> {
    let skew = config.allowed_skew;
    if let Some(exp) = numeric_date(claims, "exp")?
        && now >= exp + skew
    {
        return Err("JWTExpired".into());
    }
    if let Some(nbf) = numeric_date(claims, "nbf")?
        && now < nbf - skew
    {
        return Err("JWTNotYetValid".into());
    }
    match claims.get("iss") {
        None | Some(Value::Null) if config.require_issuer => return Err("JWTNotInIssuer".into()),
        None | Some(Value::Null) => {}
        Some(iss) => {
            if let Some(want) = &config.issuer
                && iss.as_str() != Some(want.as_str())
            {
                return Err("JWTNotInIssuer".into());
            }
        }
    }
    let token_aud: Option<Vec<&str>> = match claims.get("aud") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(vec![s.as_str()]),
        Some(Value::Array(a)) => Some(a.iter().filter_map(Value::as_str).collect()),
        Some(_) => return Err("JWTNotInAudience".into()),
    };
    match (token_aud, &config.audience) {
        (None, _) if config.require_audience => return Err("JWTNotInAudience".into()),
        (Some(auds), Some(allowed)) if !auds.iter().any(|a| allowed.iter().any(|b| a == b)) => {
            return Err("JWTNotInAudience".into());
        }
        _ => {}
    }
    Ok(())
}
