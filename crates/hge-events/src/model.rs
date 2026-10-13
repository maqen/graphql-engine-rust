//! Typed event-trigger metadata (`event_triggers` in a table's YAML) and its resolution
//! against the environment.
//!
//! The metadata model in `hge-metadata` keeps event triggers as raw JSON so they round-trip
//! untouched; this module gives them types where the engine needs them.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::json::Json;

/// Longest trigger name V2 accepts: 63 − len("notify_hasura_") − len("_INSERT").
pub const MAX_TRIGGER_NAME_LENGTH: usize = 42;

/// Default HTTP timeout when `retry_conf.timeout_sec` is absent.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 60;

/// `EventTriggerConf`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventTriggerConf {
    pub name: String,
    pub definition: TriggerOpsDef,
    pub retry_conf: RetryConf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_from_env: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderConf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_transform: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_transform: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_config: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trigger_on_replication: bool,
}

/// `TriggerOpsDef`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerOpsDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert: Option<SubscribeOpSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<SubscribeOpSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<SubscribeOpSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_manual: Option<bool>,
}

impl TriggerOpsDef {
    pub fn op(&self, op: Op) -> Option<&SubscribeOpSpec> {
        match op {
            Op::Insert => self.insert.as_ref(),
            Op::Update => self.update.as_ref(),
            Op::Delete => self.delete.as_ref(),
        }
    }

    pub fn manual_enabled(&self) -> bool {
        self.enable_manual.unwrap_or(false)
    }
}

/// The row operations a trigger can listen to (MANUAL never gets a SQL trigger).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Insert,
    Update,
    Delete,
}

impl Op {
    pub const ALL: [Op; 3] = [Op::Insert, Op::Update, Op::Delete];

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Insert => "INSERT",
            Op::Update => "UPDATE",
            Op::Delete => "DELETE",
        }
    }
}

/// `SubscribeOpSpec`: the columns that fire the trigger and the columns delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscribeOpSpec {
    pub columns: SubscribeColumns,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<SubscribeColumns>,
}

/// `"*"` or a list of column names.
#[derive(Debug, Clone, PartialEq)]
pub enum SubscribeColumns {
    Star,
    Columns(Vec<String>),
}

impl Serialize for SubscribeColumns {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            SubscribeColumns::Star => s.serialize_str("*"),
            SubscribeColumns::Columns(c) => c.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for SubscribeColumns {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) if s == "*" => Ok(SubscribeColumns::Star),
            Value::String(_) => Err(serde::de::Error::custom("only * or [] allowed")),
            v @ Value::Array(_) => serde_json::from_value(v)
                .map(SubscribeColumns::Columns)
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom("only * or [] allowed")),
        }
    }
}

/// `RetryConf` (event triggers).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetryConf {
    pub num_retries: i64,
    pub interval_sec: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_sec: Option<i64>,
}

impl Default for RetryConf {
    fn default() -> Self {
        RetryConf {
            num_retries: 0,
            interval_sec: 10,
            timeout_sec: Some(60),
        }
    }
}

/// `HeaderConf`: `{name, value}` or `{name, value_from_env}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeaderConf {
    pub name: String,
    #[serde(flatten)]
    pub value: HeaderValue,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum HeaderValue {
    /// A template; `{{VAR}}` is replaced from the environment.
    #[serde(rename = "value")]
    Value(String),
    #[serde(rename = "value_from_env")]
    Env(String),
}

impl<'de> Deserialize<'de> for HeaderConf {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            name: String,
            value: Option<String>,
            value_from_env: Option<String>,
        }
        let raw = Raw::deserialize(d)?;
        let value = match (raw.value, raw.value_from_env) {
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "expecting only one of value or value_from_env keys",
                ));
            }
            (None, None) => {
                return Err(serde::de::Error::custom(
                    "expecting value or value_from_env keys",
                ));
            }
            (Some(v), None) => HeaderValue::Value(v),
            (None, Some(env)) if env.starts_with("HASURA_GRAPHQL_") => {
                return Err(serde::de::Error::custom(format!(
                    "env variables starting with \"HASURA_GRAPHQL_\" are not allowed in value_from_env: {env}"
                )));
            }
            (None, Some(env)) => HeaderValue::Env(env),
        };
        Ok(HeaderConf {
            name: raw.name,
            value,
        })
    }
}

impl HeaderConf {
    /// The invocation-log form (`{"name": …, "value": …}` / `{"name": …, "value_from_env": …}`).
    pub fn to_json(&self) -> Json {
        let (k, v) = match &self.value {
            HeaderValue::Value(v) => ("value", v),
            HeaderValue::Env(v) => ("value_from_env", v),
        };
        Json::object([("name", Json::str(&self.name)), (k, Json::str(v))])
    }

    /// A plain `{name, value}` header (as logged for default and response headers).
    pub fn plain(name: impl Into<String>, value: impl Into<String>) -> Self {
        HeaderConf {
            name: name.into(),
            value: HeaderValue::Value(value.into()),
        }
    }
}

/// Whether a trigger name is legal (`^[A-Za-z]+[A-Za-z0-9_\-]*$`).
pub fn is_legal_trigger_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Where values for `{{VAR}}` templates and `*_from_env` keys come from.
pub trait Env {
    fn var(&self, name: &str) -> Option<String>;
}

/// The process environment.
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

impl Env for BTreeMap<String, String> {
    fn var(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}

/// Substitute `{{VAR}}` occurrences. On failure returns the names of the missing variables.
pub fn resolve_template(template: &str, env: &dyn Env) -> Result<String, Vec<String>> {
    let mut out = String::with_capacity(template.len());
    let mut missing = vec![];
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else {
            break;
        };
        out.push_str(&rest[..start]);
        let var = rest[start + 2..start + 2 + len].trim();
        match env.var(var) {
            Some(v) => out.push_str(&v),
            None => missing.push(var.to_owned()),
        }
        rest = &rest[start + 2 + len + 2..];
    }
    out.push_str(rest);
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(missing)
    }
}

/// A header with its value resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedHeader {
    pub conf: HeaderConf,
    pub value: String,
}

/// Resolve configured headers (`value` templates and `value_from_env`).
pub fn resolve_headers(
    headers: &[HeaderConf],
    env: &dyn Env,
) -> Result<Vec<ResolvedHeader>, String> {
    headers
        .iter()
        .map(|h| {
            let value = match &h.value {
                HeaderValue::Value(t) => resolve_template(t, env).map_err(|missing| {
                    format!(
                        "template cannot be resolved: Value for environment variables not found: {}",
                        missing.join(", ")
                    )
                })?,
                HeaderValue::Env(var) => env
                    .var(var)
                    .ok_or_else(|| format!("environment variable '{var}' not set"))?,
            };
            Ok(ResolvedHeader {
                conf: h.clone(),
                value,
            })
        })
        .collect()
}

/// A webhook URL resolved from the environment, plus what logs may show instead of it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedWebhook {
    pub url: String,
    /// The template text or env var name (V2 never logs the resolved URL).
    pub display: String,
}

/// Resolve `webhook` / `webhook_from_env` the way V2 does at schema-build time.
pub fn resolve_webhook(
    webhook: Option<&str>,
    webhook_from_env: Option<&str>,
    env: &dyn Env,
) -> Result<ResolvedWebhook, String> {
    match (webhook, webhook_from_env) {
        (Some(template), None) => {
            let url = resolve_template(template, env).map_err(|missing| {
                format!(
                    "Value for environment variables not found: {}",
                    missing.join(", ")
                )
            })?;
            Ok(ResolvedWebhook {
                url,
                display: template.to_owned(),
            })
        }
        (None, Some(var)) => {
            let url = env
                .var(var)
                .ok_or_else(|| format!("environment variable '{var}' not set"))?;
            Ok(ResolvedWebhook {
                url,
                display: var.to_owned(),
            })
        }
        _ => Err("expected webhook or webhook_from_env".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("HOOK".to_owned(), "http://h:1".to_owned()),
            ("KEY".to_owned(), "secret".to_owned()),
        ])
    }

    #[test]
    fn parses_v2_trigger() {
        let conf: EventTriggerConf = serde_json::from_value(json!({
            "name": "playlist_created",
            "definition": {"enable_manual": true, "insert": {"columns": "*"},
                           "update": {"columns": ["a"], "payload": ["a", "b"]}},
            "retry_conf": {"interval_sec": 10, "num_retries": 0, "timeout_sec": 60},
            "webhook_from_env": "EVENT_WEBHOOK_URL",
            "headers": [{"name": "X-Key", "value_from_env": "KEY"}, {"name": "X-S", "value": "v"}]
        }))
        .unwrap();
        assert_eq!(
            conf.definition.insert.as_ref().unwrap().columns,
            SubscribeColumns::Star
        );
        assert_eq!(
            conf.definition.update.as_ref().unwrap().payload,
            Some(SubscribeColumns::Columns(vec!["a".into(), "b".into()]))
        );
        assert_eq!(conf.headers[0].value, HeaderValue::Env("KEY".into()));
        assert!(conf.definition.manual_enabled());
    }

    #[test]
    fn header_errors() {
        let err = |v| {
            serde_json::from_value::<HeaderConf>(v)
                .unwrap_err()
                .to_string()
        };
        assert!(err(json!({"name": "a"})).contains("expecting value or value_from_env keys"));
        assert!(
            err(json!({"name": "a", "value": "x", "value_from_env": "Y"})).contains("only one")
        );
        assert!(
            err(json!({"name": "a", "value_from_env": "HASURA_GRAPHQL_X"}))
                .contains("HASURA_GRAPHQL_")
        );
        assert!(serde_json::from_value::<SubscribeColumns>(json!("a")).is_err());
    }

    #[test]
    fn templates() {
        let env = env();
        assert_eq!(
            resolve_template("{{HOOK}}/x", &env).unwrap(),
            "http://h:1/x"
        );
        assert_eq!(resolve_template("{{ HOOK }}", &env).unwrap(), "http://h:1");
        assert_eq!(
            resolve_template("{{A}}{{B}}", &env).unwrap_err(),
            vec!["A", "B"]
        );
        assert_eq!(resolve_template("plain {{", &env).unwrap(), "plain {{");
        let w = resolve_webhook(None, Some("HOOK"), &env).unwrap();
        assert_eq!((w.url.as_str(), w.display.as_str()), ("http://h:1", "HOOK"));
        assert_eq!(
            resolve_webhook(None, Some("NOPE"), &env).unwrap_err(),
            "environment variable 'NOPE' not set"
        );
        let hs = resolve_headers(
            &[HeaderConf {
                name: "k".into(),
                value: HeaderValue::Env("KEY".into()),
            }],
            &env,
        )
        .unwrap();
        assert_eq!(hs[0].value, "secret");
        assert_eq!(
            hs[0].conf.to_json().encode(),
            r#"{"name":"k","value_from_env":"KEY"}"#
        );
    }

    #[test]
    fn trigger_names() {
        assert!(is_legal_trigger_name("a-b_1"));
        assert!(!is_legal_trigger_name("1a"));
        assert!(!is_legal_trigger_name("a b"));
        assert!(!is_legal_trigger_name(""));
    }
}
