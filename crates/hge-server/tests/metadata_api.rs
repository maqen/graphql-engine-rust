//! The metadata and schema APIs replayed against responses recorded from Hasura V2.
//!
//! `metadata_core.json` is a stateful scenario (run_sql, replace/export, inconsistencies,
//! reload, catalog state, bulk, errors, schema changes through run_sql) recorded from
//! `hasura/graphql-engine:v2.51.0-ce` on a fresh database. It is replayed in order against
//! an empty catalog and every response must match exactly, key order included. Steps
//! marked `todo` are known differences: they must still differ, so fixing one means
//! removing its marker.
//!
//! The database comes from `HASURA_GRAPHQL_DATABASE_URL` (its `hdb_catalog` is dropped first); without it the test is
//! skipped.

use axum::body::Body;
use axum::http::Request;
use hge_server::ServerConfig;
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

const SCENARIO: &str = include_str!("metadata_core.json");

#[derive(Deserialize)]
struct Scenario {
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    url: String,
    query: Value,
    status: u16,
    response: Value,
    #[serde(default)]
    todo: Option<String>,
}

/// Catalog ids are random UUIDs; compare them as a placeholder.
fn normalise(v: &Value) -> String {
    let text = serde_json::to_string(v).unwrap();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 36 <= bytes.len() && is_uuid(&bytes[i..i + 36]) {
            out.push_str("<uuid>");
            i += 36;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn is_uuid(b: &[u8]) -> bool {
    b.iter().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => *c == b'-',
        _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
    })
}

/// Start from an empty catalog. The scenario's first step recreates its own tables.
async fn reset_database(url: &str) {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect to test database");
    tokio::spawn(conn);
    client
        .batch_execute("DROP SCHEMA IF EXISTS hdb_catalog CASCADE")
        .await
        .expect("drop hdb_catalog");
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_api_matches_v2() {
    // The scenario's metadata reads the database from this same variable.
    let Ok(url) = std::env::var("HASURA_GRAPHQL_DATABASE_URL") else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping end-to-end tests");
        return;
    };
    reset_database(&url).await;
    let config = ServerConfig {
        database_url: Some(url),
        ..ServerConfig::default()
    };
    let engine = hge_server::build_engine(config.clone()).await.unwrap();
    let router = hge_server::routes::router(engine.clone());

    let scenario: Scenario = serde_json::from_str(SCENARIO).unwrap();
    let mut failures = vec![];
    for (i, step) in scenario.steps.iter().enumerate() {
        let (status, actual) = post(&router, &step.url, &step.query).await;
        let matches = status == step.status && normalise(&actual) == normalise(&step.response);
        match (&step.todo, matches) {
            (None, true) | (Some(_), false) => {}
            (None, false) => failures.push(format!(
                "step {i} {} {}\n  expected ({}): {}\n  actual   ({status}): {}",
                step.url,
                step.query,
                step.status,
                normalise(&step.response),
                normalise(&actual),
            )),
            (Some(todo), true) => failures.push(format!(
                "step {i} now matches V2; remove its todo marker (\"{todo}\")"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} step(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );

    session_recorded(&router).await;
    replicas_sync(&router, config).await;
}

/// run_sql records the caller's session variables for event triggers, as V2 does.
async fn session_recorded(router: &axum::Router) {
    let query =
        json!({"type": "run_sql", "args": {"sql": "select current_setting('hasura.user')"}});
    let (status, body) =
        post_with(router, "/v2/query", &query, &[("x-hasura-user-id", "42")]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["result"][1][0],
        r#"{"x-hasura-role":"admin","x-hasura-user-id":"42"}"#
    );
}

async fn post(router: &axum::Router, url: &str, body: &Value) -> (u16, Value) {
    post_with(router, url, body, &[]).await
}

async fn post_with(
    router: &axum::Router,
    url: &str,
    body: &Value,
    headers: &[(&str, &str)],
) -> (u16, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(url)
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

/// A second instance on the same catalog picks up changes made through the first.
async fn replicas_sync(first: &axum::Router, config: ServerConfig) {
    let second = hge_server::build_engine(config).await.unwrap();
    let second_router = hge_server::routes::router(second.clone());
    let query = json!({"query": "{ synced { id } }"});
    let (_, before) = post(&second_router, "/v1/graphql", &query).await;
    assert!(before.get("errors").is_some(), "{before}");
    assert!(!hge_server::sync::sync_once(&second).await.unwrap());

    let (status, body) = post(
        first,
        "/v2/query",
        &json!({"type": "bulk", "args": [
            {"type": "run_sql", "args": {"sql": "DROP TABLE IF EXISTS synced; CREATE TABLE synced(id int primary key); INSERT INTO synced VALUES (7)"}},
        ]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (_, mut metadata) = post(
        first,
        "/v1/metadata",
        &json!({"type": "export_metadata", "args": {}}),
    )
    .await;
    metadata["sources"][0]["tables"] = json!([{"table": {"schema": "public", "name": "synced"}}]);
    let (status, body) = post(
        first,
        "/v1/metadata",
        &json!({"type": "replace_metadata", "args": metadata}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    assert!(hge_server::sync::sync_once(&second).await.unwrap());
    let (status, after) = post(&second_router, "/v1/graphql", &query).await;
    assert_eq!(
        (status, after),
        (200, json!({"data": {"synced": [{"id": 7}]}}))
    );
    assert!(!hge_server::sync::sync_once(&second).await.unwrap());
}
