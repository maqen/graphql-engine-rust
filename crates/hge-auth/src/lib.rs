//! Request authentication: headers → [`Session`] (spec 05).
//!
//! [`Authenticator::new`] validates the configuration with V2's startup rules (the mode
//! table in spec 05 §3) and [`Authenticator::authenticate`] resolves each request.
//! Modes: no auth, admin secret(s), JWT (static keys or a refreshed JWK set) and the auth
//! webhook, each optionally combined with the admin secret and the unauthorized role.

mod admin;
mod cache_control;
mod config;
mod der;
mod http_exception;
pub mod jsonpath;
pub mod jwt;
mod webhook;

use std::sync::Arc;
use std::time::SystemTime;

use hge_core::{Code, Error, RoleName, Session, SessionVariables};
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::Value;

use admin::AdminSecrets;
pub use config::{AuthConfig, HookConfig, HookMode};
use jwt::JwtContext;
use webhook::Webhook;

pub const ADMIN_SECRET_HEADER: &str = "x-hasura-admin-secret";
pub const ROLE_HEADER: &str = "x-hasura-role";
pub const BACKEND_ONLY_HEADER: &str = "x-hasura-use-backend-only-permissions";

/// A failed authentication: the V2 error plus the HTTP status V2 attaches to it.
///
/// GraphQL endpoints report most of these with status 200 (spec 05 §9.2); other endpoints
/// and `/v1alpha1/graphql` use `status`.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthError {
    pub status: StatusCode,
    pub error: Error,
}

impl AuthError {
    pub(crate) fn new(status: StatusCode, code: Code, message: impl Into<String>) -> Self {
        AuthError {
            status,
            error: Error::new(code, message),
        }
    }

    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, Code::AccessDenied, message)
    }

    pub(crate) fn bad_request(code: Code, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, Code::Unexpected, message)
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for AuthError {}

/// A successfully authenticated request.
#[derive(Debug, Clone, PartialEq)]
pub struct Authenticated {
    pub session: Session,
    /// When the credentials stop being valid (JWT `exp`, or the webhook's `Cache-Control` /
    /// `Expires` keys). WebSocket connections are closed at this time.
    pub expires_at: Option<SystemTime>,
    /// `Set-Cookie` headers returned by the auth webhook, to be added to the response.
    pub set_cookies: Vec<HeaderValue>,
}

impl Authenticated {
    fn new(session: Session) -> Self {
        Authenticated {
            session,
            expires_at: None,
            set_cookies: vec![],
        }
    }
}

/// The resolved auth mode (V2's `AuthMode`).
#[derive(Debug)]
enum Mode {
    /// No admin secret: everyone is admin unless they pick a role.
    NoAuth,
    /// `HASURA_GRAPHQL_DISABLE_ADMIN_SECRET` without JWT or webhook: every request fails.
    Unauthenticated,
    AdminSecret {
        unauthorized_role: Option<RoleName>,
    },
    Jwt {
        check_secret: bool,
        unauthorized_role: Option<RoleName>,
    },
    Hook {
        check_secret: bool,
    },
}

/// Authenticates requests. Cheap to share; build one per server.
pub struct Authenticator {
    mode: Mode,
    secrets: AdminSecrets,
    jwt: Vec<Arc<JwtContext>>,
    hook: Option<Webhook>,
}

impl std::fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticator")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl Authenticator {
    /// Validate the configuration and set up the mode. Errors are V2's fatal startup
    /// messages. JWK URLs are fetched here (and refreshed in the background afterwards), so
    /// this must run inside a Tokio runtime.
    pub async fn new(config: AuthConfig) -> Result<Self, String> {
        config::build(config).await
    }

    /// An authenticator that lets everyone in as admin (V2 without an admin secret).
    pub fn no_auth() -> Self {
        Authenticator {
            mode: Mode::NoAuth,
            secrets: AdminSecrets::default(),
            jwt: vec![],
            hook: None,
        }
    }

    /// `true` when [`authenticate`](Self::authenticate) wants the GraphQL request
    /// (webhook POST mode with `send_request_body`). Callers can skip building it otherwise.
    pub fn needs_request_body(&self) -> bool {
        self.hook.as_ref().is_some_and(Webhook::sends_request_body)
    }

    /// `/v1alpha1/config`: every mode but no-auth counts as "admin secret set".
    pub fn is_admin_secret_set(&self) -> bool {
        !matches!(self.mode, Mode::NoAuth)
    }

    pub fn is_jwt_set(&self) -> bool {
        matches!(self.mode, Mode::Jwt { .. })
    }

    pub fn is_auth_hook_set(&self) -> bool {
        matches!(self.mode, Mode::Hook { .. })
    }

    /// The redacted JWT configs, as `/v1alpha1/config` reports them.
    pub fn jwt_configs_json(&self) -> Vec<Value> {
        self.jwt.iter().map(|c| c.config.redacted_json()).collect()
    }

    /// Resolve the session for a request.
    ///
    /// `request` is the parsed GraphQL request (or batch), forwarded to a POST webhook with
    /// `send_request_body`; pass `None` for non-GraphQL endpoints or invalid JSON.
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        request: Option<&Value>,
    ) -> Result<Authenticated, AuthError> {
        match &self.mode {
            Mode::NoAuth => {
                fallback_admin(header_session_vars(headers), true).map(Authenticated::new)
            }
            Mode::Unauthenticated => Err(AuthError::unauthorized("unauthenticated")),
            Mode::AdminSecret { unauthorized_role } => {
                if let Some(done) = self.check_secret_if_sent(headers) {
                    return done;
                }
                match unauthorized_role {
                    Some(role) => Ok(Authenticated::new(unauthorized(role, headers))),
                    None => Err(AuthError::unauthorized(format!(
                        "\"{ADMIN_SECRET_HEADER}\" required, but not found"
                    ))),
                }
            }
            Mode::Jwt {
                check_secret,
                unauthorized_role,
            } => {
                if *check_secret && let Some(done) = self.check_secret_if_sent(headers) {
                    return done;
                }
                jwt::authenticate(&self.jwt, headers, unauthorized_role.as_ref())
            }
            Mode::Hook { check_secret } => {
                if *check_secret && let Some(done) = self.check_secret_if_sent(headers) {
                    return done;
                }
                let hook = self.hook.as_ref().expect("hook mode has a webhook");
                hook.authenticate(headers, request).await
            }
        }
    }

    /// V2's `checkingSecretIfSent`: `None` when no admin secret header was sent.
    fn check_secret_if_sent(
        &self,
        headers: &HeaderMap,
    ) -> Option<Result<Authenticated, AuthError>> {
        // Last header wins, as in V2's session-variable map.
        let given = headers.get_all(ADMIN_SECRET_HEADER).iter().next_back()?;
        Some(if self.secrets.check(given.as_bytes()) {
            fallback_admin(header_session_vars(headers), true).map(Authenticated::new)
        } else {
            Err(AuthError::unauthorized(format!(
                "invalid \"{ADMIN_SECRET_HEADER}\""
            )))
        })
    }
}

/// The session for an unauthenticated request: the client's `x-hasura-*` headers are kept
/// as session variables, but the role is forced.
fn unauthorized(role: &RoleName, headers: &HeaderMap) -> Session {
    make_session(role.clone(), header_session_vars(headers), false)
}

/// Role from `x-hasura-role` (when non-empty), defaulting to `admin`. `backend_only_allowed`
/// is V2's "admin secret sent or no auth" condition for honouring the backend-only header.
fn fallback_admin(
    variables: SessionVariables,
    backend_only_allowed: bool,
) -> Result<Session, AuthError> {
    let role = variables
        .get(ROLE_HEADER)
        .filter(|r| !r.is_empty())
        .map(RoleName::new)
        .unwrap_or_else(RoleName::admin);
    let backend_only = if backend_only_allowed {
        backend_only_flag(&variables)?
    } else {
        false
    };
    Ok(make_session(role, variables, backend_only))
}

fn backend_only_flag(variables: &SessionVariables) -> Result<bool, AuthError> {
    let Some(value) = variables.get(BACKEND_ONLY_HEADER) else {
        return Ok(false);
    };
    match value.to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" => Ok(true),
        "false" | "f" | "no" | "n" => Ok(false),
        _ => Err(AuthError::bad_request(
            Code::BadRequest,
            format!(
                "\"{BACKEND_ONLY_HEADER}\":  Not a valid boolean text. True values are \
                 [\"true\",\"t\",\"yes\",\"y\"] and  False values are \
                 [\"false\",\"f\",\"no\",\"n\"]. All values are case insensitive"
            ),
        )),
    }
}

/// A session whose `x-hasura-role` variable is the resolved role, whatever was sent.
fn make_session(role: RoleName, mut variables: SessionVariables, backend_only: bool) -> Session {
    variables.insert(ROLE_HEADER, role.as_str());
    Session {
        role,
        variables,
        backend_only,
    }
}

/// All `x-hasura-*` headers except the admin secret. For repeated headers the last wins.
pub fn header_session_vars(headers: &HeaderMap) -> SessionVariables {
    let mut vars = SessionVariables::new();
    for (name, value) in headers {
        let name = name.as_str();
        if hge_core::session::is_session_variable(name)
            && name != ADMIN_SECRET_HEADER
            && let Ok(v) = value.to_str()
        {
            vars.insert(name, v);
        }
    }
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    async fn auth(config: AuthConfig) -> Authenticator {
        Authenticator::new(config).await.unwrap()
    }

    fn secret(s: &str) -> AuthConfig {
        AuthConfig {
            admin_secrets: vec![s.into()],
            ..AuthConfig::default()
        }
    }

    async fn run(a: &Authenticator, pairs: &[(&str, &str)]) -> Result<Session, AuthError> {
        a.authenticate(&headers(pairs), None)
            .await
            .map(|a| a.session)
    }

    #[tokio::test]
    async fn no_secret_means_admin_with_role_switch() {
        let a = auth(AuthConfig::default()).await;
        assert!(run(&a, &[]).await.unwrap().is_admin());
        let s = run(&a, &[("x-hasura-role", "user")]).await.unwrap();
        assert_eq!(s.role.as_str(), "user");
        // An empty role counts as absent.
        assert!(run(&a, &[("x-hasura-role", "")]).await.unwrap().is_admin());
        // The last of repeated headers wins.
        let s = run(&a, &[("x-hasura-role", "a"), ("x-hasura-role", "b")])
            .await
            .unwrap();
        assert_eq!(s.role.as_str(), "b");
    }

    #[tokio::test]
    async fn admin_secret_checks() {
        let a = auth(secret("s3cret")).await;
        let err = run(&a, &[]).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert_eq!(err.error.code, Code::AccessDenied);
        assert_eq!(
            err.error.message,
            "\"x-hasura-admin-secret\" required, but not found"
        );
        let err = run(&a, &[("x-hasura-admin-secret", "nope")])
            .await
            .unwrap_err();
        assert_eq!(err.error.message, "invalid \"x-hasura-admin-secret\"");
        let err = run(&a, &[("x-hasura-admin-secret", "")]).await.unwrap_err();
        assert_eq!(err.error.message, "invalid \"x-hasura-admin-secret\"");
        // The legacy access key header is not a secret.
        let err = run(&a, &[("x-hasura-access-key", "s3cret")])
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        let s = run(
            &a,
            &[
                ("X-Hasura-Admin-Secret", "s3cret"),
                ("x-hasura-user-id", "7"),
            ],
        )
        .await
        .unwrap();
        assert!(s.is_admin());
        assert_eq!(s.variables.get("x-hasura-user-id"), Some("7"));
        assert_eq!(s.variables.get("x-hasura-role"), Some("admin"));
        assert_eq!(s.variables.get(ADMIN_SECRET_HEADER), None);
    }

    #[tokio::test]
    async fn impersonation_and_backend_only() {
        let a = auth(secret("s")).await;
        let s = run(
            &a,
            &[
                ("x-hasura-admin-secret", "s"),
                ("x-hasura-role", "user"),
                ("x-hasura-use-backend-only-permissions", "YES"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(s.role.as_str(), "user");
        assert!(s.backend_only);
        let err = run(
            &a,
            &[
                ("x-hasura-admin-secret", "s"),
                ("x-hasura-use-backend-only-permissions", "maybe"),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error.code, Code::BadRequest);
        assert!(
            err.error.message.starts_with(
                "\"x-hasura-use-backend-only-permissions\":  Not a valid boolean text."
            )
        );
    }

    #[tokio::test]
    async fn multiple_admin_secrets() {
        let a = auth(AuthConfig {
            admin_secrets: vec!["one".into(), "two".into()],
            ..AuthConfig::default()
        })
        .await;
        assert!(run(&a, &[("x-hasura-admin-secret", "two")]).await.is_ok());
        assert!(run(&a, &[("x-hasura-admin-secret", "one")]).await.is_ok());
        assert!(
            run(&a, &[("x-hasura-admin-secret", "three")])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unauthorized_role() {
        let a = auth(AuthConfig {
            admin_secrets: vec!["s".into()],
            unauthorized_role: Some("anonymous".into()),
            ..AuthConfig::default()
        })
        .await;
        let s = run(
            &a,
            &[
                ("x-hasura-role", "admin"),
                ("x-hasura-user-id", "1"),
                ("x-hasura-use-backend-only-permissions", "true"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(s.role.as_str(), "anonymous");
        assert_eq!(s.variables.get("x-hasura-role"), Some("anonymous"));
        assert_eq!(s.variables.get("x-hasura-user-id"), Some("1"));
        assert!(!s.backend_only);
    }

    #[tokio::test]
    async fn disabled_admin_secret() {
        let a = auth(AuthConfig {
            admin_secrets: vec!["s".into()],
            disable_admin_secret: true,
            ..AuthConfig::default()
        })
        .await;
        for h in [&[][..], &[("x-hasura-admin-secret", "s")][..]] {
            let err = run(&a, h).await.unwrap_err();
            assert_eq!(err.status, StatusCode::UNAUTHORIZED);
            assert_eq!(err.error.message, "unauthenticated");
        }
    }
}
