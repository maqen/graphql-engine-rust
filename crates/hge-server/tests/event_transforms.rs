//! Request and response transforms on event triggers, cron triggers and one-off events,
//! against what V2 did with the same requests (`event_transforms.json`, recorded by
//! `record/transforms_steps.py`): what the webhook received, the event states and the
//! invocation logs, normalised the same way (ids, times and the user agent are masked).
//!
//! Runs in a throwaway database on the Postgres server `HASURA_GRAPHQL_DATABASE_URL` points
//! at (see `hge-testdb`); without it the test is skipped.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::IntoResponse;
use hge_server::ServerConfig;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const RECORDING: &str = include_str!("event_transforms.json");
/// The webhook address in the recording.
const RECORDED_HOOK: &str = "http://127.0.0.1:9911";

type Calls = Arc<Mutex<Vec<Value>>>;

/// Answers like `record/hook.py`.
async fn hook(
    State(calls): State<Calls>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
    let headers: BTreeMap<String, String> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
        .collect();
    calls.lock().unwrap().push(json!({
        "method": method.as_str(),
        "path": path,
        "headers": headers,
        "body": String::from_utf8_lossy(&body),
    }));
    let (status, body) = if path.contains("fail") {
        (StatusCode::INTERNAL_SERVER_ERROR, r#"{"oops": 1}"#)
    } else {
        (StatusCode::OK, r#"{"hello": "world", "n": 3}"#)
    };
    (status, [("content-type", "application/json")], body)
}

/// `record/transforms_steps.py`'s `normalise`: the same four patterns, applied in order.
fn normalise(text: &str) -> String {
    fn is_hex(b: u8) -> bool {
        b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
    }
    fn uuid(s: &[u8], i: usize) -> Option<usize> {
        let shape = [8, 4, 4, 4, 12];
        let mut j = i;
        for (n, len) in shape.iter().enumerate() {
            if n > 0 {
                (s.get(j) == Some(&b'-')).then_some(())?;
                j += 1;
            }
            (s.get(j..j + len)?.iter().all(|b| is_hex(*b))).then_some(())?;
            j += len;
        }
        Some(j - i)
    }
    fn time(s: &[u8], i: usize) -> Option<usize> {
        let digits = |from: usize, n: usize| {
            s.get(from..from + n)
                .is_some_and(|d| d.iter().all(u8::is_ascii_digit))
        };
        let ok = digits(i, 4)
            && s.get(i + 4) == Some(&b'-')
            && digits(i + 5, 2)
            && s.get(i + 7) == Some(&b'-')
            && digits(i + 8, 2)
            && matches!(s.get(i + 10), Some(b'T' | b' '));
        if !ok {
            return None;
        }
        let mut j = i + 11;
        while s
            .get(j)
            .is_some_and(|b| b.is_ascii_digit() || *b == b':' || *b == b'.')
        {
            j += 1;
        }
        if j == i + 11 {
            return None;
        }
        if s.get(j) == Some(&b'Z') {
            j += 1;
        }
        Some(j - i)
    }
    fn hex(s: &[u8], i: usize) -> Option<usize> {
        if i > 0 && s[i - 1].is_ascii_alphanumeric() {
            return None;
        }
        let n = s[i..].iter().take_while(|b| is_hex(**b)).count();
        let next_ok = !s.get(i + n).is_some_and(u8::is_ascii_alphanumeric);
        ((n == 16 || n == 32) && next_ok).then_some(n)
    }
    fn user_agent(s: &[u8], i: usize) -> Option<usize> {
        const PREFIX: &[u8] = b"hasura-graphql-engine/";
        s[i..].starts_with(PREFIX).then(|| {
            PREFIX.len()
                + s[i + PREFIX.len()..]
                    .iter()
                    .take_while(|b| **b != b'"' && **b != b'\\')
                    .count()
        })
    }
    fn replace(text: String, matcher: fn(&[u8], usize) -> Option<usize>, with: &str) -> String {
        let s = text.as_bytes();
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            match matcher(s, i) {
                Some(n) => {
                    out.extend_from_slice(with.as_bytes());
                    i += n;
                }
                None => {
                    out.push(s[i]);
                    i += 1;
                }
            }
        }
        String::from_utf8(out).unwrap()
    }
    let text = replace(text.to_owned(), uuid, "<uuid>");
    let text = replace(text, time, "<time>");
    let text = replace(text, hex, "<hex>");
    replace(text, user_agent, "<user-agent>")
}

async fn send(router: &Router, step: &Value, hook: &str) -> (u16, Value) {
    let body = serde_json::to_string(&step["body"])
        .unwrap()
        .replace("__HOOK__", hook);
    let mut req =
        Request::post(step["url"].as_str().unwrap()).header("content-type", "application/json");
    for (k, v) in step["headers"].as_object().into_iter().flatten() {
        req = req.header(k, v.as_str().unwrap());
    }
    let resp = router
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
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

/// The observations of the recording, normalised.
async fn observe(router: &Router, recording: &Value, calls: &Calls, hook: &str) -> Value {
    let mut observed = serde_json::Map::new();
    for (name, sql) in recording["observe"].as_object().unwrap() {
        let step = json!({"url": "/v2/query", "body": {"type": "run_sql", "args": {"sql": sql}}});
        let (_, mut result) = send(router, &step, hook).await;
        let rows = match result["result"].as_array_mut() {
            Some(rows) => rows.split_off(1),
            None => vec![result],
        };
        observed.insert(name.clone(), Value::Array(rows));
    }
    let wanted: Vec<&str> = recording["webhook_headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap())
        .collect();
    let mut webhook: Vec<Value> = calls.lock().unwrap().clone();
    for call in &mut webhook {
        call["headers"]
            .as_object_mut()
            .unwrap()
            .retain(|k, _| wanted.contains(&k.as_str()));
    }
    webhook.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    observed.insert("webhook".into(), Value::Array(webhook));
    let text = Value::Object(observed)
        .to_string()
        .replace(hook, RECORDED_HOOK);
    serde_json::from_str(&normalise(&text)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn transforms_match_v2() {
    let Some(db) = hge_testdb::TestDb::create("event_transforms").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping");
        return;
    };
    let recording: Value = serde_json::from_str(RECORDING).unwrap();

    let calls: Calls = Arc::default();
    let app = Router::new().fallback(hook).with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut config = ServerConfig {
        database_url: Some(db.url().to_owned()),
        ..ServerConfig::default()
    };
    config.events.fetch_interval = Duration::from_millis(100);
    let engine = hge_server::build_engine(config).await.unwrap();
    let router = hge_server::routes::router(engine.clone());

    for (i, step) in recording["steps"].as_array().unwrap().iter().enumerate() {
        let (status, body) = send(&router, step, &hook_base).await;
        assert_eq!(status, step["status"], "step {i}: {body}");
    }
    // Started after the steps: the scheduled-event loop picks up due events when it starts.
    let shutdown = CancellationToken::new();
    let workers = [
        hge_server::events::spawn_worker(&engine, shutdown.clone()).unwrap(),
        hge_server::scheduled::spawn_worker(&engine, shutdown.clone()).unwrap(),
    ];

    let expected = &recording["observed"];
    let mut actual = Value::Null;
    for _ in 0..100 {
        actual = observe(&router, &recording, &calls, &hook_base).await;
        if &actual == expected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    shutdown.cancel();
    for w in workers {
        w.await.unwrap();
    }
    let mut differences = vec![];
    for (key, want) in expected.as_object().unwrap() {
        let got = &actual[key];
        if got != want {
            differences.push(format!(
                "{key}:\n  expected {}\n  actual   {}",
                serde_json::to_string_pretty(want).unwrap(),
                serde_json::to_string_pretty(got).unwrap()
            ));
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}
