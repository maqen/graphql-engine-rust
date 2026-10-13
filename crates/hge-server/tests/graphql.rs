//! End-to-end tests against a real Postgres.
//!
//! Each file in `tests/cases/` holds an optional server `config` and a list of
//! V2-style cases (`description`, `url`, `status`, `headers`, `query`, `response`).
//! Requests go through the full router in-process. The database comes from
//! `HASURA_GRAPHQL_DATABASE_URL` (also what the fixture metadata reads); without it
//! the test is skipped.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hge_server::ServerConfig;
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/basic");
const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases");

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    admin_secret: Option<String>,
    unauthorized_role: Option<String>,
    stringify_numeric_types: bool,
    v1_boolean_null_collapse: bool,
    preserve_401_errors: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    #[serde(default)]
    config: FileConfig,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    description: String,
    #[serde(default = "default_url")]
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default = "default_status")]
    status: u16,
    #[serde(default)]
    headers: indexmap::IndexMap<String, String>,
    #[serde(default)]
    query: Option<Value>,
    response: Value,
}

fn default_url() -> String {
    "/v1/graphql".to_owned()
}

fn default_status() -> u16 {
    200
}

async fn setup_database(url: &str) {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect to test database");
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(Path::new(FIXTURE).join("schema.sql")).unwrap();
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

async fn run_file(path: &Path, database_url: &str) -> Vec<String> {
    let file: CaseFile = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let c = file.config;
    let config = ServerConfig {
        database_url: Some(database_url.to_owned()),
        metadata_dir: Some(Path::new(FIXTURE).join("metadata")),
        admin_secrets: c.admin_secret.into_iter().collect(),
        unauthorized_role: c.unauthorized_role,
        stringify_numeric_types: c.stringify_numeric_types,
        v1_boolean_null_collapse: c.v1_boolean_null_collapse,
        preserve_401_errors: c.preserve_401_errors,
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
    for case in file.cases {
        let method = case
            .method
            .clone()
            .unwrap_or_else(|| if case.query.is_some() { "POST" } else { "GET" }.to_owned());
        let mut req = Request::builder().method(method.as_str()).uri(&case.url);
        for (k, v) in &case.headers {
            req = req.header(k, v);
        }
        let body = match &case.query {
            Some(q) => {
                req = req.header("content-type", "application/json");
                Body::from(serde_json::to_vec(q).unwrap())
            }
            None => Body::empty(),
        };
        let resp = router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let actual: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        if status != StatusCode::from_u16(case.status).unwrap() || actual != case.response {
            failures.push(format!(
                "{}: {}\n  expected ({}): {}\n  actual   ({}): {}",
                path.file_name().unwrap().to_string_lossy(),
                case.description,
                case.status,
                case.response,
                status.as_u16(),
                actual,
            ));
        }
    }
    failures
}

#[tokio::test(flavor = "multi_thread")]
async fn graphql_cases() {
    let Ok(url) = std::env::var("HASURA_GRAPHQL_DATABASE_URL") else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping end-to-end tests");
        return;
    };
    setup_database(&url).await;
    let mut failures = vec![];
    for file in case_files() {
        failures.extend(run_file(&file, &url).await);
    }
    failures.extend(query_limits(&url).await);
    assert!(
        failures.is_empty(),
        "{} case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Queries built to exhaust the server (deep fragment chains, exponential fragment
/// fan-out) are rejected quickly instead of overflowing the stack or burning CPU.
async fn query_limits(database_url: &str) -> Vec<String> {
    let config = ServerConfig {
        database_url: Some(database_url.to_owned()),
        metadata_dir: Some(Path::new(FIXTURE).join("metadata")),
        ..ServerConfig::default()
    };
    let router = hge_server::routes::router(hge_server::build_engine(config).await.unwrap());

    // 4000 fragments, each one level deeper than the last.
    let mut deep = String::from("{ author { ...F0 } }");
    for i in 0..4000 {
        if i % 2 == 0 {
            deep += &format!(
                " fragment F{i} on author {{ articles {{ ...F{} }} }}",
                i + 1
            );
        } else {
            deep += &format!(" fragment F{i} on article {{ author {{ ...F{} }} }}", i + 1);
        }
    }
    deep += " fragment F4000 on author { id }";

    // Each fragment spreads the next one twice: 2^30 selections if expanded.
    let mut wide = String::from("{ __type(name: \"author\") { ...F0 } }");
    for i in 0..30 {
        wide += &format!(
            " fragment F{i} on __Type {{ name a: ofType {{ ...F{n} }} b: ofType {{ ...F{n} }} }}",
            n = i + 1
        );
    }
    wide += " fragment F30 on __Type { name }";

    let mut failures = vec![];
    for (name, query, expected) in [
        (
            "deep fragment chain",
            deep,
            "the query exceeds the maximum depth of 100",
        ),
        (
            "exponential fragment spreads",
            wide,
            "the query exceeds the maximum of 20000 selections",
        ),
    ] {
        let started = std::time::Instant::now();
        let req = Request::post("/v1/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "query": query }).to_string(),
            ))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let message = body["errors"][0]["message"].as_str().unwrap_or_default();
        if message != expected || started.elapsed() > std::time::Duration::from_secs(5) {
            failures.push(format!(
                "{name}: expected error {expected:?} within 5s, got {body} after {:?}",
                started.elapsed()
            ));
        }
    }
    failures
}
