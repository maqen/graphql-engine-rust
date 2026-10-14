//! `run_sql` changes that remove event triggers (a dropped column with `cascade`, a dropped
//! table) must not wait on their own transaction: the triggers' SQL is dropped after it
//! commits, as in V2.
//!
//! Runs in a throwaway database on the Postgres server `HASURA_GRAPHQL_DATABASE_URL` points
//! at (see `hge-testdb`); without it the test is skipped.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use hge_server::ServerConfig;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn send(router: &Router, url: &str, body: Value) -> (u16, Value) {
    let req = Request::post(url)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let call = router.clone().oneshot(req);
    let resp = tokio::time::timeout(Duration::from_secs(30), call)
        .await
        .unwrap_or_else(|_| panic!("{url} {body} did not answer"))
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn run_sql(router: &Router, sql: &str, cascade: bool) -> (u16, Value) {
    let args = json!({"sql": sql, "cascade": cascade});
    send(
        router,
        "/v2/query",
        json!({"type": "run_sql", "args": args}),
    )
    .await
}

/// Names of the installed `notify_hasura_*` functions.
async fn functions(router: &Router) -> Vec<Value> {
    let sql = "SELECT proname FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
               WHERE n.nspname = 'hdb_catalog' AND proname LIKE 'notify_hasura_%' ORDER BY 1";
    let (_, r) = run_sql(router, sql, false).await;
    r["result"].as_array().unwrap()[1..]
        .iter()
        .map(|row| row[0].clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_trigger_objects_does_not_hang() {
    let Some(db) = hge_testdb::TestDb::create("run_sql_event_triggers").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping");
        return;
    };
    let config = ServerConfig {
        database_url: Some(db.url().to_owned()),
        ..ServerConfig::default()
    };
    let engine = hge_server::build_engine(config).await.unwrap();
    let router = hge_server::routes::router(engine);

    let (status, r) = run_sql(
        &router,
        "CREATE TABLE t (id int PRIMARY KEY, c1 text, c2 text); \
         CREATE TABLE u (id int PRIMARY KEY);",
        false,
    )
    .await;
    assert_eq!(status, 200, "{r}");
    let trigger = |name: &str, columns: Value| {
        json!({"name": name, "webhook": "http://127.0.0.1:1",
               "definition": {"update": {"columns": columns}}})
    };
    let metadata = json!({"version": 3, "sources": [{
    "name": "default", "kind": "postgres",
    "configuration": {"connection_info": {"database_url": db.url()}},
    "tables": [
        {"table": {"schema": "public", "name": "t"},
         "event_triggers": [trigger("t_c1", json!(["c1"])), trigger("t_all", json!("*"))]},
        {"table": {"schema": "public", "name": "u"},
         "event_triggers": [trigger("u_all", json!("*"))]}
    ]}]});
    let (status, r) = send(
        &router,
        "/v1/metadata",
        json!({"type": "replace_metadata", "args": metadata}),
    )
    .await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(
        functions(&router).await,
        [
            "notify_hasura_t_all_UPDATE",
            "notify_hasura_t_c1_UPDATE",
            "notify_hasura_u_all_UPDATE"
        ]
    );

    let (status, r) = run_sql(&router, "ALTER TABLE t DROP COLUMN c1", true).await;
    assert_eq!(status, 200, "{r}");
    let (status, r) = run_sql(&router, "DROP TABLE u", false).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(functions(&router).await, ["notify_hasura_t_all_UPDATE"]);
}
