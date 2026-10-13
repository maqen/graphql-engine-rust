//! `HASURA_GRAPHQL_JWT_SECRET` parsing, with V2's error messages (spec 05 §5.1).

use serde_json::{Map, Value, json};

use super::keys::{self, Key};
use crate::jsonpath::JsonPath;

pub const DEFAULT_NAMESPACE: &str = "https://hasura.io/jwt/claims";

/// Where the token is read from (`header`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenLocation {
    /// `Authorization: Bearer <token>`
    Authorization,
    /// The named cookie.
    Cookie(String),
    /// The raw value of the named header (lower-cased).
    Header(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimsFormat {
    Json,
    StringifiedJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Namespace {
    Key(String),
    Path(JsonPath),
}

/// A `claims_map` value: a literal, or a JSONPath into the token's claims.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimValue {
    Literal(Value),
    Path {
        path: JsonPath,
        default: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClaimsMap {
    pub allowed_roles: ClaimValue,
    pub default_role: ClaimValue,
    /// Other session variables (names lower-cased).
    pub others: Vec<(String, ClaimValue)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Claims {
    Namespace {
        namespace: Namespace,
        format: ClaimsFormat,
    },
    Map(Box<ClaimsMap>),
}

/// Where the keys come from.
#[derive(Debug)]
pub enum KeySource {
    Static(Box<Key>),
    Url(String),
}

/// One parsed JWT config.
#[derive(Debug)]
pub struct JwtConfig {
    pub keys: KeySource,
    pub claims: Claims,
    pub audience: Option<Vec<String>>,
    pub issuer: Option<String>,
    /// Leeway for `exp` and `nbf`, in seconds.
    pub allowed_skew: f64,
    pub header: TokenLocation,
    pub require_audience: bool,
    pub require_issuer: bool,
    /// The config as given, for `/v1alpha1/config`.
    source: Map<String, Value>,
}

impl JwtConfig {
    pub fn parse(value: &Value) -> Result<Self, String> {
        let obj = value
            .as_object()
            .ok_or_else(|| format!("expected Object, but encountered {}", kind(value)))?;
        let string = |name: &str| -> Result<Option<String>, String> {
            match obj.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(v) => Err(format!(
                    "parsing {name} failed, expected String, but encountered {}",
                    kind(v)
                )),
            }
        };

        let keys = match (string("key")?, string("jwk_url")?) {
            (None, None) => return Err("key and jwk_url both cannot be empty".into()),
            (Some(_), Some(_)) => return Err("key, jwk_url both cannot be present".into()),
            (Some(key), None) => {
                let ty =
                    string("type")?.ok_or("Invalid JWK: Key type: <missing> is not supported")?;
                KeySource::Static(Box::new(keys::static_key(&ty, &key)?))
            }
            (None, Some(url)) => KeySource::Url(url),
        };

        let format = match string("claims_format")?.as_deref() {
            None | Some("json") => ClaimsFormat::Json,
            Some("stringified_json") => ClaimsFormat::StringifiedJson,
            Some(other) => {
                return Err(format!(
                    "unexpected claims_format \"{other}\", expected \"json\" or \"stringified_json\""
                ));
            }
        };
        let namespace = match (
            string("claims_namespace")?,
            string("claims_namespace_path")?,
        ) {
            (Some(_), Some(_)) => {
                return Err("claims_namespace and claims_namespace_path both cannot be set".into());
            }
            (Some(ns), None) => Namespace::Key(ns),
            (None, Some(path)) => Namespace::Path(
                JsonPath::parse(&path)
                    .map_err(|e| format!("invalid JSON path claims_namespace_path error: {e}"))?,
            ),
            (None, None) => Namespace::Key(DEFAULT_NAMESPACE.to_owned()),
        };
        let claims = match obj.get("claims_map") {
            None | Some(Value::Null) => Claims::Namespace { namespace, format },
            Some(map) => Claims::Map(Box::new(parse_claims_map(map)?)),
        };

        let audience = match obj.get("audience") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(vec![s.clone()]),
            Some(Value::Array(a)) => Some(
                a.iter()
                    .map(|v| {
                        v.as_str().map(str::to_owned).ok_or_else(|| {
                            format!(
                                "parsing audience failed, expected String, but encountered {}",
                                kind(v)
                            )
                        })
                    })
                    .collect::<Result<_, _>>()?,
            ),
            Some(v) => {
                return Err(format!(
                    "parsing audience failed, expected String or Array, but encountered {}",
                    kind(v)
                ));
            }
        };

        let allowed_skew = match obj.get("allowed_skew") {
            None | Some(Value::Null) => 0.0,
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0).abs(),
            Some(v) => {
                return Err(format!(
                    "parsing allowed_skew failed, expected Number, but encountered {}",
                    kind(v)
                ));
            }
        };

        let header = parse_header(obj.get("header"))?;

        let (mut require_audience, mut require_issuer) = (false, false);
        if let Some(extra) = obj.get("extra_required_claims").filter(|v| !v.is_null()) {
            let list = extra.as_array().ok_or_else(|| {
                format!(
                    "parsing extra_required_claims failed, expected Array, but encountered {}",
                    kind(extra)
                )
            })?;
            for v in list {
                match v.as_str() {
                    Some("audience") => require_audience = true,
                    Some("issuer") => require_issuer = true,
                    _ => {
                        let shown = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                        return Err(format!("Unknown extra_required_claims value: {shown}"));
                    }
                }
            }
        }

        Ok(JwtConfig {
            keys,
            claims,
            audience,
            issuer: string("issuer")?,
            allowed_skew,
            header,
            require_audience,
            require_issuer,
            source: obj.clone(),
        })
    }

    /// The config with `type` and `key` redacted, as `/v1alpha1/config` shows it.
    pub fn redacted_json(&self) -> Value {
        let mut obj = self.source.clone();
        if obj.contains_key("type") {
            obj.insert("type".into(), json!("<TYPE REDACTED>"));
        }
        if obj.contains_key("key") {
            obj.insert("key".into(), json!("<JWK REDACTED>"));
        }
        Value::Object(obj)
    }
}

fn parse_header(value: Option<&Value>) -> Result<TokenLocation, String> {
    let Some(v) = value.filter(|v| !v.is_null()) else {
        return Ok(TokenLocation::Authorization);
    };
    let bad = || "expected 'type' is 'Authorization' or 'Cookie' or 'CustomHeader'".to_owned();
    let ty = v.get("type").and_then(Value::as_str).ok_or_else(bad)?;
    let name = || {
        v.get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("key \"name\" not found for header type {ty}"))
    };
    match ty.to_ascii_lowercase().as_str() {
        "authorization" => Ok(TokenLocation::Authorization),
        "cookie" => Ok(TokenLocation::Cookie(name()?)),
        "customheader" => Ok(TokenLocation::Header(name()?.to_ascii_lowercase())),
        _ => Err(bad()),
    }
}

const ALLOWED_ROLES: &str = "x-hasura-allowed-roles";
const DEFAULT_ROLE: &str = "x-hasura-default-role";

fn parse_claims_map(map: &Value) -> Result<ClaimsMap, String> {
    let obj = map.as_object().ok_or_else(|| {
        format!(
            "parsing claims_map failed, expected Object, but encountered {}",
            kind(map)
        )
    })?;
    let mut allowed_roles = None;
    let mut default_role = None;
    let mut others = vec![];
    for (name, value) in obj {
        let lower = name.to_ascii_lowercase();
        let parsed = parse_claim_value(&lower, value)?;
        match lower.as_str() {
            ALLOWED_ROLES => allowed_roles = Some(parsed),
            DEFAULT_ROLE => default_role = Some(parsed),
            _ => others.push((lower, parsed)),
        }
    }
    Ok(ClaimsMap {
        allowed_roles: allowed_roles
            .ok_or_else(|| format!("{ALLOWED_ROLES} is expected but not found"))?,
        default_role: default_role
            .ok_or_else(|| format!("{DEFAULT_ROLE} is expected but not found"))?,
        others,
    })
}

/// A literal is checked now: a role list, a role, or a string session variable.
fn parse_claim_value(name: &str, value: &Value) -> Result<ClaimValue, String> {
    let check = |v: &Value| -> Result<(), String> {
        let ok = match name {
            ALLOWED_ROLES => v
                .as_array()
                .is_some_and(|a| a.iter().all(|r| r.as_str().is_some_and(|s| !s.is_empty()))),
            _ => v
                .as_str()
                .is_some_and(|s| name != DEFAULT_ROLE || !s.is_empty()),
        };
        if ok {
            Ok(())
        } else {
            let expected = if name == ALLOWED_ROLES {
                "a list of roles"
            } else {
                "a string"
            };
            Err(format!(
                "invalid value for {name} in claims_map; expected {expected}, found: {v}"
            ))
        }
    };
    match value {
        Value::Object(o) => {
            let path = o
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("key \"path\" not found for {name} in claims_map"))?;
            let path = JsonPath::parse(path)
                .map_err(|e| format!("invalid JSON path for {name} in claims_map: {e}"))?;
            let default = o.get("default").filter(|d| !d.is_null()).cloned();
            if let Some(d) = &default {
                check(d)?;
            }
            Ok(ClaimValue::Path { path, default })
        }
        v => {
            check(v)?;
            Ok(ClaimValue::Literal(v.clone()))
        }
    }
}

/// aeson's name for a JSON value's type.
pub(crate) fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HS_KEY: &str = "an-hmac-secret-that-is-32-bytes!";

    fn err(v: Value) -> String {
        JwtConfig::parse(&v).unwrap_err()
    }

    #[test]
    fn startup_errors() {
        assert_eq!(
            err(json!({"type": "HS256"})),
            "key and jwk_url both cannot be empty"
        );
        assert_eq!(
            err(json!({"type": "HS256", "key": HS_KEY, "jwk_url": "http://x"})),
            "key, jwk_url both cannot be present"
        );
        assert_eq!(
            err(json!({"type": "HS256", "key": "short"})),
            "Invalid JWK: Key size too small; should be atleast 32 characters"
        );
        assert_eq!(
            err(json!({"type": "HS512", "key": HS_KEY})),
            "Invalid JWK: Key size too small; should be atleast 64 characters"
        );
        assert_eq!(
            err(json!({"type": "XX256", "key": HS_KEY})),
            "Invalid JWK: Key type: XX256 is not supported"
        );
        assert!(
            err(json!({"type": "RS256", "key": "not pem"}))
                .starts_with("Invalid JWK: Could not decode PEM: ")
        );
        assert_eq!(
            err(
                json!({"type": "HS256", "key": HS_KEY, "claims_namespace": "a", "claims_namespace_path": "$.a"})
            ),
            "claims_namespace and claims_namespace_path both cannot be set"
        );
        assert!(
            err(json!({"type": "HS256", "key": HS_KEY, "claims_namespace_path": "$["}))
                .starts_with("invalid JSON path claims_namespace_path error: ")
        );
        assert_eq!(
            err(json!({"type": "HS256", "key": HS_KEY, "header": {"type": "Query"}})),
            "expected 'type' is 'Authorization' or 'Cookie' or 'CustomHeader'"
        );
        assert_eq!(
            err(json!({"type": "HS256", "key": HS_KEY, "extra_required_claims": ["subject"]})),
            "Unknown extra_required_claims value: subject"
        );
        assert_eq!(
            err(
                json!({"type": "HS256", "key": HS_KEY, "claims_map": {"x-hasura-default-role": "user"}})
            ),
            "x-hasura-allowed-roles is expected but not found"
        );
        assert_eq!(
            err(
                json!({"type": "HS256", "key": HS_KEY, "claims_map": {"x-hasura-allowed-roles": ["user"]}})
            ),
            "x-hasura-default-role is expected but not found"
        );
    }

    #[test]
    fn full_config() {
        let c = JwtConfig::parse(&json!({
            "jwk_url": "https://example.com/jwks",
            "claims_namespace_path": "$.hasura.claims",
            "claims_format": "stringified_json",
            "audience": ["a", "b"],
            "issuer": "iss",
            "allowed_skew": 60,
            "header": {"type": "cookie", "name": "token"},
            "extra_required_claims": ["audience", "issuer"]
        }))
        .unwrap();
        assert!(matches!(c.keys, KeySource::Url(ref u) if u == "https://example.com/jwks"));
        assert!(matches!(
            c.claims,
            Claims::Namespace {
                format: ClaimsFormat::StringifiedJson,
                namespace: Namespace::Path(_)
            }
        ));
        assert_eq!(c.audience, Some(vec!["a".into(), "b".into()]));
        assert_eq!(c.issuer.as_deref(), Some("iss"));
        assert_eq!(c.allowed_skew, 60.0);
        assert_eq!(c.header, TokenLocation::Cookie("token".into()));
        assert!(c.require_audience && c.require_issuer);

        let c = JwtConfig::parse(&json!({
            "type": "HS256", "key": HS_KEY,
            "header": {"type": "CustomHeader", "name": "X-Token"},
            "claims_map": {
                "x-hasura-allowed-roles": {"path": "$.roles"},
                "X-Hasura-Default-Role": {"path": "$.roles[0]", "default": "user"},
                "x-hasura-org": "acme"
            }
        }))
        .unwrap();
        assert_eq!(c.header, TokenLocation::Header("x-token".into()));
        let Claims::Map(m) = &c.claims else { panic!() };
        assert_eq!(
            m.others,
            vec![("x-hasura-org".into(), ClaimValue::Literal(json!("acme")))]
        );
        assert_eq!(c.redacted_json()["key"], json!("<JWK REDACTED>"));
        assert_eq!(c.redacted_json()["type"], json!("<TYPE REDACTED>"));
    }
}
