//! Websocket steps: GraphQL over `graphql-ws` (Apollo legacy) and `graphql-transport-ws`.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::compare;
use crate::suite::{WsClose, WsConnect, WsStep};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const SECRET_PLACEHOLDER: &str = "{{HASURA_GRAPHQL_ADMIN_SECRET}}";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How a connection ended.
#[derive(Debug, Clone, PartialEq)]
struct Closed {
    code: Option<u16>,
    reason: Option<String>,
}

struct Conn {
    socket: Socket,
    /// Messages received but not consumed by an expectation yet.
    pending: VecDeque<Value>,
    closed: Option<Closed>,
}

/// The websocket connections of one case.
#[derive(Default)]
pub struct Connections {
    conns: HashMap<String, Conn>,
}

/// Run a websocket step. In record mode a failed expectation is rewritten with what
/// arrived instead of failing.
pub async fn run_step(
    conns: &mut Connections,
    base_url: &str,
    admin_secret: Option<&str>,
    step: &mut WsStep,
    record: bool,
) -> Result<(), String> {
    match step {
        WsStep::Connect(c) => conns.connect(base_url, admin_secret, c).await,
        WsStep::Send(s) => {
            let text = substitute_value(&s.message, admin_secret)?.to_string();
            let conn = conns.get(&s.conn)?;
            conn.socket
                .send(Message::text(text))
                .await
                .map_err(|e| format!("send failed: {e}"))
        }
        WsStep::Expect(x) => {
            let timeout = x.timeout_ms.map_or(DEFAULT_TIMEOUT, Duration::from_millis);
            let id = x.message.get("id").cloned();
            let conn = conns.get(&x.conn)?;
            let Some(actual) = conn.next_for(id.as_ref(), timeout).await else {
                return Err(match &conn.closed {
                    Some(c) => format!(
                        "connection closed ({c:?}) while waiting for {}",
                        compare::clip(&x.message.to_string(), 300)
                    ),
                    None => format!(
                        "no message within {timeout:?}, expected {}",
                        compare::clip(&x.message.to_string(), 300)
                    ),
                });
            };
            match compare::ws_message_diff(&x.message, &actual) {
                None => Ok(()),
                Some(_) if record => {
                    x.message = actual;
                    Ok(())
                }
                Some(d) => Err(format!(
                    "message differs at {d}\n      expected: {}\n      actual:   {}",
                    compare::clip(&x.message.to_string(), 600),
                    compare::clip(&actual.to_string(), 600)
                )),
            }
        }
        WsStep::ExpectSilence(s) => {
            let id = s.id.clone().map(Value::String);
            let conn = conns.get(&s.conn)?;
            match conn
                .next_for_any(id.as_ref(), Duration::from_millis(s.ms))
                .await
            {
                Some(m) => Err(format!(
                    "expected no message for {}ms, got {}",
                    s.ms,
                    compare::clip(&m.to_string(), 600)
                )),
                None => match &conn.closed {
                    // Silence means the connection stays open, too.
                    Some(c) => Err(format!("connection closed ({c:?}) during the silence")),
                    None => Ok(()),
                },
            }
        }
        WsStep::ExpectClose(c) => {
            let timeout = c.timeout_ms.map_or(DEFAULT_TIMEOUT, Duration::from_millis);
            let conn = conns.get(&c.conn)?;
            let closed = conn.wait_closed(timeout).await.ok_or_else(|| {
                format!(
                    "connection still open after {timeout:?}; pending messages: {:?}",
                    conn.pending
                )
            })?;
            let matches = c.code.is_none_or(|code| closed.code == Some(code))
                && c.reason
                    .as_ref()
                    .is_none_or(|r| closed.reason.as_ref() == Some(r));
            if matches {
                Ok(())
            } else if record {
                *c = WsClose {
                    conn: c.conn.clone(),
                    code: closed.code,
                    reason: closed.reason,
                    timeout_ms: c.timeout_ms,
                };
                Ok(())
            } else {
                Err(format!(
                    "closed with {:?} {:?}, expected {:?} {:?}",
                    closed.code, closed.reason, c.code, c.reason
                ))
            }
        }
    }
}

/// Replace the admin-secret placeholder (an error in a suite without a secret).
/// How a failing step is named in reports: `ws_expect(conn)`.
pub fn label(step: &WsStep) -> String {
    let (kind, conn) = match step {
        WsStep::Connect(c) => ("ws_connect", &c.conn),
        WsStep::Send(c) => ("ws_send", &c.conn),
        WsStep::Expect(c) => ("ws_expect", &c.conn),
        WsStep::ExpectSilence(c) => ("ws_expect_silence", &c.conn),
        WsStep::ExpectClose(c) => ("ws_expect_close", &c.conn),
    };
    format!("{kind}({conn})")
}

fn substitute(text: &str, admin_secret: Option<&str>) -> Result<String, String> {
    if !text.contains(SECRET_PLACEHOLDER) {
        return Ok(text.to_owned());
    }
    admin_secret
        .map(|s| text.replace(SECRET_PLACEHOLDER, s))
        .ok_or_else(|| format!("{SECRET_PLACEHOLDER} used in a suite without admin_secret"))
}

/// [`substitute`] in every string of a JSON value; `{jwt: ...}` objects become signed tokens.
fn substitute_value(v: &Value, admin_secret: Option<&str>) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) => Value::String(substitute(s, admin_secret)?),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|v| substitute_value(v, admin_secret))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => match crate::jwt::header_value(v) {
            Some(token) => Value::String(token?),
            None => Value::Object(
                o.iter()
                    .map(|(k, v)| Ok((k.clone(), substitute_value(v, admin_secret)?)))
                    .collect::<Result<_, String>>()?,
            ),
        },
        v => v.clone(),
    })
}

impl Connections {
    fn get(&mut self, name: &str) -> Result<&mut Conn, String> {
        self.conns
            .get_mut(name)
            .ok_or_else(|| format!("no websocket connection named {name:?}"))
    }

    async fn connect(
        &mut self,
        base_url: &str,
        admin_secret: Option<&str>,
        c: &WsConnect,
    ) -> Result<(), String> {
        let url = format!("{}{}", base_url.replacen("http", "ws", 1), c.url);
        let mut req = url.into_client_request().map_err(|e| e.to_string())?;
        let header = |v: &str| HeaderValue::from_str(v).map_err(|e| e.to_string());
        req.headers_mut()
            .insert("sec-websocket-protocol", header(&c.protocol)?);
        for (k, v) in &c.headers {
            let name: tokio_tungstenite::tungstenite::http::HeaderName =
                k.parse().map_err(|e| format!("header {k}: {e}"))?;
            req.headers_mut()
                .insert(name, header(&substitute(v, admin_secret)?)?);
        }
        let (socket, _) = tokio_tungstenite::connect_async(req)
            .await
            .map_err(|e| format!("websocket connect to {} failed: {e}", c.url))?;
        self.conns.insert(
            c.conn.clone(),
            Conn {
                socket,
                pending: VecDeque::new(),
                closed: None,
            },
        );
        Ok(())
    }

    /// Close every connection (end of case).
    pub async fn close_all(&mut self) {
        for (_, mut c) in self.conns.drain() {
            if c.closed.is_none() {
                let _ = c.socket.close(None).await;
            }
        }
    }
}

/// Does `m` belong to the stream of messages with this id (`None`: messages without one)?
fn same_id(m: &Value, id: Option<&Value>) -> bool {
    m.get("id") == id
}

impl Conn {
    /// The next message with this id (or without an id), waiting up to `timeout`.
    async fn next_for(&mut self, id: Option<&Value>, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(i) = self.pending.iter().position(|m| same_id(m, id)) {
                return self.pending.remove(i);
            }
            if !self.receive(deadline).await {
                return None;
            }
        }
    }

    /// Like [`Conn::next_for`], but with no id any message counts.
    async fn next_for_any(&mut self, id: Option<&Value>, timeout: Duration) -> Option<Value> {
        match id {
            Some(_) => self.next_for(id, timeout).await,
            None => {
                let deadline = Instant::now() + timeout;
                loop {
                    if let Some(m) = self.pending.pop_front() {
                        return Some(m);
                    }
                    if !self.receive(deadline).await {
                        return None;
                    }
                }
            }
        }
    }

    async fn wait_closed(&mut self, timeout: Duration) -> Option<Closed> {
        let deadline = Instant::now() + timeout;
        while self.closed.is_none() {
            if !self.receive(deadline).await && self.closed.is_none() {
                return None;
            }
        }
        self.closed.clone()
    }

    /// Read one frame into `pending` (or note the close). False on timeout or once closed.
    async fn receive(&mut self, deadline: Instant) -> bool {
        if self.closed.is_some() {
            return false;
        }
        let frame = match tokio::time::timeout_at(deadline, self.socket.next()).await {
            Err(_) => return false,
            Ok(f) => f,
        };
        match frame {
            Some(Ok(Message::Text(t))) => {
                let v: Value =
                    serde_json::from_str(&t).unwrap_or_else(|_| Value::String(t.to_string()));
                match v.get("type").and_then(Value::as_str) {
                    // Keep-alives are timing, not behaviour.
                    Some("ka" | "pong") => {}
                    Some("ping") => {
                        let _ = self.socket.send(Message::text(r#"{"type":"pong"}"#)).await;
                    }
                    _ => self.pending.push_back(v),
                }
                true
            }
            Some(Ok(Message::Close(frame))) => {
                self.closed = Some(Closed {
                    code: frame.as_ref().map(|f| u16::from(f.code)),
                    reason: frame
                        .map(|f| f.reason.to_string())
                        .filter(|r| !r.is_empty()),
                });
                false
            }
            Some(Ok(_)) => true,
            Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) | None => {
                self.closed.get_or_insert(Closed {
                    code: None,
                    reason: None,
                });
                false
            }
            Some(Err(e)) => {
                self.closed = Some(Closed {
                    code: Some(u16::from(CloseCode::Abnormal)),
                    reason: Some(e.to_string()),
                });
                false
            }
        }
    }
}
