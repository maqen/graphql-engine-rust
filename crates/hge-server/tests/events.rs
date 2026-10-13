//! Event trigger conformance cases (`tests/events_cases/*.yaml`) against a real Postgres
//! (`HASURA_GRAPHQL_DATABASE_URL`) and an in-process webhook. Skipped without a database.
//!
//! The engine boots from `tests/fixtures/events/metadata` (with the webhook address filled
//! in), so this also covers catalog set-up and SQL trigger installation on startup.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use hge_server::ServerConfig;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/events");
const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/events_cases");
const VOLATILE: [&str; 3] = ["id", "created_at", "trace_context"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    description: String,
    sql: String,
    deliveries: Vec<Delivery>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delivery {
    path: String,
    body: Value,
}

type Calls = Arc<Mutex<Vec<(String, HeaderMap, String)>>>;

async fn hook(
    State(calls): State<Calls>,
    UrlPath(path): UrlPath<String>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    calls.lock().unwrap().push((path.clone(), headers, body));
    if path == "fail" {
        (StatusCode::INTERNAL_SERVER_ERROR, "failed").into_response()
    } else {
        (StatusCode::OK, "ok").into_response()
    }
}

fn strip(v: &mut Value) {
    if let Value::Object(m) = v {
        for k in VOLATILE {
            m.remove(k);
        }
        if let Some(e) = m.get_mut("event").and_then(Value::as_object_mut) {
            e.remove("trace_context");
        }
    }
}

/// Copy the fixture metadata, pointing webhooks at `base`.
fn metadata_dir(base: &str) -> PathBuf {
    let dst = std::env::temp_dir().join(format!("hge-events-metadata-{}", std::process::id()));
    let src = Path::new(FIXTURE).join("metadata");
    fn copy(src: &Path, dst: &Path, base: &str) {
        std::fs::create_dir_all(dst).unwrap();
        for e in std::fs::read_dir(src).unwrap() {
            let e = e.unwrap();
            let to = dst.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy(&e.path(), &to, base);
            } else {
                let text = std::fs::read_to_string(e.path()).unwrap();
                std::fs::write(to, text.replace("__WEBHOOK__", base)).unwrap();
            }
        }
    }
    copy(&src, &dst, base);
    dst
}

#[tokio::test(flavor = "multi_thread")]
async fn event_trigger_cases() {
    let Some(db) = hge_testdb::TestDb::create("events").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping event trigger tests");
        return;
    };
    let url = db.url().to_owned();
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    client
        .batch_execute(&std::fs::read_to_string(Path::new(FIXTURE).join("schema.sql")).unwrap())
        .await
        .unwrap();

    let calls: Calls = Arc::default();
    let app = Router::new()
        .route("/{path}", post(hook))
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut config = ServerConfig {
        database_url: Some(url.clone()),
        metadata_dir: Some(metadata_dir(&base)),
        ..ServerConfig::default()
    };
    config.events.fetch_interval = Duration::from_millis(100);
    let engine = hge_server::build_engine(config)
        .await
        .expect("build engine");

    // The trigger with an unresolvable webhook is inconsistent and gets no SQL trigger.
    let inconsistencies: Vec<String> = engine
        .state
        .load()
        .inconsistencies
        .iter()
        .map(|o| o.reason.clone())
        .collect();
    assert_eq!(
        inconsistencies,
        vec![
            "in table \"ev.broken\": in event trigger \"broken_events\": environment variable 'HGE_TEST_UNSET_WEBHOOK_VARIABLE' not set"
                .to_owned()
        ]
    );
    let functions: Vec<String> = client
        .query(
            "SELECT proname::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE nspname = 'hdb_catalog' AND proname LIKE 'notify_hasura_%' ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        functions,
        [
            "notify_hasura_flaky-events_INSERT",
            "notify_hasura_item_events_DELETE",
            "notify_hasura_item_events_INSERT",
            "notify_hasura_item_events_UPDATE",
        ]
    );

    let shutdown = CancellationToken::new();
    let worker = hge_server::events::spawn_worker(&engine, shutdown.clone()).expect("worker");

    let mut files: Vec<PathBuf> = std::fs::read_dir(CASES)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "yaml"))
        .collect();
    files.sort();
    let mut failures = vec![];
    for file in files {
        let cases: CaseFile =
            serde_yaml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        for case in cases.cases {
            calls.lock().unwrap().clear();
            client.batch_execute(&case.sql).await.unwrap();
            let want = case.deliveries.len();
            let mut got = vec![];
            for _ in 0..100 {
                got = calls.lock().unwrap().clone();
                if got.len() >= want {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            // Let late or duplicate deliveries show up.
            tokio::time::sleep(Duration::from_millis(300)).await;
            got = calls.lock().unwrap().clone();
            let actual: Vec<(String, Value)> = got
                .iter()
                .map(|(p, _, b)| {
                    let mut v: Value = serde_json::from_str(b).unwrap();
                    strip(&mut v);
                    (p.clone(), v)
                })
                .collect();
            let expected: Vec<(String, Value)> = case
                .deliveries
                .into_iter()
                .map(|d| (d.path, d.body))
                .collect();
            if actual != expected {
                failures.push(format!(
                    "{}: {}\n  expected: {expected:?}\n  actual:   {actual:?}",
                    file.file_name().unwrap().to_string_lossy(),
                    case.description
                ));
            }
            // Bodies are sent with sorted keys and V2's headers.
            for (_, headers, body) in &got {
                assert!(body.starts_with("{\"created_at\":"), "{body}");
                assert_eq!(headers["content-type"], "application/json");
                assert!(
                    headers["user-agent"]
                        .to_str()
                        .unwrap()
                        .starts_with("hasura-graphql-engine/")
                );
            }
        }
    }
    shutdown.cancel();
    worker.await.unwrap();
    assert!(
        failures.is_empty(),
        "{} case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
