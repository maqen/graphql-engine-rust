//! Replaying metadata API requests against responses recorded from Hasura V2 (see
//! `record/record.py`).
//!
//! Steps are replayed in order and every response must match exactly, key order included.
//! Ids the server generates (uuids, trace ids) are compared as placeholders; ids a scenario
//! assigns itself through `run_sql` start with one of its `fixed_ids` prefixes and are
//! compared as they are. Steps marked `todo` are known differences: they must still differ,
//! so fixing one means removing its marker.

use axum::body::Body;
use axum::http::Request;
use hge_server::ServerConfig;
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;

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

/// A server on a throwaway database (see `hge-testdb`; keep the returned guard alive for the
/// test), or `None` to skip the test.
pub async fn router(label: &str) -> Option<(hge_testdb::TestDb, axum::Router)> {
    let Some(db) = hge_testdb::TestDb::create(label).await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping end-to-end tests");
        return None;
    };
    let config = ServerConfig {
        database_url: Some(db.url().to_owned()),
        ..ServerConfig::default()
    };
    let engine = hge_server::build_engine(config).await.unwrap();
    Some((db, hge_server::routes::router(engine)))
}

pub async fn post(router: &axum::Router, url: &str, body: &Value) -> (u16, Value) {
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

/// Replay `scenario` (the recorded JSON) and fail with every mismatch.
pub async fn replay(router: &axum::Router, scenario: &str, fixed_ids: &[&str]) {
    let scenario: Scenario = serde_json::from_str(scenario).unwrap();
    let mut failures = vec![];
    for (i, step) in scenario.steps.iter().enumerate() {
        let (status, actual) = post(router, &step.url, &step.query).await;
        let normalise = |v: &Value| normalise(v, fixed_ids);
        let (actual, expected) = (normalise(&actual), normalise(&step.response));
        // Compare the text so key order counts (`Value` equality ignores it).
        let text = |v: &Value| serde_json::to_string(v).unwrap();
        let matches = status == step.status && text(&actual) == text(&expected);
        match (&step.todo, matches) {
            (None, true) | (Some(_), false) => {}
            (None, false) => failures.push(format!(
                "step {i} {} {}\n  expected ({}): {expected}\n  actual   ({status}): {actual}",
                step.url, step.query, step.status,
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

/// Replace values the server generates (uuids, trace ids) with placeholders.
fn normalise(v: &Value, fixed_ids: &[&str]) -> Value {
    match v {
        Value::String(s) if is_uuid(s) && !fixed_ids.iter().any(|p| s.starts_with(p)) => {
            Value::String("<uuid>".into())
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| normalise(v, fixed_ids)).collect())
        }
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| {
                    let v = match (k.as_str(), v) {
                        ("trace_id" | "span_id", Value::String(_)) => Value::String("<id>".into()),
                        _ => normalise(v, fixed_ids),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
        })
}
