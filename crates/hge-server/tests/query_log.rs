//! `query-log`: V2's `QueryLog`, one line per execution step (spec 11 §5.5). The database
//! part needs the Postgres server `HASURA_GRAPHQL_DATABASE_URL` points at (skipped without it).

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, header};
use hge_server::ServerConfig;
use hge_server::telemetry::{self, Level, LogConfig, LogType, LogTypes, query_log};
use serde_json::{Value, json};
use tower::ServiceExt;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/subscriptions");
const SECRET: &str = "s3cret";

type Lines = Arc<Mutex<Vec<String>>>;

/// Send a GraphQL request as admin and return the `query-log` lines it wrote.
async fn run(router: &axum::Router, lines: &Lines, body: Value) -> Vec<Value> {
    let req = Request::post("/v1/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-hasura-admin-secret", SECRET)
        .header("x-request-id", "rid-1")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let mut lines = lines.lock().unwrap();
    let logs = lines
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|l| l["type"] == "query-log")
        .collect();
    lines.clear();
    logs
}

#[tokio::test]
async fn query_log() {
    let mut types = LogTypes::DEFAULT;
    types.insert(LogType::QueryLog);
    let lines = telemetry::init_capture(LogConfig {
        level: Level::Info,
        types,
    });
    query_log::set_masked_variables(["secret".to_owned()].into());

    let Some(db) = hge_testdb::TestDb::create("query_log").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping");
        return;
    };
    let (client, conn) = tokio_postgres::connect(db.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(Path::new(FIXTURE).join("schema.sql")).unwrap();
    client.batch_execute(&sql).await.unwrap();
    let engine = hge_server::build_engine(ServerConfig {
        database_url: Some(db.url().to_owned()),
        metadata_dir: Some(Path::new(FIXTURE).join("metadata")),
        admin_secrets: vec![SECRET.into()],
        ..ServerConfig::default()
    })
    .await
    .unwrap();
    let router = hge_server::routes::router(engine);

    // A query: one line per root field, each with its SQL; variables masked by name.
    let query = "query Q($n: Int!, $secret: String) { a: message(limit: $n, where: {priority: {_gte: 1}, body: {_neq: $secret}}) { id } __typename }";
    let logs = run(
        &router,
        &lines,
        json!({"query": query, "operationName": "Q", "variables": {"n": 2, "secret": "pw"}}),
    )
    .await;
    assert_eq!(logs.len(), 2, "{logs:#?}");
    let d = &logs[0]["detail"];
    assert_eq!(logs[0]["level"], "info");
    let keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["query", "generated_sql", "request_id", "kind"]);
    assert_eq!(
        d["query"],
        json!({"operationName": "Q", "query": query, "variables": {"n": 2, "secret": "[MASKED]"}})
    );
    assert_eq!(d["request_id"], "rid-1");
    assert_eq!(d["kind"], "database");
    let generated = d["generated_sql"]["a"].as_object().unwrap();
    assert!(generated["query"].as_str().unwrap().contains("message"));
    // Arguments are typed like V2's: numbers as numbers, text as text.
    let args = generated["prepared_arguments"].as_array().unwrap();
    assert!(args.contains(&json!(1)), "{args:?}");
    // V2 masks variables, not the positional arguments they become.
    assert!(args.contains(&json!("pw")), "{args:?}");
    assert_eq!(logs[1]["detail"]["generated_sql"], Value::Null, "{logs:#?}");
    assert_eq!(logs[1]["detail"]["kind"], "introspection");

    // Mutations on one source share a transaction: one line, without SQL.
    let logs = run(
        &router,
        &lines,
        json!({"query": "mutation { a: insert_message_one(object: {author_id: 3, body: \"x\"}) { id } b: delete_message(where: {author_id: {_eq: 3}}) { affected_rows } }"}),
    )
    .await;
    assert_eq!(logs.len(), 1, "{logs:#?}");
    assert_eq!(logs[0]["detail"]["generated_sql"], Value::Null, "{logs:#?}");
    assert_eq!(logs[0]["detail"]["kind"], "database");
    assert!(logs[0]["detail"]["query"].get("variables").is_none());

    // Mixed with another kind of field, each mutation runs (and is logged) on its own.
    let logs = run(
        &router,
        &lines,
        json!({"query": "mutation { a: delete_message(where: {author_id: {_eq: 3}}) { affected_rows } __typename }"}),
    )
    .await;
    let kinds: Vec<&Value> = logs.iter().map(|l| &l["detail"]["kind"]).collect();
    assert_eq!(kinds, [&json!("database"), &json!("introspection")]);

    // Introspection-only responses are cached, but each request is still logged.
    for _ in 0..2 {
        let logs = run(&router, &lines, json!({"query": "{ __typename }"})).await;
        assert_eq!(logs.len(), 1, "{logs:#?}");
        assert_eq!(logs[0]["detail"]["kind"], "introspection");
    }

    // A request that fails validation runs nothing and logs nothing.
    let logs = run(&router, &lines, json!({"query": "{ nope }"})).await;
    assert!(logs.is_empty(), "{logs:#?}");

    websocket(addr_of(router).await, &lines).await;
}

async fn addr_of(router: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

/// Over websockets: queries like HTTP, subscriptions once when they start, with the request id
/// of the connection's `x-request-id`.
async fn websocket(addr: std::net::SocketAddr, lines: &Lines) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut req = format!("ws://{addr}/v1/graphql")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        "graphql-transport-ws".parse().unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut send = async |msg: Value, until: Value| {
        socket.send(Message::text(msg.to_string())).await.unwrap();
        loop {
            let Some(Ok(Message::Text(t))) = socket.next().await else {
                panic!("socket closed");
            };
            let v: Value = serde_json::from_str(&t).unwrap();
            if until.as_object().unwrap().iter().all(|(k, x)| v[k] == *x) {
                return;
            }
        }
    };
    send(
        json!({"type": "connection_init", "payload": {"headers": {"x-hasura-admin-secret": SECRET, "x-request-id": "ws-rid"}}}),
        json!({"type": "connection_ack"}),
    )
    .await;
    send(
        json!({"type": "subscribe", "id": "1", "payload": {"query": "{ message { id } }"}}),
        json!({"type": "complete", "id": "1"}),
    )
    .await;
    send(
        json!({"type": "subscribe", "id": "2", "payload": {"query": "subscription { message { id } }"}}),
        json!({"type": "next", "id": "2"}),
    )
    .await;
    let logs = {
        let mut lines = lines.lock().unwrap();
        let logs: Vec<Value> = lines
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|l| l["type"] == "query-log")
            .map(|l| l["detail"].clone())
            .collect();
        lines.clear();
        logs
    };
    assert_eq!(logs.len(), 2, "{logs:#?}");
    assert!(logs[0]["generated_sql"]["message"]["query"].is_string());
    assert_eq!(logs[1]["generated_sql"], Value::Null);
    assert_eq!(logs[1]["kind"], "database");
    assert!(
        logs.iter().all(|l| l["request_id"] == "ws-rid"),
        "{logs:#?}"
    );
}
