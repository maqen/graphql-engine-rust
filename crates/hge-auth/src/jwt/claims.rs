//! Hasura claims → role and session variables (spec 05 §5.5, §5.6).

use hge_core::{Code, RoleName, Session, SessionVariables};
use http::HeaderMap;
use serde_json::{Map, Value};

use super::config::{ClaimValue, Claims, ClaimsFormat, ClaimsMap, JwtConfig, Namespace, kind};
use crate::jsonpath::{Element, JsonPath};
use crate::{AuthError, ROLE_HEADER};

const ALLOWED_ROLES: &str = "x-hasura-allowed-roles";
const DEFAULT_ROLE: &str = "x-hasura-default-role";
const REGISTERED: [&str; 7] = ["iss", "sub", "aud", "exp", "nbf", "iat", "jti"];

fn invalid_claims(message: impl Into<String>) -> AuthError {
    AuthError::bad_request(Code::JwtInvalidClaims, message)
}

fn missing_role_claim(name: &str) -> AuthError {
    AuthError::bad_request(
        Code::JwtMissingRoleClaims,
        format!("JWT claim does not contain \"{name}\""),
    )
}

/// aeson's message for a value of the wrong type.
fn type_error(expected: &str, v: &Value) -> String {
    let want = if expected == "[]" { "Array" } else { "String" };
    format!(
        "parsing {expected} failed, expected {want}, but encountered {}",
        kind(v)
    )
}

/// The role, session variables and backend-only flag from verified claims.
pub(super) fn session(
    config: &JwtConfig,
    claims: &Map<String, Value>,
    headers: &HeaderMap,
) -> Result<Session, AuthError> {
    let (allowed, default, vars) = match &config.claims {
        Claims::Namespace { namespace, format } => from_namespace(claims, namespace, *format)?,
        Claims::Map(map) => from_claims_map(claims, map)?,
    };
    // The first `x-hasura-role` header, when non-empty, else the default role.
    let requested = headers
        .get(ROLE_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|r| !r.is_empty());
    let role = requested.unwrap_or(default.as_str());
    if !allowed.iter().any(|r| r == role) {
        return Err(AuthError::bad_request(
            Code::AccessDenied,
            "Your requested role is not in allowed roles",
        ));
    }
    Ok(crate::make_session(RoleName::new(role), vars, false))
}

type Resolved = (Vec<String>, String, SessionVariables);

fn from_namespace(
    claims: &Map<String, Value>,
    namespace: &Namespace,
    format: ClaimsFormat,
) -> Result<Resolved, AuthError> {
    let (value, location) = match namespace {
        Namespace::Key(ns) => (
            claims
                .get(ns)
                .filter(|_| !REGISTERED.contains(&ns.as_str())),
            format!("claims_namespace {ns}"),
        ),
        Namespace::Path(path) => (
            unregistered_at(claims, path),
            format!("claims_namespace_path {path}"),
        ),
    };
    let not_found = || match namespace {
        Namespace::Key(ns) => invalid_claims(format!("claims key: '{ns}' not found")),
        Namespace::Path(path) => invalid_claims(format!(
            "claims not found at claims_namespace_path: '{path}'"
        )),
    };
    let root_object;
    let value = match (value, namespace) {
        (Some(v), _) => v,
        // `$`: the claims are the token's own (unregistered) claims.
        (None, Namespace::Path(p)) if p.elements.is_empty() => {
            root_object = Value::Object(
                claims
                    .iter()
                    .filter(|(k, _)| !REGISTERED.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            );
            &root_object
        }
        (None, _) => return Err(not_found()),
    };
    let parsed;
    let object = match format {
        ClaimsFormat::Json => value
            .as_object()
            .ok_or_else(|| invalid_claims("expecting a json object when claims_format is json"))?,
        ClaimsFormat::StringifiedJson => {
            let s = value.as_str().ok_or_else(|| {
                invalid_claims("expecting a string when claims_format is stringified_json")
            })?;
            parsed = match serde_json::from_str::<Value>(s) {
                Ok(Value::Object(o)) => o,
                _ => {
                    return Err(invalid_claims(format!(
                        "expecting stringified json at: '{location}', but found: {s}"
                    )));
                }
            };
            &parsed
        }
    };

    let mut allowed = None;
    let mut default = None;
    let mut vars = SessionVariables::new();
    for (name, value) in object {
        if !hge_core::session::is_session_variable(name) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            ALLOWED_ROLES => allowed = Some(value),
            DEFAULT_ROLE => default = Some(value),
            _ => match value {
                Value::String(s) => vars.insert(&lower, s.as_str()),
                v => {
                    return Err(invalid_claims(format!(
                        "x-hasura-* claims: {}",
                        type_error("Text", v)
                    )));
                }
            },
        }
    }
    let allowed = role_list(allowed.ok_or_else(|| missing_role_claim(ALLOWED_ROLES))?)?;
    let default = role(
        default.ok_or_else(|| missing_role_claim(DEFAULT_ROLE))?,
        "should be a single role name",
    )?;
    Ok((allowed, default, vars))
}

fn from_claims_map(claims: &Map<String, Value>, map: &ClaimsMap) -> Result<Resolved, AuthError> {
    let allowed =
        resolve(claims, &map.allowed_roles).ok_or_else(|| missing_role_claim(ALLOWED_ROLES))?;
    let allowed = role_list(allowed)?;
    let default =
        resolve(claims, &map.default_role).ok_or_else(|| missing_role_claim(DEFAULT_ROLE))?;
    let default = role(default, "should be a role")?;
    let mut vars = SessionVariables::new();
    for (name, value) in &map.others {
        let v = resolve(claims, value).ok_or_else(|| {
            invalid_claims(format!("JWT claim from claims_map, \"{name}\" not found"))
        })?;
        match v {
            Value::String(s) => vars.insert(name, s.as_str()),
            v => {
                return Err(invalid_claims(format!(
                    "x-hasura-* claims: {}",
                    type_error("Text", v)
                )));
            }
        }
    }
    Ok((allowed, default, vars))
}

/// A `claims_map` value against the full claim set.
fn resolve<'a>(claims: &'a Map<String, Value>, value: &'a ClaimValue) -> Option<&'a Value> {
    match value {
        ClaimValue::Literal(v) => Some(v),
        ClaimValue::Path { path, default } => lookup(claims, path).or(default.as_ref()),
    }
}

fn lookup<'a>(claims: &'a Map<String, Value>, path: &JsonPath) -> Option<&'a Value> {
    let (first, rest) = path.elements.split_first()?;
    let Element::Key(k) = first else { return None };
    let mut value = claims.get(k)?;
    for e in rest {
        value = match (e, value) {
            (Element::Key(k), Value::Object(o)) => o.get(k)?,
            (Element::Index(i), Value::Array(a)) => a.get(*i)?,
            _ => return None,
        };
    }
    Some(value)
}

/// `claims_namespace_path` is evaluated against the unregistered claims only.
fn unregistered_at<'a>(claims: &'a Map<String, Value>, path: &JsonPath) -> Option<&'a Value> {
    match path.elements.first() {
        Some(Element::Key(k)) if REGISTERED.contains(&k.as_str()) => None,
        _ => lookup(claims, path),
    }
}

fn role_list(v: &Value) -> Result<Vec<String>, AuthError> {
    let invalid = |detail: String| {
        invalid_claims(format!(
            "invalid \"{ALLOWED_ROLES}\"; should be a list of roles: {detail}"
        ))
    };
    let list = v.as_array().ok_or_else(|| invalid(type_error("[]", v)))?;
    list.iter()
        .map(|r| match r {
            Value::String(s) if !s.is_empty() => Ok(s.clone()),
            Value::String(_) => Err(invalid("empty string not allowed".into())),
            r => Err(invalid(type_error("Text", r))),
        })
        .collect()
}

fn role(v: &Value, should_be: &str) -> Result<String, AuthError> {
    let invalid = |detail: String| {
        invalid_claims(format!("invalid \"{DEFAULT_ROLE}\"; {should_be}: {detail}"))
    };
    match v {
        Value::String(s) if !s.is_empty() => Ok(s.clone()),
        Value::String(_) => Err(invalid("empty string not allowed".into())),
        v => Err(invalid(type_error("Text", v))),
    }
}
