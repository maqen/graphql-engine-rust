//! Request authentication: headers → [`Session`].
//!
//! Implements V2's admin-secret modes. JWT and auth-webhook modes belong to the *Auth*
//! workstream and slot in as further [`AuthMode`] variants.

use hge_core::{Code, Error, RoleName, Session, SessionVariables};
use http::HeaderMap;
use subtle::ConstantTimeEq;

pub const ADMIN_SECRET_HEADER: &str = "x-hasura-admin-secret";
pub const ROLE_HEADER: &str = "x-hasura-role";

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// `HASURA_GRAPHQL_ADMIN_SECRET` / `HASURA_GRAPHQL_ADMIN_SECRETS`. Empty = no auth.
    pub admin_secrets: Vec<String>,
    /// `HASURA_GRAPHQL_UNAUTHORIZED_ROLE`
    pub unauthorized_role: Option<String>,
}

/// Resolve the session for a request.
pub fn authenticate(config: &AuthConfig, headers: &HeaderMap) -> Result<Session, Error> {
    let variables = session_variables(headers);
    if config.admin_secrets.is_empty() {
        return Ok(fallback_admin(variables));
    }
    match headers.get(ADMIN_SECRET_HEADER) {
        Some(given) => {
            let given = given.as_bytes();
            let ok = config
                .admin_secrets
                .iter()
                .any(|s| bool::from(s.as_bytes().ct_eq(given)));
            if ok {
                Ok(fallback_admin(variables))
            } else {
                Err(access_denied(format!("invalid \"{ADMIN_SECRET_HEADER}\"")))
            }
        }
        None => match &config.unauthorized_role {
            Some(role) => Ok(Session {
                role: RoleName::new(role),
                variables,
            }),
            None => Err(access_denied(format!(
                "\"{ADMIN_SECRET_HEADER}\" required, but not found"
            ))),
        },
    }
}

fn access_denied(message: String) -> Error {
    Error::new(Code::AccessDenied, message)
}

/// Role from `x-hasura-role`, defaulting to `admin`.
fn fallback_admin(variables: SessionVariables) -> Session {
    let role = variables
        .get(ROLE_HEADER)
        .map(RoleName::new)
        .unwrap_or_else(RoleName::admin);
    Session { role, variables }
}

/// All `x-hasura-*` headers except the admin secret.
pub fn session_variables(headers: &HeaderMap) -> SessionVariables {
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

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn no_secret_means_admin_with_role_switch() {
        let cfg = AuthConfig::default();
        assert!(authenticate(&cfg, &headers(&[])).unwrap().is_admin());
        let s = authenticate(&cfg, &headers(&[("x-hasura-role", "user")])).unwrap();
        assert_eq!(s.role.as_str(), "user");
    }

    #[test]
    fn admin_secret_checks() {
        let cfg = AuthConfig {
            admin_secrets: vec!["s3cret".into()],
            unauthorized_role: None,
        };
        let err = authenticate(&cfg, &headers(&[])).unwrap_err();
        assert_eq!(err.code, Code::AccessDenied);
        assert_eq!(
            err.message,
            "\"x-hasura-admin-secret\" required, but not found"
        );
        let err = authenticate(&cfg, &headers(&[("x-hasura-admin-secret", "nope")])).unwrap_err();
        assert_eq!(err.message, "invalid \"x-hasura-admin-secret\"");
        let s = authenticate(
            &cfg,
            &headers(&[
                ("x-hasura-admin-secret", "s3cret"),
                ("x-hasura-user-id", "7"),
            ]),
        )
        .unwrap();
        assert!(s.is_admin());
        assert_eq!(s.variables.get("x-hasura-user-id"), Some("7"));
        assert_eq!(s.variables.get(ADMIN_SECRET_HEADER), None);
    }

    #[test]
    fn unauthorized_role() {
        let cfg = AuthConfig {
            admin_secrets: vec!["s".into()],
            unauthorized_role: Some("anonymous".into()),
        };
        let s = authenticate(&cfg, &headers(&[("x-hasura-role", "admin")])).unwrap();
        assert_eq!(s.role.as_str(), "anonymous");
    }
}
