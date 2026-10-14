//! `websocket-log` and `livequery-poller-log`: V2's shapes (spec 09 §11). The subscription
//! part needs the Postgres server `HASURA_GRAPHQL_DATABASE_URL` points at (skipped without
//! it).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hge_server::ServerConfig;
use hge_server::telemetry::{self, Level, LogConfig, LogType, LogTypes};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/subscriptions");
const SECRET: &str = "s3cret";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Lines = Arc<Mutex<Vec<String>>>;

async fn serve(config: ServerConfig) -> std::net::SocketAddr {
    let engine = hge_server::build_engine(config).await.expect("engine");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = hge_server::routes::router(engine);
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

async fn connect(addr: std::net::SocketAddr, headers: &[(&'static str, &str)]) -> Socket {
    let mut req = format!("ws://{addr}/v1/graphql")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("Sec-WebSocket-Protocol", "graphql-ws".parse().unwrap());
    for (k, v) in headers {
        req.headers_mut().insert(
            *k,
            v.parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
                .unwrap(),
        );
    }
    tokio_tungstenite::connect_async(req)
        .await
        .expect("connect")
        .0
}

async fn send(socket: &mut Socket, msg: Value) {
    socket.send(Message::text(msg.to_string())).await.unwrap();
}

/// The next message of `type`, skipping others.
async fn recv(socket: &mut Socket, ty: &str) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a message in time")
            .expect("open socket")
            .expect("valid frame");
        if let Message::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == ty {
                return v;
            }
        }
    }
}

/// Wait until `count` logs of `ty` were written, then take every log of that type.
async fn logs(lines: &Lines, ty: &str, count: usize) -> Vec<Value> {
    for _ in 0..200 {
        let n = lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains(&format!(r#""type":"{ty}""#)))
            .count();
        if n >= count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut lines = lines.lock().unwrap();
    let (taken, rest): (Vec<String>, Vec<String>) = lines
        .drain(..)
        .partition(|l| l.contains(&format!(r#""type":"{ty}""#)));
    *lines = rest;
    taken
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|l| l["type"] == ty)
        .collect()
}

fn event_types(logs: &[Value]) -> Vec<String> {
    logs.iter()
        .map(|l| {
            let event = &l["detail"]["event"];
            match event["type"].as_str().unwrap() {
                "operation" => {
                    let op = event["detail"]["operation_type"]["type"].as_str().unwrap();
                    format!("operation:{op}")
                }
                other => other.to_owned(),
            }
        })
        .collect()
}

#[tokio::test]
async fn websocket_log() {
    let mut types = LogTypes::DEFAULT;
    types.insert(LogType::QueryLog);
    types.insert(LogType::LivequeryPollerLog);
    let lines = telemetry::init_capture(LogConfig {
        level: Level::Info,
        types,
    });

    let addr = serve(ServerConfig {
        admin_secrets: vec![SECRET.into()],
        ..ServerConfig::default()
    })
    .await;

    // A rejected connection_init.
    let mut socket = connect(addr, &[]).await;
    send(&mut socket, json!({"type": "connection_init", "payload": {"headers": {"x-hasura-admin-secret": "wrong"}}})).await;
    recv(&mut socket, "connection_error").await;
    drop(socket);
    let got = logs(&lines, "websocket-log", 3).await;
    assert_eq!(
        event_types(&got),
        ["accepted", "conn_err", "closed"],
        "{got:#?}"
    );
    let ws_id = &got[0]["detail"]["connection_info"]["websocket_id"];
    assert_eq!(ws_id.as_str().unwrap().len(), 36);
    assert!(
        got.iter()
            .all(|l| l["detail"]["connection_info"]["websocket_id"] == *ws_id)
    );
    assert_eq!(got[0]["level"], "info");
    assert_eq!(
        got[0]["detail"],
        json!({"user_vars": null, "connection_info": {"websocket_id": ws_id, "token_expiry": null, "msg": null}, "event": {"type": "accepted"}})
    );
    assert_eq!(got[1]["level"], "error");
    assert_eq!(
        got[1]["detail"]["event"],
        json!({"type": "conn_err", "detail": "invalid \"x-hasura-admin-secret\""})
    );

    // Queries: one that succeeds, one that fails, one that cannot start.
    let mut socket = connect(addr, &[("x-request-id", "ignored")]).await;
    send(
        &mut socket,
        json!({"type": "start", "id": "0", "payload": {"query": "{ __typename }"}}),
    )
    .await;
    recv(&mut socket, "error").await;
    send(&mut socket, json!({"type": "connection_init", "payload": {"headers": {"x-hasura-admin-secret": SECRET, "x-request-id": "rid-7"}}})).await;
    recv(&mut socket, "connection_ack").await;
    send(&mut socket, json!({"type": "start", "id": "1", "payload": {"query": "query Q($a: Boolean!) { __typename @include(if: $a) }", "operationName": "Q", "variables": {"a": true}}})).await;
    recv(&mut socket, "complete").await;
    send(
        &mut socket,
        json!({"type": "start", "id": "2", "payload": {"query": "{ nope }"}}),
    )
    .await;
    recv(&mut socket, "complete").await;
    send(&mut socket, json!({"type": "nonsense"})).await;
    recv(&mut socket, "connection_error").await;
    socket.close(None).await.unwrap();
    let got = logs(&lines, "websocket-log", 8).await;
    assert_eq!(
        event_types(&got),
        [
            "accepted",
            "operation:proto_err",
            "operation:completed",
            "operation:completed",
            "operation:query_err",
            "operation:completed",
            "conn_err",
            "closed",
        ],
        "{got:#?}"
    );
    // Before connection_init: no session, no request id.
    assert_eq!(got[1]["level"], "error");
    assert_eq!(got[1]["detail"]["user_vars"], Value::Null);
    assert_eq!(
        got[1]["detail"]["event"]["detail"],
        json!({
            "operation_id": "0",
            "request_id": null,
            "operation_name": null,
            "operation_type": {"type": "proto_err", "detail": "start received before the connection is initialised"},
            "query": {"query": "{ __typename }"},
            "parameterized_query_hash": null,
        })
    );
    // After: the session and the connection's request id; variables are never logged.
    assert_eq!(got[3]["level"], "info");
    assert_eq!(
        got[3]["detail"]["user_vars"],
        json!({"x-hasura-role": "admin"})
    );
    assert_eq!(
        got[3]["detail"]["event"]["detail"],
        json!({
            "operation_id": "1",
            "request_id": "rid-7",
            "operation_name": "Q",
            "operation_type": {"type": "completed"},
            "query": {"operationName": "Q", "query": "query Q($a: Boolean!) { __typename @include(if: $a) }"},
            "parameterized_query_hash": null,
        })
    );
    // A request that fails before executing: its `completed` has no request id.
    assert_eq!(got[4]["detail"]["event"]["detail"]["request_id"], "rid-7");
    assert_eq!(
        got[5]["detail"]["event"]["detail"]["request_id"],
        Value::Null
    );
    let query_err = &got[4]["detail"]["event"]["detail"]["operation_type"];
    assert_eq!(got[4]["level"], "error");
    assert_eq!(query_err["type"], "query_err");
    assert_eq!(query_err["detail"]["code"], "validation-failed");
    assert!(query_err["detail"]["error"].is_string());
    assert!(
        got[6]["detail"]["event"]["detail"]
            .as_str()
            .unwrap()
            .starts_with("parsing ClientMessage failed: ")
    );

    // A rejected upgrade (CORS).
    let cors = serve(ServerConfig {
        cors_domain: "https://allowed.example".into(),
        ..ServerConfig::default()
    })
    .await;
    let mut req = format!("ws://{cors}/v1/graphql")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
    let got = logs(&lines, "websocket-log", 1).await;
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(got[0]["level"], "error");
    assert_eq!(
        got[0]["detail"]["event"],
        json!({"type": "rejected", "detail": {"path": "$", "error": "received origin header does not match configured CORS domains", "code": "access-denied"}})
    );

    subscriptions(&lines).await;
}

/// A live query: `started`, polls logged as `livequery-poller-log`, `stopped`.
async fn subscriptions(lines: &Lines) {
    let Some(db) = hge_testdb::TestDb::create("websocket_log").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping subscriptions");
        return;
    };
    let (client, conn) = tokio_postgres::connect(db.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(Path::new(FIXTURE).join("schema.sql")).unwrap();
    client.batch_execute(&sql).await.unwrap();
    let mut config = ServerConfig {
        database_url: Some(db.url().to_owned()),
        metadata_dir: Some(Path::new(FIXTURE).join("metadata")),
        admin_secrets: vec![SECRET.into()],
        ..ServerConfig::default()
    };
    config.websocket.live_queries.refetch_interval = Duration::from_millis(50);
    let addr = serve(config).await;
    logs(lines, "websocket-log", 0).await;

    let mut socket = connect(addr, &[]).await;
    send(&mut socket, json!({"type": "connection_init", "payload": {"headers": {"x-hasura-admin-secret": SECRET}}})).await;
    recv(&mut socket, "connection_ack").await;
    send(&mut socket, json!({"type": "start", "id": "s", "payload": {"query": "subscription S { message(order_by: {id: asc}) { id } }", "operationName": "S"}})).await;
    recv(&mut socket, "data").await;
    let polls = logs(lines, "livequery-poller-log", 2).await;
    assert!(polls.len() >= 2, "{polls:#?}");
    let poll = &polls[0];
    assert_eq!(poll["level"], "info");
    let d = &poll["detail"];
    let keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "poller_id",
            "kind",
            "snapshot_time",
            "batches",
            "execution_batches",
            "subscriber_count",
            "total_time",
            "source",
            "generated_sql",
            "role",
            "subscription_options",
        ]
    );
    assert_eq!(d["kind"], "live-query");
    assert_eq!(d["source"], "default");
    assert_eq!(d["role"], "admin");
    assert_eq!(d["subscriber_count"], 1);
    assert_eq!(
        d["subscription_options"],
        json!({"batch_size": 100, "refetch_delay": 0.05})
    );
    assert_eq!(d["batches"], d["execution_batches"]);
    let batch = &d["batches"][0];
    assert_eq!(batch["batch_id"], 1);
    assert!(batch["db_execution_time"].is_f64());
    assert!(batch["batch_response_size_bytes"].as_u64().unwrap() > 0);
    assert_eq!(polls[1]["detail"]["poller_id"], d["poller_id"]);

    send(&mut socket, json!({"type": "stop", "id": "s"})).await;
    socket.close(None).await.unwrap();
    let got = logs(lines, "websocket-log", 4).await;
    assert_eq!(
        event_types(&got),
        [
            "accepted",
            "operation:started",
            "operation:stopped",
            "closed"
        ],
        "{got:#?}"
    );
    let started = &got[1]["detail"]["event"]["detail"];
    assert_eq!(started["operation_name"], "S");
    assert_eq!(started["request_id"].as_str().unwrap().len(), 36);
    // A stop carries neither the request id nor the query.
    let stopped = &got[2]["detail"]["event"]["detail"];
    assert_eq!(stopped["request_id"], Value::Null);
    assert_eq!(stopped["query"], Value::Null);
}
