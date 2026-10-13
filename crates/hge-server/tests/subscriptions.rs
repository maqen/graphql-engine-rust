//! GraphQL over WebSockets, end to end: a real server on a local port, real WebSocket
//! clients (both sub-protocols) and a throwaway database on the Postgres server
//! `HASURA_GRAPHQL_DATABASE_URL` points at (see `hge-testdb`; skipped without it).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use hge_server::{Engine, ServerConfig};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/subscriptions");
const JWT_KEY: &str = "subscriptions-secret-that-is-at-least-32-bytes";
const SECRET: &str = "s3cret";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Server {
    engine: Arc<Engine>,
    addr: std::net::SocketAddr,
}

async fn start(database_url: &str, configure: impl FnOnce(&mut ServerConfig)) -> Server {
    let (client, conn) = tokio_postgres::connect(database_url, tokio_postgres::NoTls)
        .await
        .expect("connect to test database");
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(Path::new(FIXTURE).join("schema.sql")).unwrap();
    client.batch_execute(&sql).await.expect("apply schema.sql");

    let mut config = ServerConfig {
        database_url: Some(database_url.to_owned()),
        metadata_dir: Some(Path::new(FIXTURE).join("metadata")),
        admin_secrets: vec![SECRET.to_owned()],
        jwt_secret: Some(json!({"type": "HS256", "key": JWT_KEY}).to_string()),
        ..ServerConfig::default()
    };
    config.websocket.live_queries.refetch_interval = Duration::from_millis(50);
    config.websocket.streaming.refetch_interval = Duration::from_millis(50);
    configure(&mut config);
    let engine = hge_server::build_engine(config)
        .await
        .expect("build engine");
    assert!(engine.state.load().inconsistencies.is_empty());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = hge_server::routes::router(engine.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    Server { engine, addr }
}

impl Server {
    async fn connect(&self, subprotocol: Option<&str>) -> Socket {
        self.connect_to("/v1/graphql", subprotocol).await
    }

    async fn connect_to(&self, path: &str, subprotocol: Option<&str>) -> Socket {
        let mut req = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        if let Some(p) = subprotocol {
            req.headers_mut()
                .insert("Sec-WebSocket-Protocol", p.parse().unwrap());
        }
        let (socket, resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("connect");
        assert_eq!(
            resp.headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok()),
            subprotocol
        );
        socket
    }

    /// `POST /v1/graphql` as admin.
    async fn graphql(&self, query: &str) -> Value {
        use tower::ServiceExt;
        let req = axum::http::Request::post("/v1/graphql")
            .header("x-hasura-admin-secret", SECRET)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({ "query": query }).to_string(),
            ))
            .unwrap();
        let resp = hge_server::routes::router(self.engine.clone())
            .oneshot(req)
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }
}

async fn send(socket: &mut Socket, msg: Value) {
    socket.send(Message::text(msg.to_string())).await.unwrap();
}

/// The next text message, skipping keep-alives.
async fn recv(socket: &mut Socket) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a message in time")
            .expect("open socket")
            .expect("valid frame");
        if let Message::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).unwrap();
            let ty = v["type"].as_str().unwrap_or_default();
            let keep_alive = ty == "ka" || (ty == "ping" && v["payload"]["message"] == "keepalive");
            if !keep_alive {
                return v;
            }
        }
    }
}

/// The close frame (code, reason), skipping other messages.
async fn closed(socket: &mut Socket) -> (u16, String) {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a close in time");
        match msg {
            Some(Ok(Message::Close(Some(frame)))) => {
                return (frame.code.into(), frame.reason.to_string());
            }
            Some(Ok(Message::Close(None))) => return (1005, String::new()),
            Some(Ok(_)) => continue,
            None | Some(Err(_)) => return (1006, String::new()),
        }
    }
}

/// No message other than keep-alives for a few poll intervals.
async fn quiet(socket: &mut Socket) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return,
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                let ty = v["type"].as_str().unwrap_or_default();
                assert!(
                    ty == "ka" || (ty == "ping" && v["payload"]["message"] == "keepalive"),
                    "unexpected message {v}"
                );
            }
            Ok(other) => panic!("unexpected frame {other:?}"),
        }
    }
}

async fn init(socket: &mut Socket, headers: Value) {
    send(
        socket,
        json!({"type": "connection_init", "payload": {"headers": headers}}),
    )
    .await;
    assert_eq!(recv(socket).await, json!({"type": "connection_ack"}));
}

fn admin() -> Value {
    json!({ "x-hasura-admin-secret": SECRET })
}

fn user(id: u32) -> Value {
    json!({ "x-hasura-admin-secret": SECRET, "x-hasura-role": "user", "x-hasura-user-id": id.to_string() })
}

fn jwt(claims: Value) -> String {
    let b64 = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    let input = format!(
        "{}.{}",
        b64(&json!({"alg": "HS256", "typ": "JWT"})),
        b64(&claims)
    );
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, JWT_KEY.as_bytes());
    let sig = ring::hmac::sign(&key, input.as_bytes());
    format!(
        "{input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.as_ref())
    )
}

fn data(id: &str, ty: &str, data: Value) -> Value {
    json!({"type": ty, "id": id, "payload": {"data": data}})
}

#[tokio::test]
async fn websockets() {
    let Some(db) = hge_testdb::TestDb::create("subscriptions").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping");
        return;
    };
    let url = db.url().to_owned();
    let server = start(&url, |_| {}).await;
    // Before the scenarios that change the rows.
    native_query_live_query(&server).await;
    aggregate_live_query(&server).await;
    apollo_live_queries(&server).await;
    graphql_ws_protocol(&server).await;
    cohorts_and_permissions(&server).await;
    streaming(&server).await;
    restrictions(&server).await;
    function_live_query(&server).await;
    auth_failures(&server).await;
    server_close(&server).await;
    jwt_expiry(&server).await;
    async_actions(&server, &url).await;
    async_actions_with_relationships(&server, &url).await;

    let server = start(&url, |c| {
        c.websocket.init_timeout = Duration::from_millis(200)
    })
    .await;
    init_timeout(&server).await;

    let server = start(&url, |c| c.websocket.queue_size = 0).await;
    overflow(&server).await;
}

/// `T_aggregate` is a subscription root field too, multiplexed like the others.
async fn aggregate_live_query(server: &Server) {
    let mut ws = server.connect(None).await;
    init(&mut ws, admin()).await;
    let sub = "subscription { message_aggregate(where: {priority: {_eq: 2}}) { aggregate { count max { id } } } }";
    send(
        &mut ws,
        json!({"id": "1", "type": "start", "payload": {"query": sub}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        data(
            "1",
            "data",
            json!({"message_aggregate": {"aggregate": {"count": 2, "max": {"id": 3}}}})
        )
    );
    send(&mut ws, json!({"id": "1", "type": "stop"})).await;
}

async fn apollo_live_queries(server: &Server) {
    let mut ws = server.connect(None).await;
    init(&mut ws, admin()).await;
    let sub =
        "subscription { message(order_by: {id: asc}, where: {priority: {_eq: 2}}) { id body } }";
    send(
        &mut ws,
        json!({"id": "1", "type": "start", "payload": {"query": sub}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        data(
            "1",
            "data",
            json!({"message": [{"id": 2, "body": "two"}, {"id": 3, "body": "three"}]})
        )
    );
    quiet(&mut ws).await;

    // A change that does not affect the result sends nothing.
    server
        .graphql("mutation { insert_message_one(object: {author_id: 3, body: \"low\", priority: 1}) { id } }")
        .await;
    quiet(&mut ws).await;

    // Mutations over the socket: data then complete.
    let insert = "mutation { insert_message_one(object: {author_id: 3, body: \"four\", priority: 2}) { id } }";
    send(
        &mut ws,
        json!({"id": "2", "type": "start", "payload": {"query": insert}}),
    )
    .await;
    let mut got = vec![
        recv(&mut ws).await,
        recv(&mut ws).await,
        recv(&mut ws).await,
    ];
    got.sort_by_key(|m| m["id"].as_str().unwrap().to_owned());
    assert_eq!(
        got,
        vec![
            data(
                "1",
                "data",
                json!({"message": [{"id": 2, "body": "two"}, {"id": 3, "body": "three"}, {"id": 5, "body": "four"}]})
            ),
            data("2", "data", json!({"insert_message_one": {"id": 5}})),
            json!({"type": "complete", "id": "2"}),
        ]
    );

    // Queries over the socket, with aliases.
    send(
        &mut ws,
        json!({"id": "3", "type": "start", "payload": {"query": "query Q($id: Int!) { m: message_by_pk(id: $id) { body } }", "variables": {"id": 1}, "operationName": "Q"}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        data("3", "data", json!({"m": {"body": "one"}}))
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "3"}));

    // A request error: `error` with `{"errors": [...]}`, then complete.
    send(
        &mut ws,
        json!({"id": "4", "type": "start", "payload": {"query": "subscription { nope }"}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        json!({"type": "error", "id": "4", "payload": {"errors": [{"message": "field 'nope' not found in type: 'subscription_root'", "extensions": {"path": "$.selectionSet.nope", "code": "validation-failed"}}]}})
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "4"}));

    // A malformed message: connection_error, the socket stays open.
    send(&mut ws, json!({"type": "stop"})).await;
    let err = recv(&mut ws).await;
    assert_eq!(err["type"], "connection_error");
    assert!(
        err["payload"]
            .as_str()
            .unwrap()
            .contains("key \"id\" not found")
    );

    // A second start with a running id.
    send(
        &mut ws,
        json!({"id": "1", "type": "start", "payload": {"query": sub}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        json!({"type": "error", "id": "1", "payload": {"message": "an operation already exists with this id: 1", "extensions": {"path": "$", "code": "start-failed"}}})
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "1"}));

    // Stop: nothing more arrives for the subscription.
    send(&mut ws, json!({"id": "1", "type": "stop"})).await;
    server
        .graphql("mutation { insert_message_one(object: {author_id: 3, body: \"five\", priority: 2}) { id } }")
        .await;
    quiet(&mut ws).await;
    assert_eq!(server.engine.ws.live_queries.poller_count(), 0);

    send(&mut ws, json!({"type": "connection_terminate"})).await;
    assert_eq!(
        closed(&mut ws).await,
        (1000, "GQL_CONNECTION_TERMINATE received".to_owned())
    );
}

async fn graphql_ws_protocol(server: &Server) {
    let mut ws = server.connect(Some("graphql-transport-ws")).await;
    send(
        &mut ws,
        json!({"type": "ping", "payload": {"message": "hi"}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        json!({"type": "pong", "payload": {"message": "hi"}})
    );
    init(&mut ws, admin()).await;
    let sub = "subscription { message_by_pk(id: 1) { id body } }";
    send(
        &mut ws,
        json!({"id": "a", "type": "subscribe", "payload": {"query": sub}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        data(
            "a",
            "next",
            json!({"message_by_pk": {"id": 1, "body": "one"}})
        )
    );
    server
        .graphql(
            "mutation { update_message_by_pk(pk_columns: {id: 1}, _set: {body: \"uno\"}) { id } }",
        )
        .await;
    assert_eq!(
        recv(&mut ws).await,
        data(
            "a",
            "next",
            json!({"message_by_pk": {"id": 1, "body": "uno"}})
        )
    );

    // Request errors are a bare list.
    send(&mut ws, json!({"id": "b", "type": "subscribe", "payload": {"query": "subscription { a: message_by_pk(id: 1) { id } b: message_by_pk(id: 2) { id } }"}})).await;
    assert_eq!(
        recv(&mut ws).await,
        json!({"type": "error", "id": "b", "payload": [{"message": "subscriptions must select one top level field", "extensions": {"path": "$", "code": "validation-failed"}}]})
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "b"}));

    // Execution errors of queries are `error` messages too.
    send(&mut ws, json!({"id": "c", "type": "subscribe", "payload": {"query": "mutation { insert_message_one(object: {author_id: 1, body: \"x\", id: 1}) { id } }"}})).await;
    let err = recv(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(
        err["payload"][0]["extensions"]["code"],
        "constraint-violation"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "c"}));

    send(&mut ws, json!({"id": "a", "type": "complete"})).await;
    quiet(&mut ws).await;

    // An invalid message closes the socket with 4400.
    send(&mut ws, json!({"type": "what"})).await;
    let (code, reason) = closed(&mut ws).await;
    assert_eq!(code, 4400);
    assert!(reason.starts_with(r#"{"server_error_msg":"4400: Parsing client message failed"#));
}

async fn cohorts_and_permissions(server: &Server) {
    let sub = "subscription { message(order_by: {id: asc}) { id body } }";
    let mut one = server.connect(Some("graphql-ws")).await;
    init(&mut one, user(1)).await;
    let mut two = server.connect(Some("graphql-ws")).await;
    init(&mut two, user(2)).await;
    let mut one_again = server.connect(Some("graphql-ws")).await;
    init(&mut one_again, user(1)).await;
    for ws in [&mut one, &mut two, &mut one_again] {
        send(
            ws,
            json!({"id": "s", "type": "start", "payload": {"query": sub}}),
        )
        .await;
    }
    assert_eq!(
        recv(&mut one).await,
        data(
            "s",
            "data",
            json!({"message": [{"id": 1, "body": "uno"}, {"id": 3, "body": "three"}]})
        )
    );
    assert_eq!(
        recv(&mut two).await,
        data("s", "data", json!({"message": [{"id": 2, "body": "two"}]}))
    );
    assert_eq!(
        recv(&mut one_again).await,
        data(
            "s",
            "data",
            json!({"message": [{"id": 1, "body": "uno"}, {"id": 3, "body": "three"}]})
        )
    );
    // One statement for everyone; one cohort per user.
    assert_eq!(server.engine.ws.live_queries.poller_count(), 1);
    assert_eq!(server.engine.ws.live_queries.cohort_count(), 2);

    // User 2 inserts (author_id comes from the session): only user 2's cohort changes.
    let mut writer = server.connect(Some("graphql-ws")).await;
    init(&mut writer, user(2)).await;
    send(&mut writer, json!({"id": "w", "type": "start", "payload": {"query": "mutation { insert_message_one(object: {body: \"mine\"}) { id } }"}})).await;
    let inserted = recv(&mut writer).await;
    let new_id = inserted["payload"]["data"]["insert_message_one"]["id"].clone();
    assert_eq!(
        recv(&mut two).await,
        data(
            "s",
            "data",
            json!({"message": [{"id": 2, "body": "two"}, {"id": new_id, "body": "mine"}]})
        )
    );
    quiet(&mut one).await;

    // The `reader` role has no streaming field (subscription_root_fields: [select]).
    let mut reader = server.connect(None).await;
    init(
        &mut reader,
        json!({"x-hasura-admin-secret": SECRET, "x-hasura-role": "reader"}),
    )
    .await;
    send(&mut reader, json!({"id": "r", "type": "start", "payload": {"query": "subscription { message_by_pk(id: 1) { id } }"}})).await;
    let err = recv(&mut reader).await;
    assert_eq!(
        err["payload"]["errors"][0]["message"],
        "field 'message_by_pk' not found in type: 'subscription_root'"
    );
}

/// Tracked functions are subscription root fields; one statement serves both users, and each
/// cohort passes its own session to the function's session argument.
async fn function_live_query(server: &Server) {
    let sub = "subscription { my_messages(args: {min_priority: 2}, order_by: {id: asc}) { id } }";
    let mut one = server.connect(None).await;
    init(&mut one, user(1)).await;
    let mut two = server.connect(None).await;
    init(&mut two, user(2)).await;
    for ws in [&mut one, &mut two] {
        send(
            ws,
            json!({"id": "f", "type": "start", "payload": {"query": sub}}),
        )
        .await;
    }
    assert_eq!(
        recv(&mut one).await,
        data("f", "data", json!({"my_messages": [{"id": 3}]}))
    );
    assert_eq!(
        recv(&mut two).await,
        data("f", "data", json!({"my_messages": [{"id": 2}]}))
    );
    for ws in [&mut one, &mut two] {
        send(ws, json!({"id": "f", "type": "stop"})).await;
    }
}

/// Native query arguments are parameters, so each cohort sees its own rows.
async fn native_query_live_query(server: &Server) {
    let mut one = server.connect(None).await;
    init(&mut one, admin()).await;
    let mut two = server.connect(None).await;
    init(&mut two, admin()).await;
    for (ws, priority) in [(&mut one, 1), (&mut two, 2)] {
        let sub = format!(
            "subscription {{ messages_at(args: {{priority: {priority}}}, order_by: {{id: asc}}) {{ id body }} }}"
        );
        send(
            ws,
            json!({"id": "n", "type": "start", "payload": {"query": sub}}),
        )
        .await;
    }
    assert_eq!(
        recv(&mut one).await,
        data(
            "n",
            "data",
            json!({"messages_at": [{"id": 1, "body": "one"}]})
        )
    );
    assert_eq!(
        recv(&mut two).await,
        data(
            "n",
            "data",
            json!({"messages_at": [{"id": 2, "body": "two"}, {"id": 3, "body": "three"}]})
        )
    );
    for ws in [&mut one, &mut two] {
        send(ws, json!({"id": "n", "type": "stop"})).await;
    }
}

async fn streaming(server: &Server) {
    let mut ws = server.connect(Some("graphql-transport-ws")).await;
    init(&mut ws, admin()).await;
    let sub =
        "subscription { message_feed(batch_size: 2, cursor: {initial_value: {id: 0}}) { id } }";
    send(
        &mut ws,
        json!({"id": "f", "type": "subscribe", "payload": {"query": sub}}),
    )
    .await;
    let mut seen = vec![];
    loop {
        let msg = recv(&mut ws).await;
        let batch = msg["payload"]["data"]["message_feed"]
            .as_array()
            .unwrap()
            .clone();
        assert!(!batch.is_empty() && batch.len() <= 2, "{msg}");
        seen.extend(batch.iter().map(|r| r["id"].as_i64().unwrap()));
        let max = server
            .graphql("{ message(order_by: {id: desc}, limit: 1) { id } }")
            .await;
        let max = max["data"]["message"][0]["id"].as_i64();
        if max.is_some_and(|m| seen.last() == Some(&m)) || seen.len() > 20 {
            break;
        }
    }
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted);
    quiet(&mut ws).await;

    // New rows arrive past the cursor.
    let inserted = server
        .graphql("mutation { insert_message(objects: [{author_id: 9, body: \"s1\"}, {author_id: 9, body: \"s2\"}]) { returning { id } } }")
        .await;
    let ids: Vec<Value> = inserted["data"]["insert_message"]["returning"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| json!({"id": r["id"]}))
        .collect();
    assert_eq!(
        recv(&mut ws).await,
        data("f", "next", json!({"message_feed": ids}))
    );
    quiet(&mut ws).await;

    // Descending from a cursor, with a filter.
    let sub = "subscription { message_feed(batch_size: 10, cursor: {initial_value: {id: 3}, ordering: DESC}, where: {priority: {_gte: 1}}) { id } }";
    send(
        &mut ws,
        json!({"id": "g", "type": "subscribe", "payload": {"query": sub}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        data("g", "next", json!({"message_feed": [{"id": 2}, {"id": 1}]}))
    );

    // Cursor validation.
    let bad = "subscription { message_feed(batch_size: 1, cursor: [{initial_value: {id: 1}}, {initial_value: {id: 2}}]) { id } }";
    send(
        &mut ws,
        json!({"id": "h", "type": "subscribe", "payload": {"query": bad}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await["payload"][0]["message"],
        "multiple column cursors are not supported yet"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "h"}));
}

async fn restrictions(server: &Server) {
    // Subscriptions over HTTP are refused.
    let resp = server.graphql("subscription { message { id } }").await;
    assert_eq!(
        resp,
        json!({"errors": [{"message": "subscriptions are not supported over HTTP, use websockets instead", "extensions": {"path": "$", "code": "unexpected-payload"}}]})
    );

    let mut ws = server.connect(None).await;
    // Starting before connection_init.
    send(&mut ws, json!({"id": "1", "type": "start", "payload": {"query": "subscription { message { id } }"}})).await;
    assert_eq!(
        recv(&mut ws).await["payload"]["message"],
        "start received before the connection is initialised"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "1"}));
    init(&mut ws, admin()).await;
    send(
        &mut ws,
        json!({"id": "2", "type": "start", "payload": {"query": "subscription { __typename }"}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await["payload"]["errors"][0]["message"],
        "Introspection at root field level is not supported over subscriptions"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "2"}));
    // Values the database rejects are reported when the subscription starts.
    send(&mut ws, json!({"id": "3", "type": "start", "payload": {"query": "subscription($b: String!) { message(where: {body: {_regex: $b}}) { id } }", "variables": {"b": "("}}})).await;
    let err = recv(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(
        err["payload"]["errors"][0]["extensions"]["code"],
        "data-exception"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "3"}));
    // Variables become cohort values: the same subscription with a valid pattern works.
    send(&mut ws, json!({"id": "4", "type": "start", "payload": {"query": "subscription($b: String!) { message(where: {body: {_regex: $b}}) { id } }", "variables": {"b": "^tw"}}})).await;
    assert_eq!(
        recv(&mut ws).await,
        data("4", "data", json!({"message": [{"id": 2}]}))
    );

    // One root field, unless the operation has V2's `@_multiple_top_level_fields`.
    let two = "{ a: message(where: {id: {_eq: 1}}) { id } b: message_by_pk(id: 2) { id } }";
    let start =
        |id: &str, query: String| json!({"id": id, "type": "start", "payload": {"query": query}});
    send(&mut ws, start("5", format!("subscription {two}"))).await;
    assert_eq!(
        recv(&mut ws).await["payload"]["errors"][0]["message"],
        "subscriptions must select one top level field"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "5"}));
    send(
        &mut ws,
        start(
            "6",
            format!("subscription @_multiple_top_level_fields {two}"),
        ),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        data("6", "data", json!({"a": [{"id": 1}], "b": {"id": 2}}))
    );
    let mixed = "subscription @_multiple_top_level_fields { message { id } message_feed(batch_size: 1, cursor: {initial_value: {id: 0}}) { id } }";
    send(&mut ws, start("7", mixed.to_owned())).await;
    assert_eq!(
        recv(&mut ws).await["payload"]["errors"][0]["message"],
        "streaming and livequery subscriptions cannot be executed in the same subscription"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "7"}));
    send(
        &mut ws,
        start(
            "8",
            "query @_multiple_top_level_fields { message { id } }".to_owned(),
        ),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await["payload"]["errors"][0]["message"],
        "directive '_multiple_top_level_fields' is not allowed on a query"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "8"}));

    // Aggregates are live queries too.
    send(
        &mut ws,
        start(
            "9",
            "subscription { message_aggregate { aggregate { count } } }".to_owned(),
        ),
    )
    .await;
    let msg = recv(&mut ws).await;
    assert!(
        msg["payload"]["data"]["message_aggregate"]["aggregate"]["count"].is_u64(),
        "{msg}"
    );

    // Upgrades are only served on the GraphQL endpoints.
    for path in ["/v2/graphql", "/healthz", "/v1/metadata"] {
        let req = format!("ws://{}{path}", server.addr)
            .into_client_request()
            .unwrap();
        match tokio_tungstenite::connect_async(req).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), 404, "{path}");
                let body: Value = serde_json::from_slice(resp.body().as_deref().unwrap()).unwrap();
                assert_eq!(body["extensions"]["code"], "not-found");
            }
            other => panic!("expected a 404 on {path}, got {other:?}"),
        }
    }
}

/// Subscriptions to an async action's result: sent when it changes, then `complete`.
async fn async_actions(server: &Server, url: &str) {
    let id = server.graphql("mutation { asyncTask(n: 1) }").await["data"]["asyncTask"]
        .as_str()
        .expect("action id")
        .to_owned();
    let mut ws = server.connect(Some("graphql-transport-ws")).await;
    init(&mut ws, admin()).await;
    let sub = format!(
        r#"subscription {{ task: asyncTask(id: "{id}") {{ id output {{ label }} errors }} }}"#
    );
    send(
        &mut ws,
        json!({"id": "1", "type": "subscribe", "payload": {"query": sub}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        data(
            "1",
            "next",
            json!({"task": {"id": id, "output": null, "errors": null}})
        )
    );
    // Nothing is sent while the result is unchanged (it is read every second).
    tokio::time::sleep(Duration::from_millis(1200)).await;
    quiet(&mut ws).await;
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    client
        .execute(
            r#"UPDATE hdb_catalog.hdb_action_log
               SET response_payload = '{"label": "done"}', status = 'completed'
               WHERE id = $1::text::uuid"#,
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(
        recv(&mut ws).await,
        data(
            "1",
            "next",
            json!({"task": {"id": id, "output": {"label": "done"}, "errors": null}})
        )
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "1"}));
    // The id is free again.
    send(
        &mut ws,
        json!({"id": "1", "type": "subscribe", "payload": {"query": sub}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await["payload"]["data"]["task"]["output"]["label"],
        "done"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "1"}));

    // Several actions complete once the last one has finished; errors finish one too.
    let second = server.graphql("mutation { asyncTask(n: 2) }").await["data"]["asyncTask"]
        .as_str()
        .unwrap()
        .to_owned();
    let both = format!(
        r#"subscription @_multiple_top_level_fields {{ a: asyncTask(id: "{id}") {{ output {{ label }} }} b: asyncTask(id: "{second}") {{ errors }} }}"#
    );
    send(
        &mut ws,
        json!({"id": "3", "type": "subscribe", "payload": {"query": both}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        data(
            "3",
            "next",
            json!({"a": {"output": {"label": "done"}}, "b": {"errors": null}})
        )
    );
    client
        .execute(
            r#"UPDATE hdb_catalog.hdb_action_log
               SET errors = '{"path": "$", "error": "failed", "code": "unexpected"}', status = 'error'
               WHERE id = $1::text::uuid"#,
            &[&second],
        )
        .await
        .unwrap();
    assert_eq!(
        recv(&mut ws).await,
        data(
            "3",
            "next",
            json!({"a": {"output": {"label": "done"}}, "b": {"errors": {"code": "unexpected", "error": "failed", "path": "$"}}})
        )
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "3"}));

    // Async action results cannot be mixed with database fields.
    let mixed = format!(
        r#"subscription @_multiple_top_level_fields {{ asyncTask(id: "{id}") {{ id }} message {{ id }} }}"#
    );
    send(
        &mut ws,
        json!({"id": "2", "type": "subscribe", "payload": {"query": mixed}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await["payload"][0]["message"],
        "async action queries with no relationships aren't expected to mix with normal source database queries"
    );
}

/// An output type with relationships makes the result a live query: it never completes,
/// follows the relationships, and only the session that created the action sees it.
async fn async_actions_with_relationships(server: &Server, url: &str) {
    let id = server.graphql("mutation { asyncLink(n: 1) }").await["data"]["asyncLink"]
        .as_str()
        .expect("action id")
        .to_owned();
    let sub = format!(
        r#"subscription {{ link: asyncLink(id: "{id}") {{ output {{ messageId message {{ body }} }} }} }}"#
    );
    let mut ws = server.connect(None).await;
    init(&mut ws, admin()).await;
    send(
        &mut ws,
        json!({"id": "1", "type": "start", "payload": {"query": sub}}),
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        data("1", "data", json!({"link": {"output": null}}))
    );
    // Another session does not see the admin's action.
    let mut other = server.connect(None).await;
    init(&mut other, user(1)).await;
    send(
        &mut other,
        json!({"id": "1", "type": "start", "payload": {"query": sub}}),
    )
    .await;
    assert_eq!(
        recv(&mut other).await,
        data("1", "data", json!({"link": null}))
    );

    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    client
        .execute(
            r#"UPDATE hdb_catalog.hdb_action_log
               SET response_payload = '{"messageId": 2}', status = 'completed'
               WHERE id = $1::text::uuid"#,
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(
        recv(&mut ws).await,
        data(
            "1",
            "data",
            json!({"link": {"output": {"messageId": 2, "message": {"body": "two"}}}})
        )
    );
    // Finished, but still running: no `complete`.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    quiet(&mut ws).await;
    // After `stop` nothing more is sent.
    send(&mut ws, json!({"id": "1", "type": "stop"})).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    quiet(&mut ws).await;
    quiet(&mut other).await;

    // Actions with and without relationships, or with relationships and database fields,
    // do not mix.
    let task = server.graphql("mutation { asyncTask(n: 3) }").await["data"]["asyncTask"]
        .as_str()
        .unwrap()
        .to_owned();
    for (op, message) in [
        (
            format!(r#"a: asyncLink(id: "{id}") {{ id }} b: asyncTask(id: "{task}") {{ id }}"#),
            "async action queries with no relationships aren't expected to mix with normal source database queries",
        ),
        (
            format!(r#"a: asyncLink(id: "{id}") {{ id }} message {{ id }}"#),
            "async action queries with relationships cannot be mixed with database queries in one subscription",
        ),
    ] {
        let query = format!("subscription @_multiple_top_level_fields {{ {op} }}");
        send(
            &mut ws,
            json!({"id": "3", "type": "start", "payload": {"query": query}}),
        )
        .await;
        let err = recv(&mut ws).await;
        assert_eq!(err["type"], "error");
        assert_eq!(err["payload"]["errors"][0]["message"], message);
        assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "3"}));
    }

    // An unknown id is an error, without `complete`.
    let missing =
        r#"subscription { asyncTask(id: "00000000-0000-0000-0000-000000000000") { id } }"#;
    send(
        &mut ws,
        json!({"id": "2", "type": "start", "payload": {"query": missing}}),
    )
    .await;
    let err = recv(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(
        err["payload"]["errors"][0]["message"],
        "database query error"
    );
    quiet(&mut ws).await;
}

/// With no room in the send queue, every result overflows.
async fn overflow(server: &Server) {
    let mut ws = server.connect(None).await;
    init(&mut ws, admin()).await;
    let sub =
        "subscription { message_feed(batch_size: 1, cursor: {initial_value: {id: 0}}) { id } }";
    send(
        &mut ws,
        json!({"id": "1", "type": "start", "payload": {"query": sub}}),
    )
    .await;
    let err = recv(&mut ws).await;
    assert_eq!(
        err["payload"]["errors"][0],
        json!({
            "message": "This operation's result could not be delivered because the connection's send queue was at capacity, so it has been stopped. Please retry, using this cursor as the initial_value to resume from exactly where results were lost: {\"id\":\"0\"}.",
            "extensions": {"path": "$", "code": "unexpected", "resume_cursor": {"id": "0"}}
        })
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "1"}));
    send(&mut ws, json!({"id": "2", "type": "start", "payload": {"query": "subscription { message_by_pk(id: 1) { id } }"}})).await;
    assert_eq!(
        recv(&mut ws).await["payload"]["errors"][0]["extensions"],
        json!({"path": "$", "code": "unexpected"})
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "2"}));
    quiet(&mut ws).await;
}

async fn auth_failures(server: &Server) {
    let mut ws = server.connect(None).await;
    send(&mut ws, json!({"type": "connection_init", "payload": {}})).await;
    assert_eq!(
        recv(&mut ws).await,
        json!({"type": "connection_error", "payload": "Missing 'Authorization' or 'Cookie' header in JWT authentication mode"})
    );
    let (code, reason) = closed(&mut ws).await;
    assert_eq!(code, 4400);
    let reason: Value = serde_json::from_str(&reason).unwrap();
    assert!(
        reason["server_error_msg"]
            .as_str()
            .unwrap()
            .starts_with("4400: Connection initialization failed: Missing 'Authorization'")
    );

    let mut ws = server.connect(Some("graphql-transport-ws")).await;
    send(&mut ws, json!({"type": "connection_init", "payload": {"headers": {"x-hasura-admin-secret": "wrong"}}})).await;
    assert_eq!(
        closed(&mut ws).await,
        (4403, r#"{"server_error_msg":"4403: Forbidden"}"#.to_owned())
    );
}

async fn server_close(server: &Server) {
    let mut ws = server.connect(None).await;
    init(&mut ws, admin()).await;
    send(&mut ws, json!({"id": "1", "type": "start", "payload": {"query": "subscription { message_by_pk(id: 2) { id } }"}})).await;
    recv(&mut ws).await;
    server
        .engine
        .ws
        .close_all("Server state changed, restarting the server");
    assert_eq!(
        closed(&mut ws).await,
        (
            1012,
            "Server state changed, restarting the server".to_owned()
        )
    );
    drop(ws);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while server.engine.ws.live_queries.poller_count() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "poller still running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn jwt_expiry(server: &Server) {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 2;
    let token = jwt(json!({
        "sub": "1",
        "exp": exp,
        "https://hasura.io/jwt/claims": {
            "x-hasura-allowed-roles": ["user"],
            "x-hasura-default-role": "user",
            "x-hasura-user-id": "1",
        }
    }));
    let mut ws = server.connect(Some("graphql-transport-ws")).await;
    init(&mut ws, json!({"Authorization": format!("Bearer {token}")})).await;
    send(&mut ws, json!({"id": "1", "type": "subscribe", "payload": {"query": "subscription { message(where: {id: {_eq: 2}}) { id } }"}})).await;
    // User 1 cannot see message 2.
    assert_eq!(
        recv(&mut ws).await,
        data("1", "next", json!({"message": []}))
    );
    // At expiry the connection is dropped without a close frame, as V2 does.
    assert_eq!(closed(&mut ws).await.0, 1006);
}

async fn init_timeout(server: &Server) {
    let mut ws = server.connect(Some("graphql-transport-ws")).await;
    assert_eq!(
        closed(&mut ws).await,
        (4408, "Connection initialisation timed out".to_owned())
    );
    let mut ws = server.connect(None).await;
    assert_eq!(
        closed(&mut ws).await,
        (1011, "Connection initialisation timed out".to_owned())
    );
}
