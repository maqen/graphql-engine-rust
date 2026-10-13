//! The action and custom type metadata commands, end to end.
//!
//! `metadata_actions.yaml` is a stateful scenario (`create_action`, `update_action`,
//! `drop_action`, `create_action_permission`, `drop_action_permission`,
//! `set_custom_types`, with their errors, bulk, `resource_version` and `export_metadata`)
//! interleaved with GraphQL calls through the new actions to a mock handler. It is replayed
//! in order against an empty catalog, like `metadata_core.json`, but it is written by hand
//! from V2's source (`RQL/DDL/Action.hs`, `RQL/DDL/CustomTypes.hs`) rather than recorded.
//! UUIDs are compared as a placeholder.
//!
//! The database comes from `HASURA_GRAPHQL_DATABASE_URL` (its `hdb_catalog` is dropped
//! first); without it the test is skipped.

use std::net::SocketAddr;

use axum::Json;
use axum::body::Body;
use axum::http::Request;
use axum::routing::post;
use hge_server::ServerConfig;
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

const SCENARIO: &str = include_str!("metadata_actions.yaml");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    description: String,
    #[serde(default = "default_url")]
    url: String,
    #[serde(default)]
    headers: indexmap::IndexMap<String, String>,
    query: Value,
    #[serde(default = "default_status")]
    status: u16,
    response: Value,
}

fn default_url() -> String {
    "/v1/metadata".to_owned()
}

fn default_status() -> u16 {
    200
}

/// The mock action handler.
async fn start_handler() -> SocketAddr {
    async fn user(Json(p): Json<Value>) -> Json<Value> {
        let id = &p["input"]["id"];
        Json(
            json!({"id": id, "name": format!("user {id}"), "role": p["session_variables"]["x-hasura-role"]}),
        )
    }
    let router = axum::Router::new().route("/user", post(user));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

/// UUIDs (async action ids, catalog ids) become `<uuid>`.
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
            let c = text[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
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

#[tokio::test(flavor = "multi_thread")]
async fn action_metadata_commands() {
    let Ok(url) = std::env::var("HASURA_GRAPHQL_DATABASE_URL") else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping end-to-end tests");
        return;
    };
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("connect to test database");
    tokio::spawn(conn);
    client
        .batch_execute("DROP SCHEMA IF EXISTS hdb_catalog CASCADE")
        .await
        .expect("drop hdb_catalog");
    let addr = start_handler().await;
    let config = ServerConfig {
        database_url: Some(url),
        env_overrides: [("ACTION_BASE_URL".to_owned(), format!("http://{addr}"))]
            .into_iter()
            .collect(),
        ..ServerConfig::default()
    };
    let engine = hge_server::build_engine(config).await.unwrap();
    let router = hge_server::routes::router(engine);

    let scenario: Scenario = serde_yaml::from_str(SCENARIO).unwrap();
    let mut failures = vec![];
    for (i, step) in scenario.steps.iter().enumerate() {
        let mut req = Request::post(&step.url).header("content-type", "application/json");
        for (k, v) in &step.headers {
            req = req.header(k, v);
        }
        let body = Body::from(serde_json::to_vec(&step.query).unwrap());
        let resp = router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let actual: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        if status != step.status || normalise(&actual) != normalise(&step.response) {
            failures.push(format!(
                "step {i} ({}): {}\n  expected ({}): {}\n  actual   ({status}): {}",
                step.description,
                step.query,
                step.status,
                normalise(&step.response),
                normalise(&actual),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} step(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
