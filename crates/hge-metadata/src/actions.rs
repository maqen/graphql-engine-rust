//! Typed actions and custom types.
//!
//! [`Metadata`](crate::Metadata) keeps `actions` and `custom_types` as raw JSON so that
//! `export_metadata` round-trips them unchanged; these types are the parsed view used to
//! build the schema and run actions. Parsing follows V2's lenient `FromJSON` instances:
//! unknown keys are ignored.

use serde::Deserialize;
use serde_json::Value;

use hge_core::{Code, Error, QualifiedTable};
use hge_transforms::{ParseError, RequestTransform, ResponseTransform};

/// `definition.timeout` default, in seconds.
pub const DEFAULT_ACTION_TIMEOUT_SECS: u64 = 30;

/// Client headers never forwarded by `forward_client_headers` unless
/// `ignored_client_headers` says otherwise (compared case-insensitively).
pub const DEFAULT_IGNORED_CLIENT_HEADERS: &[&str] = &[
    "Content-Length",
    "Content-MD5",
    "User-Agent",
    "Host",
    "Origin",
    "Referer",
    "Accept",
    "Accept-Encoding",
    "Accept-Language",
    "Accept-Datetime",
    "Cache-Control",
    "Connection",
    "DNT",
    "Content-Type",
];

/// An element of the top-level `actions` array.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub name: String,
    pub definition: ActionDefinition,
    pub comment: Option<String>,
    pub permissions: Vec<ActionPermission>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ActionPermission {
    pub role: String,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionType {
    Query,
    Mutation(ActionKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Synchronous,
    Asynchronous,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActionDefinition {
    pub action_type: ActionType,
    pub arguments: Vec<ArgumentDefinition>,
    /// A GraphQL type (`Foo`, `[Foo!]!`).
    pub output_type: String,
    /// URL template (`{{ACTION_BASE_URL}}/login`).
    pub handler: String,
    pub headers: Vec<HeaderConf>,
    pub forward_client_headers: bool,
    pub ignored_client_headers: Vec<String>,
    pub timeout_secs: u64,
    /// Parsed transforms; their Kriti templates are only parse-checked by the metadata
    /// API, so a template error surfaces when the action is called.
    pub request_transform: Option<RequestTransform>,
    pub response_transform: Option<ResponseTransform>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ArgumentDefinition {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// A configured header: a (templated) value or the name of an environment variable.
#[derive(Debug, Clone, PartialEq)]
pub enum HeaderConf {
    Value { name: String, value: String },
    FromEnv { name: String, env: String },
}

impl HeaderConf {
    pub fn name(&self) -> &str {
        match self {
            HeaderConf::Value { name, .. } | HeaderConf::FromEnv { name, .. } => name,
        }
    }

    /// Parse one header entry with V2's error messages.
    pub fn from_json(v: &Value) -> Result<HeaderConf, String> {
        let obj = v.as_object().ok_or("expecting object for headers")?;
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .ok_or("the key 'name' was not present")?
            .to_owned();
        match (obj.get("value"), obj.get("value_from_env")) {
            (Some(_), Some(_)) => Err("expecting only one of value or value_from_env keys".into()),
            (Some(value), None) => Ok(HeaderConf::Value {
                name,
                value: value
                    .as_str()
                    .ok_or("expected String for 'value'")?
                    .to_owned(),
            }),
            (None, Some(env)) => {
                let env = env
                    .as_str()
                    .ok_or("expected String for 'value_from_env'")?
                    .to_owned();
                if env.starts_with("HASURA_GRAPHQL_") {
                    return Err(format!(
                        "env variables starting with \"HASURA_GRAPHQL_\" are not allowed in value_from_env: {env}"
                    ));
                }
                Ok(HeaderConf::FromEnv { name, env })
            }
            (None, None) => Err("expecting value or value_from_env keys".into()),
        }
    }
}

/// A parse failure of one action: its name (if readable) and the message.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionParseError {
    pub name: Option<String>,
    pub message: String,
}

impl Action {
    /// Parse one element of `actions`.
    pub fn from_json(v: &Value) -> Result<Action, ActionParseError> {
        let name = v.get("name").and_then(Value::as_str).map(str::to_owned);
        let err = |message: String| ActionParseError {
            name: name.clone(),
            message,
        };
        let action_name = name
            .clone()
            .ok_or_else(|| err("the key 'name' was not present".into()))?;
        let definition = v
            .get("definition")
            .ok_or_else(|| err("the key 'definition' was not present".into()))?;
        let definition = ActionDefinition::from_json(definition).map_err(err)?;
        let comment = v.get("comment").and_then(Value::as_str).map(str::to_owned);
        let permissions = match v.get("permissions") {
            None | Some(Value::Null) => vec![],
            Some(p) => serde_json::from_value(p.clone())
                .map_err(|e| err(format!("invalid permissions: {e}")))?,
        };
        Ok(Action {
            name: action_name,
            definition,
            comment,
            permissions,
        })
    }
}

impl ActionDefinition {
    pub fn from_json(v: &Value) -> Result<ActionDefinition, String> {
        let obj = v
            .as_object()
            .ok_or("expected Object for action definition")?;
        let str_field = |key: &str| -> Result<Option<String>, String> {
            match obj.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(format!("expected String for '{key}'")),
            }
        };
        let action_type = match str_field("type")?.as_deref() {
            None | Some("mutation") => {
                // `kind` is only read for mutations (the CLI writes `kind: ""` for queries).
                let kind = match str_field("kind")?.as_deref() {
                    None | Some("synchronous") => ActionKind::Synchronous,
                    Some("asynchronous") => ActionKind::Asynchronous,
                    Some(other) => {
                        return Err(format!(
                            "expected synchronous or asynchronous, but found {other}"
                        ));
                    }
                };
                ActionType::Mutation(kind)
            }
            Some("query") => ActionType::Query,
            Some(other) => return Err(format!("expected mutation or query, but found {other}")),
        };
        let arguments = match obj.get("arguments") {
            None | Some(Value::Null) => vec![],
            Some(a) => {
                serde_json::from_value(a.clone()).map_err(|e| format!("invalid arguments: {e}"))?
            }
        };
        let output_type =
            str_field("output_type")?.ok_or("the key 'output_type' was not present")?;
        let handler = str_field("handler")?.ok_or("the key 'handler' was not present")?;
        let headers = match obj.get("headers") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(items)) => items
                .iter()
                .map(HeaderConf::from_json)
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("expected Array for 'headers'".into()),
        };
        let forward_client_headers = match obj.get("forward_client_headers") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("expected Boolean for 'forward_client_headers'".into()),
        };
        let ignored_client_headers = match obj.get("ignored_client_headers") {
            None | Some(Value::Null) => DEFAULT_IGNORED_CLIENT_HEADERS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| format!("invalid ignored_client_headers: {e}"))?,
        };
        let timeout_secs = match obj.get("timeout") {
            None | Some(Value::Null) => DEFAULT_ACTION_TIMEOUT_SECS,
            Some(Value::Number(n)) => match (n.as_u64(), n.as_f64()) {
                (Some(t), _) => t,
                (None, Some(f)) if f < 0.0 => return Err("timeout value cannot be negative".into()),
                _ => return Err(format!("{n} is out of bounds")),
            },
            Some(_) => return Err("expected Number for 'timeout'".into()),
        };
        let request_transform =
            transform(obj.get("request_transform"), RequestTransform::from_json)
                .map_err(|e| format!("request_transform: {e}"))?;
        let response_transform =
            transform(obj.get("response_transform"), ResponseTransform::from_json)
                .map_err(|e| format!("response_transform: {e}"))?;
        Ok(ActionDefinition {
            action_type,
            arguments,
            output_type,
            handler,
            headers,
            forward_client_headers,
            ignored_client_headers,
            timeout_secs,
            request_transform,
            response_transform,
        })
    }
}

fn transform<T>(
    v: Option<&Value>,
    parse: impl Fn(&Value) -> Result<T, ParseError>,
) -> Result<Option<T>, ParseError> {
    v.filter(|v| !v.is_null()).map(parse).transpose()
}

/// Decode every action's transforms. As in V2's `FromJSON`, a malformed transform makes
/// the whole metadata document unreadable: `parse-failed` at
/// `$.actions[i].definition.request_transform…`.
pub fn check_transforms(actions: &[Value]) -> Result<(), Error> {
    for (i, action) in actions.iter().enumerate() {
        let Some(def) = action.get("definition") else {
            continue;
        };
        let request = transform(def.get("request_transform"), RequestTransform::from_json);
        let response = transform(def.get("response_transform"), ResponseTransform::from_json);
        for (key, result) in [
            ("request_transform", request.map(|_| ())),
            ("response_transform", response.map(|_| ())),
        ] {
            if let Err(e) = result {
                let rest = e.path.strip_prefix('$').unwrap_or(&e.path);
                return Err(Error::new(Code::ParseFailed, e.message)
                    .with_path(format!("$.actions[{i}].definition.{key}{rest}")));
            }
        }
    }
    Ok(())
}

/// `custom_types`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct CustomTypes {
    pub input_objects: Vec<InputObjectType>,
    pub objects: Vec<ObjectType>,
    pub scalars: Vec<ScalarType>,
    pub enums: Vec<EnumType>,
}

impl CustomTypes {
    pub fn from_json(v: Option<&Value>) -> Result<CustomTypes, String> {
        match v {
            None | Some(Value::Null) => Ok(CustomTypes::default()),
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| e.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InputObjectType {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub fields: Vec<InputObjectField>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InputObjectField {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ObjectType {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub fields: Vec<ObjectField>,
    #[serde(default)]
    pub relationships: Vec<CustomTypeRelationship>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ObjectField {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Must be absent or null; V2 rejects objects with field arguments.
    #[serde(default)]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelType {
    Object,
    Array,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CustomTypeRelationship {
    pub name: String,
    #[serde(rename = "type")]
    pub rel_type: RelType,
    #[serde(default = "default_source")]
    pub source: String,
    pub remote_table: QualifiedTable,
    /// Object field name → table column, in metadata order.
    pub field_mapping: indexmap::IndexMap<String, String>,
}

fn default_source() -> String {
    "default".to_owned()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ScalarType {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EnumType {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub values: Vec<EnumValue>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EnumValue {
    pub value: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_deprecated: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_definitions_like_v2() {
        let a = Action::from_json(&json!({
            "name": "q",
            "definition": {"type": "query", "kind": "", "output_type": "Out", "handler": "{{B}}/q"}
        }))
        .unwrap();
        assert_eq!(a.definition.action_type, ActionType::Query);
        assert_eq!(a.definition.timeout_secs, 30);
        assert_eq!(a.definition.ignored_client_headers.len(), 14);

        let m = ActionDefinition::from_json(&json!({
            "kind": "asynchronous", "output_type": "Out", "handler": "h", "timeout": 5,
            "headers": [{"name": "X", "value_from_env": "SECRET"}]
        }))
        .unwrap();
        assert_eq!(
            m.action_type,
            ActionType::Mutation(ActionKind::Asynchronous)
        );
        assert_eq!(m.timeout_secs, 5);

        let bad_kind =
            ActionDefinition::from_json(&json!({"kind": "", "output_type": "O", "handler": "h"}));
        assert!(bad_kind.is_err());
        let neg = ActionDefinition::from_json(
            &json!({"timeout": -1, "output_type": "O", "handler": "h"}),
        );
        assert_eq!(neg.unwrap_err(), "timeout value cannot be negative");
        let frac = ActionDefinition::from_json(
            &json!({"timeout": 1.5, "output_type": "O", "handler": "h"}),
        );
        assert_eq!(frac.unwrap_err(), "1.5 is out of bounds");
    }

    #[test]
    fn header_conf_errors() {
        assert_eq!(
            HeaderConf::from_json(&json!({"name": "a"})).unwrap_err(),
            "expecting value or value_from_env keys"
        );
        assert_eq!(
            HeaderConf::from_json(&json!({"name": "a", "value": "x", "value_from_env": "y"}))
                .unwrap_err(),
            "expecting only one of value or value_from_env keys"
        );
        assert!(
            HeaderConf::from_json(&json!({"name": "a", "value_from_env": "HASURA_GRAPHQL_X"}))
                .unwrap_err()
                .contains("are not allowed in value_from_env: HASURA_GRAPHQL_X")
        );
    }

    #[test]
    fn parses_custom_types() {
        let ct = CustomTypes::from_json(Some(&json!({
            "objects": [{
                "name": "PriceQuote",
                "fields": [{"name": "trackId", "type": "Int!"}],
                "relationships": [{
                    "name": "track", "type": "object",
                    "remote_table": {"schema": "public", "name": "track"},
                    "field_mapping": {"trackId": "track_id"}
                }]
            }],
            "enums": [{"name": "Color", "values": [{"value": "RED", "description": null}]}]
        })))
        .unwrap();
        assert_eq!(ct.objects[0].relationships[0].source, "default");
        assert_eq!(ct.enums[0].values[0].value, "RED");
    }
}
