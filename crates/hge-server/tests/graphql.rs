//! End-to-end tests against a real Postgres.
//!
//! Each file in `tests/cases/` holds an optional server `config` and a list of
//! V2-style cases (`description`, `url`, `status`, `headers`, `query`, `response`).
//! `config.fixture` names the directory under `tests/fixtures/` (default `basic`) whose
//! `schema.sql` is applied and whose `metadata/` is loaded before the file runs.
//! Requests go through the full router in-process. The tests run in a fresh database on the
//! server `HASURA_GRAPHQL_DATABASE_URL` points at (see `hge-testdb`); without it the test is
//! skipped.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hge_server::ServerConfig;
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/basic");
const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases");

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    fixture: Option<String>,
    admin_secret: Option<String>,
    unauthorized_role: Option<String>,
    /// `HASURA_GRAPHQL_JWT_SECRET`, written as a YAML mapping.
    jwt_secret: Option<Value>,
    disable_admin_secret: bool,
    stringify_numeric_types: bool,
    v1_boolean_null_collapse: bool,
    preserve_401_errors: bool,
    /// `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS` (default `true`).
    admin_internal_errors: Option<bool>,
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

async fn setup_database(url: &str, fixture: &Path) {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect to test database");
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(fixture.join("schema.sql")).unwrap();
    client.batch_execute(&sql).await.expect("apply schema.sql");
}

/// Directories under `tests/cases/` whose cases have their own runner (`tests/<name>.rs`).
const OWN_RUNNER: &[&str] = &["actions"];

/// Every `*.yaml` under `tests/cases/`, including subdirectories (`cases/auth/…`).
fn case_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if dir == Path::new(CASES)
                    && path
                        .file_name()
                        .is_some_and(|n| OWN_RUNNER.iter().any(|r| n == *r))
                {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "yaml") {
                out.push(path);
            }
        }
    }
    let mut files = vec![];
    walk(Path::new(CASES), &mut files);
    files.sort();
    files
}

async fn run_file(path: &Path, database_url: &str) -> Vec<String> {
    let file: CaseFile = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let c = file.config;
    let fixture = Path::new(FIXTURES).join(c.fixture.as_deref().unwrap_or("basic"));
    setup_database(database_url, &fixture).await;
    let config = ServerConfig {
        database_url: Some(database_url.to_owned()),
        metadata_dir: Some(fixture.join("metadata")),
        admin_secrets: c.admin_secret.into_iter().collect(),
        unauthorized_role: c.unauthorized_role,
        jwt_secret: c.jwt_secret.map(|v| v.to_string()),
        disable_admin_secret: c.disable_admin_secret,
        stringify_numeric_types: c.stringify_numeric_types,
        v1_boolean_null_collapse: c.v1_boolean_null_collapse,
        preserve_401_errors: c.preserve_401_errors,
        admin_internal_errors: c
            .admin_internal_errors
            .unwrap_or(ServerConfig::default().admin_internal_errors),
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
                path.strip_prefix(CASES).unwrap_or(path).display(),
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
    let Some(db) = hge_testdb::TestDb::create("graphql").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping end-to-end tests");
        return;
    };
    let url = db.url().to_owned();
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
    setup_database(database_url, Path::new(FIXTURE)).await;
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
