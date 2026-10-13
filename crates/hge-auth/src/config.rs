//! Auth configuration and V2's startup mode table (spec 05 §3).

use std::sync::Arc;
use std::time::Duration;

use hge_core::RoleName;
use serde_json::Value;

use crate::admin::AdminSecrets;
use crate::jwt::{JwtConfig, JwtContext};
use crate::webhook::Webhook;
use crate::{Authenticator, Mode};

/// What the server was started with (env vars / flags), before validation.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// `HASURA_GRAPHQL_ADMIN_SECRET` (or `HASURA_GRAPHQL_ACCESS_KEY`) plus
    /// `HASURA_GRAPHQL_ADMIN_SECRETS`.
    pub admin_secrets: Vec<String>,
    /// `HASURA_GRAPHQL_UNAUTHORIZED_ROLE`
    pub unauthorized_role: Option<String>,
    /// `HASURA_GRAPHQL_DISABLE_ADMIN_SECRET`
    pub disable_admin_secret: bool,
    /// `HASURA_GRAPHQL_JWT_SECRET`: one JSON object.
    pub jwt_secret: Option<String>,
    /// `HASURA_GRAPHQL_JWT_SECRETS`: a JSON list of objects.
    pub jwt_secrets: Option<String>,
    /// `HASURA_GRAPHQL_AUTH_HOOK` and friends.
    pub auth_hook: Option<HookConfig>,
    /// `User-Agent` for webhook and JWK requests (`hasura-graphql-engine/<version>`).
    pub user_agent: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookMode {
    #[default]
    Get,
    Post,
}

impl std::str::FromStr for HookMode {
    type Err = String;

    /// Exactly `GET` or `POST`, as V2.
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "GET" => Ok(HookMode::Get),
            "POST" => Ok(HookMode::Post),
            _ => Err("Only expecting GET / POST".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookConfig {
    /// `HASURA_GRAPHQL_AUTH_HOOK`
    pub url: String,
    /// `HASURA_GRAPHQL_AUTH_HOOK_MODE`
    pub mode: HookMode,
    /// `HASURA_GRAPHQL_AUTH_HOOK_SEND_REQUEST_BODY` (default true; always false for GET).
    pub send_request_body: bool,
    /// `HASURA_GRAPHQL_AUTH_HOOK_CACHE` (our extension, default false): reuse answers that
    /// carry `Cache-Control: max-age` / `Expires` for identical requests until they expire.
    pub cache: bool,
}

impl HookConfig {
    pub fn new(url: impl Into<String>, mode: HookMode) -> Self {
        HookConfig {
            url: url.into(),
            mode,
            send_request_body: true,
            cache: false,
        }
    }
}

const DEFAULT_USER_AGENT: &str = "hasura-graphql-engine/v2.51.0";

/// Timeout for webhook and JWK requests (V2 uses the HTTP manager default, 30 s).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Parse `HASURA_GRAPHQL_JWT_SECRET(S)` into configs, without duplicates.
fn jwt_values(config: &AuthConfig) -> Result<Vec<Value>, String> {
    let mut values: Vec<Value> = vec![];
    if let Some(text) = &config.jwt_secret {
        let v = serde_json::from_str(text)
            .map_err(|e| format!("Fatal Error: HASURA_GRAPHQL_JWT_SECRET: {e}"))?;
        values.push(v);
    }
    if let Some(text) = &config.jwt_secrets {
        let list: Vec<Value> = serde_json::from_str(text)
            .map_err(|e| format!("Fatal Error: HASURA_GRAPHQL_JWT_SECRETS: {e}"))?;
        values.extend(list);
    }
    let mut unique: Vec<Value> = vec![];
    for v in values {
        if !unique.contains(&v) {
            unique.push(v);
        }
    }
    Ok(unique)
}

pub(crate) async fn build(config: AuthConfig) -> Result<Authenticator, String> {
    let jwt_values = jwt_values(&config)?;
    let has_secret = !config.admin_secrets.is_empty();
    let has_hook = config.auth_hook.is_some();
    let has_jwt = !jwt_values.is_empty();
    let disable = config.disable_admin_secret;
    let unauthorized_role = config
        .unauthorized_role
        .as_deref()
        .filter(|r| !r.is_empty())
        .map(RoleName::new);

    let mode = match (has_secret, has_hook, has_jwt, disable) {
        (_, true, true, _) => {
            return Err(
                "Fatal Error: Both webhook and JWT mode cannot be enabled at the same time".into(),
            );
        }
        (_, false, false, true) => Mode::Unauthenticated,
        (false, false, false, false) => {
            if unauthorized_role.is_some() {
                return Err("Fatal Error: --unauthorized-role (HASURA_GRAPHQL_UNAUTHORIZED_ROLE) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set".into());
            }
            Mode::NoAuth
        }
        (true, false, false, false) => Mode::AdminSecret { unauthorized_role },
        (_, false, true, true) => Mode::Jwt {
            check_secret: false,
            unauthorized_role,
        },
        (true, false, true, false) => Mode::Jwt {
            check_secret: true,
            unauthorized_role,
        },
        (false, false, true, false) => {
            return Err("Fatal Error : --jwt-secret (HASURA_GRAPHQL_JWT_SECRET) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set".into());
        }
        (_, true, false, true) => Mode::Hook {
            check_secret: false,
        },
        (true, true, false, false) => {
            if unauthorized_role.is_some() {
                return Err("Fatal Error: --unauthorized-role (HASURA_GRAPHQL_UNAUTHORIZED_ROLE) is not allowed when --auth-hook (HASURA_GRAPHQL_AUTH_HOOK) is set".into());
            }
            Mode::Hook { check_secret: true }
        }
        (false, true, false, false) => {
            return Err("Fatal Error : --auth-hook (HASURA_GRAPHQL_AUTH_HOOK) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set".into());
        }
    };

    let user_agent = if config.user_agent.is_empty() {
        DEFAULT_USER_AGENT
    } else {
        &config.user_agent
    };
    let needs_client = has_hook || has_jwt;
    let client = if needs_client {
        reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| format!("Fatal Error: could not create an HTTP client: {e}"))?
    } else {
        reqwest::Client::new()
    };

    let mut jwt = Vec::with_capacity(jwt_values.len());
    if matches!(mode, Mode::Jwt { .. }) {
        for v in &jwt_values {
            let parsed = JwtConfig::parse(v)
                .map_err(|e| format!("Fatal Error: HASURA_GRAPHQL_JWT_SECRET: {e}"))?;
            let ctx: Arc<JwtContext> = JwtContext::new(parsed, &client, user_agent)
                .await
                .map_err(|e| format!("Fatal Error: {e}"))?;
            jwt.push(ctx);
        }
    }

    let hook = match (&mode, config.auth_hook) {
        (Mode::Hook { .. }, Some(mut hook)) => {
            if hook.mode == HookMode::Get {
                hook.send_request_body = false;
            }
            Some(Webhook::new(hook, client, user_agent))
        }
        _ => None,
    };

    Ok(Authenticator {
        mode,
        secrets: AdminSecrets::new(&config.admin_secrets),
        jwt,
        hook,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn err(config: AuthConfig) -> String {
        build(config).await.unwrap_err()
    }

    fn jwt() -> Option<String> {
        Some(r#"{"type":"HS256","key":"an-hmac-secret-that-is-32-bytes!"}"#.into())
    }

    fn hook() -> Option<HookConfig> {
        Some(HookConfig::new("http://localhost:1/hook", HookMode::Get))
    }

    #[tokio::test]
    async fn mode_table() {
        assert_eq!(
            err(AuthConfig {
                jwt_secret: jwt(),
                auth_hook: hook(),
                admin_secrets: vec!["s".into()],
                ..Default::default()
            })
            .await,
            "Fatal Error: Both webhook and JWT mode cannot be enabled at the same time"
        );
        assert_eq!(
            err(AuthConfig {
                unauthorized_role: Some("anon".into()),
                ..Default::default()
            })
            .await,
            "Fatal Error: --unauthorized-role (HASURA_GRAPHQL_UNAUTHORIZED_ROLE) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set"
        );
        assert_eq!(
            err(AuthConfig {
                jwt_secret: jwt(),
                ..Default::default()
            })
            .await,
            "Fatal Error : --jwt-secret (HASURA_GRAPHQL_JWT_SECRET) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set"
        );
        assert_eq!(
            err(AuthConfig {
                auth_hook: hook(),
                ..Default::default()
            })
            .await,
            "Fatal Error : --auth-hook (HASURA_GRAPHQL_AUTH_HOOK) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set"
        );
        assert_eq!(
            err(AuthConfig {
                auth_hook: hook(),
                admin_secrets: vec!["s".into()],
                unauthorized_role: Some("anon".into()),
                ..Default::default()
            })
            .await,
            "Fatal Error: --unauthorized-role (HASURA_GRAPHQL_UNAUTHORIZED_ROLE) is not allowed when --auth-hook (HASURA_GRAPHQL_AUTH_HOOK) is set"
        );
        let a = build(AuthConfig {
            jwt_secret: jwt(),
            disable_admin_secret: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(matches!(
            a.mode,
            Mode::Jwt {
                check_secret: false,
                ..
            }
        ));
        let a = build(AuthConfig {
            auth_hook: hook(),
            disable_admin_secret: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(matches!(
            a.mode,
            Mode::Hook {
                check_secret: false
            }
        ));
        let a = build(AuthConfig {
            jwt_secret: jwt(),
            jwt_secrets: Some(format!("[{}]", jwt().unwrap())),
            admin_secrets: vec!["s".into()],
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(a.jwt.len(), 1, "duplicate configs are removed");
        assert!(a.is_jwt_set() && a.is_admin_secret_set() && !a.is_auth_hook_set());
        assert_eq!(a.jwt_configs_json()[0]["key"], "<JWK REDACTED>");
        assert!(
            err(AuthConfig {
                jwt_secret: Some("{".into()),
                admin_secrets: vec!["s".into()],
                ..Default::default()
            })
            .await
            .starts_with("Fatal Error: HASURA_GRAPHQL_JWT_SECRET: ")
        );
        assert_eq!(
            "PUT".parse::<HookMode>().unwrap_err(),
            "Only expecting GET / POST"
        );
        assert!("get".parse::<HookMode>().is_err());
    }
}
