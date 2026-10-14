//! `action-handler-log`: V2's `ActionHandlerLog`, one line per action handler response. Needs
//! the Postgres server `HASURA_GRAPHQL_DATABASE_URL` points at (skipped without it).

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::{any, post};
use hge_server::ServerConfig;
use hge_server::telemetry::{self, Level, LogConfig, LogType, LogTypes};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASIC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/basic");
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/actions");
const SECRET: &str = "s3cret";

type Lines = Arc<Mutex<Vec<String>>>;

async fn start_handler() -> std::net::SocketAddr {
    let router = axum::Router::new()
        .route(
            "/author",
            post(|| async { Json(json!({"authorId": 1, "label": "one"})) }),
        )
        .route(
            "/respond",
            post(|Json(p): Json<Value>| async move {
                let status = p["input"]["status"].as_u64().unwrap() as u16;
                let body = p["input"]["body"].as_str().unwrap().to_owned();
                (StatusCode::from_u16(status).unwrap(), body)
            }),
        )
        .route(
            "/slow",
            post(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                Json(json!("late"))
            }),
        )
        .fallback(any(|| async { Json(json!("ok")) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

async fn router(db: &str, base_url: String, redact: bool) -> axum::Router {
    let engine = hge_server::build_engine(ServerConfig {
        database_url: Some(db.to_owned()),
        metadata_dir: Some(Path::new(FIXTURE).join("metadata")),
        admin_secrets: vec![SECRET.into()],
        env_overrides: [
            ("ACTION_BASE_URL", base_url),
            ("STATIC_PART", "part".to_owned()),
            ("SECRET_HEADER", "s".to_owned()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect(),
        redact_action_handler_logs: redact,
        ..ServerConfig::default()
    })
    .await
    .unwrap();
    hge_server::routes::router(engine)
}

/// Run a query as the `user` role and return the `action-handler-log` lines it wrote.
async fn run(router: &axum::Router, lines: &Lines, query: &str) -> Vec<Value> {
    let req = Request::post("/v1/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-hasura-admin-secret", SECRET)
        .header("x-hasura-role", "user")
        .header("x-hasura-user-id", "7")
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let _: Value = serde_json::from_slice(&body).unwrap();
    let mut lines = lines.lock().unwrap();
    let logs = lines
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|l| l["type"] == "action-handler-log")
        .collect();
    lines.clear();
    logs
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

#[tokio::test]
async fn action_handler_log() {
    let mut types = LogTypes::DEFAULT;
    types.insert(LogType::ActionHandlerLog);
    let lines = telemetry::init_capture(LogConfig {
        level: Level::Info,
        types,
    });
    let Some(db) = hge_testdb::TestDb::create("action_handler_log").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping");
        return;
    };
    let (client, conn) = tokio_postgres::connect(db.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(Path::new(BASIC).join("schema.sql")).unwrap();
    client.batch_execute(&sql).await.unwrap();
    let addr = start_handler().await;
    let router = router(db.url(), format!("http://{addr}"), false).await;

    // A query action without transforms.
    let logs = run(&router, &lines, "{ authorInfo(id: 1) { authorId } }").await;
    assert_eq!(logs.len(), 1, "{logs:#?}");
    assert_eq!(logs[0]["level"], "info");
    let d = &logs[0]["detail"];
    assert_eq!(
        keys(d),
        [
            "action_name",
            "action_type",
            "request",
            "request_size",
            "response_size"
        ]
    );
    assert_eq!(d["action_name"], "authorInfo");
    assert_eq!(d["action_type"], "query");
    let request = &d["request"];
    assert_eq!(
        keys(request),
        [
            "body",
            "headers",
            "method",
            "query_string",
            "response_timeout",
            "url"
        ]
    );
    assert_eq!(request["method"], "POST");
    assert_eq!(request["url"], format!("http://{addr}/author"));
    assert_eq!(request["query_string"], "");
    assert_eq!(request["response_timeout"], "30000000");
    assert!(
        request["headers"]
            .as_array()
            .unwrap()
            .contains(&json!(["Content-Type", "application/json"])),
        "{request}"
    );
    let body = request["body"].as_str().unwrap();
    let payload: Value = serde_json::from_str(body).unwrap();
    assert_eq!(payload["action"]["name"], "authorInfo");
    assert_eq!(d["request_size"], body.len());
    assert_eq!(d["response_size"], r#"{"authorId":1,"label":"one"}"#.len());

    // With a request transform: the transformed request too.
    let logs = run(&router, &lines, r#"{ transformRequest(id: 3, name: "n") }"#).await;
    assert_eq!(logs.len(), 1, "{logs:#?}");
    let d = &logs[0]["detail"];
    assert_eq!(
        keys(d),
        [
            "action_name",
            "action_type",
            "request",
            "request_size",
            "request_trans",
            "response_size",
            "transformed_request_size"
        ]
    );
    assert_eq!(d["request"]["method"], "POST");
    let trans = &d["request_trans"];
    assert_eq!(trans["method"], "PUT");
    assert!(
        trans["url"]
            .as_str()
            .unwrap()
            .ends_with("/users/3?id=3&role=user"),
        "{trans}"
    );
    assert_eq!(
        d["transformed_request_size"],
        trans["body"].as_str().unwrap().len()
    );

    // Any response is logged, even an error status with a body that is not JSON.
    let logs = run(
        &router,
        &lines,
        r#"{ respond(status: 500, body: "not json") { authorId } }"#,
    )
    .await;
    assert_eq!(logs.len(), 1, "{logs:#?}");
    assert_eq!(logs[0]["detail"]["response_size"], "not json".len());

    // No response (a timeout): no line.
    let logs = run(&router, &lines, "{ slow }").await;
    assert!(logs.is_empty(), "{logs:#?}");

    // HASURA_GRAPHQL_REDACT_ACTION_HANDLER_LOGS: bodies are null.
    let redacted = self::router(db.url(), format!("http://{addr}"), true).await;
    let logs = run(
        &redacted,
        &lines,
        r#"{ transformRequest(id: 3, name: "n") }"#,
    )
    .await;
    let d = &logs[0]["detail"];
    assert_eq!(d["request"]["body"], Value::Null);
    assert_eq!(d["request_trans"]["body"], Value::Null);
    assert!(d["request_size"].as_u64().unwrap() > 0);

    // URL credentials are never logged.
    let with_credentials = self::router(db.url(), format!("http://u:p@{addr}"), false).await;
    let logs = run(
        &with_credentials,
        &lines,
        "{ authorInfo(id: 1) { authorId } }",
    )
    .await;
    assert_eq!(
        logs[0]["detail"]["request"]["url"],
        format!("http://{addr}/author")
    );
}
