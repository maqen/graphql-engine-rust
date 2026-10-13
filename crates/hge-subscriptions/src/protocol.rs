//! The two WebSocket sub-protocols V2 speaks on `/v1/graphql` (spec 09 §2).
//!
//! Clients of either protocol may send either protocol's message types (V2 parses them with
//! one parser); the protocol only decides what the server sends.

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value as Json};

/// `graphql-transport-ws` (the `graphql-ws` npm package).
pub const GRAPHQL_TRANSPORT_WS: &str = "graphql-transport-ws";
/// `graphql-ws`: Apollo's legacy `subscriptions-transport-ws`.
pub const GRAPHQL_WS: &str = "graphql-ws";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `subscriptions-transport-ws` (sub-protocol `graphql-ws`). Also used when the client
    /// asks for no or an unknown sub-protocol.
    Apollo,
    /// `graphql-transport-ws`.
    GraphqlWs,
}

impl Protocol {
    /// The protocol for the sub-protocol the server selected.
    pub fn from_selected(selected: Option<&str>) -> Protocol {
        match selected {
            Some(GRAPHQL_TRANSPORT_WS) => Protocol::GraphqlWs,
            _ => Protocol::Apollo,
        }
    }

    /// The periodic keep-alive message.
    pub fn keep_alive(self) -> &'static str {
        match self {
            Protocol::Apollo => r#"{"type":"ka"}"#,
            Protocol::GraphqlWs => r#"{"type":"ping","payload":{"message":"keepalive"}}"#,
        }
    }

    /// A result message (`data` / `next`) around a raw JSON payload.
    pub fn data(self, id: &str, payload: &str) -> String {
        let ty = match self {
            Protocol::Apollo => "data",
            Protocol::GraphqlWs => "next",
        };
        message(ty, Some(id), Some(payload))
    }

    /// `{"data":null,"errors":[...]}` for a query/mutation that failed while executing.
    /// Apollo sends it as `data`; graphql-ws as an `error` holding the error list.
    pub fn execution_error(self, id: &str, errors: &[Json]) -> String {
        match self {
            Protocol::Apollo => {
                let payload = serde_json::json!({ "data": null, "errors": errors });
                message("data", Some(id), Some(&payload.to_string()))
            }
            Protocol::GraphqlWs => self.error(id, errors),
        }
    }

    /// An `error` message for a request that failed before executing (parse, validation,
    /// permissions): `{"errors":[...]}` (Apollo) or the bare list (graphql-ws).
    pub fn error(self, id: &str, errors: &[Json]) -> String {
        let payload = match self {
            Protocol::Apollo => serde_json::json!({ "errors": errors }),
            Protocol::GraphqlWs => Json::Array(errors.to_vec()),
        };
        message("error", Some(id), Some(&payload.to_string()))
    }
}

pub const CONNECTION_ACK: &str = r#"{"type":"connection_ack"}"#;

pub fn complete(id: &str) -> String {
    message("complete", Some(id), None)
}

pub fn connection_error(text: &str) -> String {
    message(
        "connection_error",
        None,
        Some(&Json::String(text.to_owned()).to_string()),
    )
}

/// The answer to a client `ping`: echoes `payload.message` when there was one.
pub fn pong(ping_message: Option<&str>) -> String {
    match ping_message {
        Some(m) => {
            let payload = serde_json::json!({ "message": m });
            message("pong", None, Some(&payload.to_string()))
        }
        None => message("pong", None, None),
    }
}

/// An `error` with a single error object as payload (Apollo `start-failed`).
pub fn error_object(id: &str, error: &Json) -> String {
    message("error", Some(id), Some(&error.to_string()))
}

/// `{"type":…,"id":…,"payload":…}` (V2's key order) with a raw JSON payload.
fn message(ty: &str, id: Option<&str>, payload: Option<&str>) -> String {
    let mut out = String::with_capacity(32 + payload.map_or(0, str::len));
    out.push_str(r#"{"type":""#);
    out.push_str(ty);
    out.push('"');
    if let Some(id) = id {
        out.push_str(r#","id":"#);
        out.push_str(&Json::String(id.to_owned()).to_string());
    }
    if let Some(p) = payload {
        out.push_str(r#","payload":"#);
        out.push_str(p);
    }
    out.push('}');
    out
}

/// The reason of a close frame as V2 sends it (`{"server_error_msg":"4400: ..."}`). The
/// message is shortened until the JSON fits the 123 bytes a close frame allows.
pub fn close_reason_json(code: u16, message: &str) -> String {
    let mut message = message.to_owned();
    loop {
        let json =
            serde_json::json!({ "server_error_msg": format!("{code}: {message}") }).to_string();
        if json.len() <= MAX_CLOSE_REASON || message.is_empty() {
            return truncate_reason(json);
        }
        // An input byte takes at most 6 bytes in JSON (`\u0000`): never cut too much.
        let cut = (json.len() - MAX_CLOSE_REASON).div_ceil(6);
        let mut end = message.len().saturating_sub(cut);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
}

const MAX_CLOSE_REASON: usize = 123;

/// Cut a close reason to 123 bytes.
pub fn truncate_reason(mut reason: String) -> String {
    if reason.len() > MAX_CLOSE_REASON {
        let mut end = MAX_CLOSE_REASON;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
    }
    reason
}

/// A GraphQL request inside `start` / `subscribe`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StartPayload {
    pub query: String,
    #[serde(default)]
    pub variables: Option<Json>,
    #[serde(default)]
    pub operation_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    /// `connection_init`, with `payload.headers` (in order) when given.
    ConnectionInit {
        headers: Vec<(String, String)>,
    },
    /// `start` / `subscribe`.
    Start {
        id: String,
        payload: StartPayload,
    },
    /// `stop` / `complete`.
    Stop {
        id: String,
    },
    /// `connection_terminate`.
    Terminate,
    /// `ping`, with `payload.message` when given.
    Ping {
        message: Option<String>,
    },
    Pong,
}

/// Parse a client frame. The error text follows V2 (`parsing ClientMessage failed: ...`).
pub fn parse_client_message(text: &[u8]) -> Result<ClientMessage, String> {
    parse(text).map_err(|e| format!("parsing ClientMessage failed: Error in $: {e}"))
}

fn parse(text: &[u8]) -> Result<ClientMessage, String> {
    let value: Json = serde_json::from_slice(text).map_err(|e| e.to_string())?;
    let Json::Object(obj) = value else {
        return Err("expected Object".to_owned());
    };
    let ty = match obj.get("type") {
        Some(Json::String(t)) => t.as_str(),
        Some(_) => return Err("parsing Text failed, expected String".to_owned()),
        None => return Err("key \"type\" not found".to_owned()),
    };
    let id = || match obj.get("id") {
        Some(Json::String(id)) => Ok(id.clone()),
        Some(_) => Err("parsing Text failed, expected String".to_owned()),
        None => Err("key \"id\" not found".to_owned()),
    };
    match ty {
        "connection_init" => Ok(ClientMessage::ConnectionInit {
            headers: init_headers(obj.get("payload"))?,
        }),
        "start" | "subscribe" => {
            let id = id()?;
            let payload = obj
                .get("payload")
                .ok_or_else(|| "key \"payload\" not found".to_owned())?;
            let payload = StartPayload::deserialize(payload).map_err(|e| e.to_string())?;
            Ok(ClientMessage::Start { id, payload })
        }
        "stop" | "complete" => Ok(ClientMessage::Stop { id: id()? }),
        "connection_terminate" => Ok(ClientMessage::Terminate),
        "ping" => Ok(ClientMessage::Ping {
            message: obj
                .get("payload")
                .and_then(|p| p.get("message"))
                .and_then(Json::as_str)
                .map(str::to_owned),
        }),
        "pong" => Ok(ClientMessage::Pong),
        other => Err(format!("unexpected type for ClientMessage: {other}")),
    }
}

/// `connection_init.payload.headers`: every value must be a string.
fn init_headers(payload: Option<&Json>) -> Result<Vec<(String, String)>, String> {
    let headers = match payload {
        None | Some(Json::Null) => return Ok(vec![]),
        Some(Json::Object(p)) => p.get("headers"),
        Some(_) => return Err("parsing ConnParams failed, expected Object".to_owned()),
    };
    let headers: &Map<String, Json> = match headers {
        None | Some(Json::Null) => return Ok(vec![]),
        Some(Json::Object(h)) => h,
        Some(_) => return Err("parsing HashMap failed, expected Object".to_owned()),
    };
    let mut out = IndexMap::with_capacity(headers.len());
    for (k, v) in headers {
        match v {
            Json::String(s) => {
                out.insert(k.clone(), s.clone());
            }
            _ => return Err(format!("parsing Text failed, expected String at key {k:?}")),
        }
    }
    Ok(out.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_protocols_messages() {
        assert_eq!(
            parse_client_message(
                br#"{"type":"connection_init","payload":{"headers":{"X-Hasura-Role":"user"}}}"#
            ),
            Ok(ClientMessage::ConnectionInit {
                headers: vec![("X-Hasura-Role".into(), "user".into())]
            })
        );
        assert_eq!(
            parse_client_message(br#"{"type":"connection_init"}"#),
            Ok(ClientMessage::ConnectionInit { headers: vec![] })
        );
        let start = parse_client_message(
            br#"{"id":"1","type":"subscribe","payload":{"query":"subscription { a }","variables":null}}"#,
        )
        .unwrap();
        assert!(matches!(start, ClientMessage::Start { ref id, .. } if id == "1"));
        assert_eq!(
            parse_client_message(br#"{"type":"complete","id":"1"}"#),
            Ok(ClientMessage::Stop { id: "1".into() })
        );
        assert_eq!(
            parse_client_message(br#"{"type":"ping","payload":{"message":"x","y":1}}"#),
            Ok(ClientMessage::Ping {
                message: Some("x".into())
            })
        );
    }

    #[test]
    fn parse_errors() {
        assert_eq!(
            parse_client_message(br#"{"type":"stop"}"#).unwrap_err(),
            r#"parsing ClientMessage failed: Error in $: key "id" not found"#
        );
        assert!(
            parse_client_message(br#"{"type":"nope"}"#)
                .unwrap_err()
                .ends_with("unexpected type for ClientMessage: nope")
        );
        assert!(parse_client_message(br#"{"type":"start","id":1,"payload":{}}"#).is_err());
        assert!(
            parse_client_message(br#"{"type":"connection_init","payload":{"headers":{"a":1}}}"#)
                .is_err()
        );
    }

    #[test]
    fn server_messages() {
        assert_eq!(
            Protocol::Apollo.data("1", r#"{"data":{}}"#),
            r#"{"type":"data","id":"1","payload":{"data":{}}}"#
        );
        assert_eq!(
            Protocol::GraphqlWs.data("a\"b", "{}"),
            r#"{"type":"next","id":"a\"b","payload":{}}"#
        );
        assert_eq!(complete("1"), r#"{"type":"complete","id":"1"}"#);
        assert_eq!(pong(None), r#"{"type":"pong"}"#);
        assert_eq!(
            pong(Some("x")),
            r#"{"type":"pong","payload":{"message":"x"}}"#
        );
        assert_eq!(
            connection_error("bad"),
            r#"{"type":"connection_error","payload":"bad"}"#
        );
        let e = serde_json::json!({"message": "m"});
        assert_eq!(
            Protocol::Apollo.error("1", std::slice::from_ref(&e)),
            r#"{"type":"error","id":"1","payload":{"errors":[{"message":"m"}]}}"#
        );
        assert_eq!(
            Protocol::GraphqlWs.error("1", &[e]),
            r#"{"type":"error","id":"1","payload":[{"message":"m"}]}"#
        );
    }

    #[test]
    fn close_reasons_fit_a_frame() {
        assert_eq!(
            close_reason_json(4403, "Forbidden"),
            r#"{"server_error_msg":"4403: Forbidden"}"#
        );
        for long in ["é".repeat(200), "\"".repeat(200), "a".repeat(200)] {
            let reason = close_reason_json(4400, &long);
            assert!(reason.len() <= 123, "{reason}");
            let v: serde_json::Value = serde_json::from_str(&reason).unwrap();
            assert!(
                v["server_error_msg"]
                    .as_str()
                    .unwrap()
                    .starts_with("4400: ")
            );
        }
    }
}
