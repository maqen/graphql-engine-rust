//! Sources, tables, relationships and permissions commands replayed against responses
//! recorded from Hasura V2.
//!
//! `metadata_tables.json` was recorded from `hasura/graphql-engine:v2.51.0-ce` on fresh
//! databases: the default source reads `HASURA_GRAPHQL_DATABASE_URL` and a second source
//! reads `SRC2_URL`. Steps marked `todo` are known differences: they must still differ, so
//! fixing one means removing its marker.
//!
//! The scenario lists every table of its databases, so it gets two databases of its own
//! (`<test database>_tables_<pid>` and `…_src2`, dropped afterwards). Since the metadata
//! names its databases by environment variable, the test re-runs itself in a child process
//! with those variables pointing at them. Without `HASURA_GRAPHQL_DATABASE_URL`, or without
//! the privilege to create databases, the test is skipped. The remote schema the scenario
//! adds (`REMOTE_URL`) is an in-process GraphQL server with V2's recording schema.

use axum::body::Body;
use axum::http::Request;
use hge_server::ServerConfig;
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;

const SCENARIO: &str = include_str!("metadata_tables.json");
const TEST_NAME: &str = "metadata_tables_match_v2";
/// Set in the child process that runs the scenario.
const CHILD: &str = "HGE_METADATA_TABLES_CHILD";

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

/// `url` with its database name replaced.
fn with_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let base = match base.rfind('/') {
        Some(i) if base[..i].contains("://") && !base[..i].ends_with('/') => &base[..i],
        _ => base,
    };
    match query {
        Some(q) => format!("{base}/{database}?{q}"),
        None => format!("{base}/{database}"),
    }
}

fn database_name(url: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    base.rsplit('/').next().unwrap_or("postgres").to_owned()
}

/// Create the scenario databases. `Ok(false)` when the role may not create databases.
async fn create_databases(client: &tokio_postgres::Client, names: &[&str]) -> Result<bool, String> {
    for name in names {
        client
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .await
            .map_err(|e| format!("drop scenario database: {e}"))?;
        match client
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .await
        {
            Ok(()) => {}
            Err(e)
                if e.code() == Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE) =>
            {
                return Ok(false);
            }
            Err(e) => return Err(format!("create scenario database: {e}")),
        }
    }
    Ok(true)
}

async fn drop_databases(client: &tokio_postgres::Client, names: &[&str]) {
    for name in names {
        if let Err(e) = client
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .await
        {
            eprintln!("could not drop scenario database {name}: {e}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_tables_match_v2() {
    let Ok(url) = std::env::var("HASURA_GRAPHQL_DATABASE_URL") else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping end-to-end tests");
        return;
    };
    if std::env::var(CHILD).is_ok() {
        replay(url).await;
        return;
    }
    let main = format!("{}_tables_{}", database_name(&url), std::process::id());
    let src2 = format!("{main}_src2");
    let names = [main.as_str(), src2.as_str()];
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("connect to test database");
    tokio::spawn(conn);
    match create_databases(&client, &names).await {
        Ok(true) => {}
        Ok(false) => {
            drop_databases(&client, &names).await;
            eprintln!("not permitted to create databases; skipping {TEST_NAME}");
            return;
        }
        Err(e) => {
            drop_databases(&client, &names).await;
            panic!("{e}");
        }
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("HASURA_GRAPHQL_DATABASE_URL", with_database(&url, &main))
        .env("SRC2_URL", with_database(&url, &src2))
        .output();
    drop_databases(&client, &names).await;
    let output = output.expect("run scenario process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "scenario failed:\n{stdout}\n{stderr}"
    );
}

/// The remote schema of the recording: `type Query { user: User }`, `type User { id: Int! }`.
#[derive(async_graphql::SimpleObject)]
struct User {
    id: i32,
}

struct Query;

#[async_graphql::Object]
impl Query {
    async fn user(&self) -> Option<User> {
        Some(User { id: 1 })
    }
}

async fn start_remote() -> String {
    type RemoteSchema = async_graphql::Schema<
        Query,
        async_graphql::EmptyMutation,
        async_graphql::EmptySubscription,
    >;
    let schema: RemoteSchema = async_graphql::Schema::build(
        Query,
        async_graphql::EmptyMutation,
        async_graphql::EmptySubscription,
    )
    .finish();
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::post(
                |axum::extract::State(schema): axum::extract::State<RemoteSchema>,
                 axum::Json(req): axum::Json<async_graphql::Request>| async move {
                    axum::Json(schema.execute(req).await)
                },
            ),
        )
        .with_state(schema);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}/")
}

async fn replay(url: String) {
    let config = ServerConfig {
        database_url: Some(url),
        env_overrides: [("REMOTE_URL".to_owned(), start_remote().await)].into(),
        ..ServerConfig::default()
    };
    let engine = hge_server::build_engine(config).await.unwrap();
    let router = hge_server::routes::router(engine);

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
}

async fn post(router: &axum::Router, url: &str, body: &Value) -> (u16, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(url)
        .header("content-type", "application/json")
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

#[test]
fn database_urls() {
    assert_eq!(
        with_database("postgres://u:p@h:5432/hge_test", "x"),
        "postgres://u:p@h:5432/x"
    );
    assert_eq!(
        with_database("postgres://u:p@h/hge_test?sslmode=disable", "x"),
        "postgres://u:p@h/x?sslmode=disable"
    );
    assert_eq!(database_name("postgres://u:p@h:5432/hge_test"), "hge_test");
}
