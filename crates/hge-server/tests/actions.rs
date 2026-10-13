//! End-to-end tests for actions: the engine runs in-process against a real Postgres (as in
//! `graphql.rs`) and calls a mock action handler served on a local port.
//!
//! Cases live in `tests/cases/actions/*.yaml` (same format as `tests/cases/`, plus optional
//! `response_headers` to assert on).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use hge_server::ServerConfig;
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASIC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/basic");
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/actions");
const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases/actions");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    description: String,
    #[serde(default = "default_url")]
    url: String,
    #[serde(default = "default_status")]
    status: u16,
    #[serde(default)]
    headers: indexmap::IndexMap<String, String>,
    query: Value,
    response: Value,
    #[serde(default)]
    response_headers: indexmap::IndexMap<String, Vec<String>>,
}

fn default_url() -> String {
    "/v1/graphql".to_owned()
}

fn default_status() -> u16 {
    200
}

/// The mock handler. Every route receives the V2 action payload.
fn handler_router() -> axum::Router {
    async fn echo(headers: HeaderMap, Json(payload): Json<Value>) -> Json<Value> {
        let mut names: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
            .filter(|(k, _)| !matches!(k.as_str(), "content-length" | "host" | "accept"))
            .collect();
        names.sort();
        Json(json!({"payload": payload, "headers": names}))
    }
    async fn author(Json(p): Json<Value>) -> Json<Value> {
        let id = &p["input"]["id"];
        Json(json!({"authorId": id, "label": format!("author {id}"), "address": {"city": "Oslo"}}))
    }
    async fn authors(Json(p): Json<Value>) -> Json<Value> {
        let items: Vec<Value> = p["input"]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| json!({"authorId": id, "label": format!("author {id}")}))
            .collect();
        Json(Value::Array(items))
    }
    async fn respond(Json(p): Json<Value>) -> Response {
        let status = p["input"]["status"].as_u64().unwrap_or(200) as u16;
        let body = &p["input"]["body"];
        let text = match body {
            Value::String(s) if s.starts_with("raw:") => s[4..].to_owned(),
            other => other.to_string(),
        };
        (
            StatusCode::from_u16(status).unwrap(),
            [("content-type", "application/json")],
            text,
        )
            .into_response()
    }
    async fn slow() -> Json<Value> {
        tokio::time::sleep(Duration::from_secs(3)).await;
        Json(json!("late"))
    }
    async fn login(Json(p): Json<Value>) -> Response {
        if p["input"]["password"] == "secret" {
            (
                axum::response::AppendHeaders([
                    ("set-cookie", "session=abc; Path=/"),
                    ("set-cookie", "theme=dark"),
                ]),
                Json(
                    json!({"token": format!("token-{}", p["input"]["username"].as_str().unwrap())}),
                ),
            )
                .into_response()
        } else {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"message": "invalid credentials", "code": "invalid-credentials"})),
            )
                .into_response()
        }
    }
    axum::Router::new()
        .route("/echo", post(echo))
        .route("/author", post(author))
        .route("/authors", post(authors))
        .route("/respond", post(respond))
        .route("/slow", post(slow))
        .route("/login", post(login))
}

async fn start_handler() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, handler_router()).await });
    addr
}

async fn setup_database(url: &str) {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect to test database");
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(Path::new(BASIC).join("schema.sql")).unwrap();
    client.batch_execute(&sql).await.expect("apply schema.sql");
}

fn case_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(CASES)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "yaml"))
        .collect();
    files.sort();
    files
}

#[tokio::test(flavor = "multi_thread")]
async fn action_cases() {
    let Ok(url) = std::env::var("HASURA_GRAPHQL_DATABASE_URL") else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping action tests");
        return;
    };
    setup_database(&url).await;
    let addr = start_handler().await;
    let config = ServerConfig {
        database_url: Some(url),
        metadata_dir: Some(Path::new(FIXTURE).join("metadata")),
        env_overrides: [
            ("ACTION_BASE_URL", format!("http://{addr}")),
            ("STATIC_PART", "part".to_owned()),
            ("SECRET_HEADER", "s3cret".to_owned()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect(),
        ..ServerConfig::default()
    };
    let engine = hge_server::build_engine(config)
        .await
        .expect("build engine");
    let inconsistencies = &engine.state.load().inconsistencies;
    assert!(
        inconsistencies.is_empty(),
        "inconsistent metadata: {inconsistencies:?}"
    );
    let router = hge_server::routes::router(engine);

    let mut failures = vec![];
    for path in case_files() {
        let file: CaseFile = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        for case in file.cases {
            let mut req = Request::post(&case.url).header("content-type", "application/json");
            for (k, v) in &case.headers {
                req = req.header(k, v);
            }
            let body = Body::from(serde_json::to_vec(&case.query).unwrap());
            let resp = router
                .clone()
                .oneshot(req.body(body).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let mut header_failures = vec![];
            for (name, expected) in &case.response_headers {
                let actual: Vec<String> = resp
                    .headers()
                    .get_all(name.as_str())
                    .iter()
                    .map(|v| v.to_str().unwrap().to_owned())
                    .collect();
                if &actual != expected {
                    header_failures.push(format!("{name}: expected {expected:?}, got {actual:?}"));
                }
            }
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let actual: Value = serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
            if status != StatusCode::from_u16(case.status).unwrap()
                || actual != case.response
                || !header_failures.is_empty()
            {
                failures.push(format!(
                    "{}: {}\n  expected ({}): {}\n  actual   ({}): {}\n  {}",
                    path.file_name().unwrap().to_string_lossy(),
                    case.description,
                    case.status,
                    case.response,
                    status.as_u16(),
                    actual,
                    header_failures.join("; "),
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );

    // More distinct join keys than Postgres allows columns in one SELECT (1664): the keyed
    // queries are split over several statements.
    let ids: Vec<u32> = (1..=2000).collect();
    let query =
        json!({"query": format!("{{ authorInfos(ids: {ids:?}) {{ author {{ name }} }} }}")});
    let req = Request::post("/v1/graphql")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&query).unwrap()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let actual: Value = serde_json::from_slice(&bytes).unwrap();
    let names = |i: usize| &actual["data"]["authorInfos"][i]["author"];
    assert_eq!(
        actual["data"]["authorInfos"].as_array().map(Vec::len),
        Some(2000),
        "{}",
        actual["errors"][0]["message"]
    );
    assert_eq!(names(0), &json!({"name": "Alice"}));
    assert_eq!(names(2), &json!({"name": "Carol"}));
    assert_eq!(names(3), &Value::Null);
    assert_eq!(names(1999), &Value::Null);
}
