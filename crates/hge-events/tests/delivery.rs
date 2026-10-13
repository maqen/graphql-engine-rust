//! Event triggers end to end against a real Postgres (`HASURA_GRAPHQL_DATABASE_URL`) and a
//! mock webhook. Skipped when no database is configured.

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use deadpool_postgres::{Pool, Runtime};
use hge_core::QualifiedTable;
use hge_events::EventTriggerConf;
use hge_events::catalog::{self, CatalogInit};
use hge_events::ddl::TriggerColumn;
use hge_events::processor::{self, EventingConfig};
use hge_events::triggers::{EventTriggerInfo, EventTriggers, SourceTriggers};
use hge_events::webhook::HttpClient;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct Received {
    calls: Arc<Mutex<Vec<(String, HeaderMap, Value)>>>,
}

/// `/ok` → 200, `/fail` → 500, `/retry-after` → 503 with `Retry-After: 1`, `/slow` sleeps 3 s.
async fn hook(
    State(r): State<Received>,
    Path(kind): Path<String>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let v: Value = serde_json::from_str(&body).unwrap();
    r.calls.lock().unwrap().push((kind.clone(), headers, v));
    match kind.as_str() {
        "fail" => (StatusCode::INTERNAL_SERVER_ERROR, [("x-a", "b")], "boom").into_response(),
        "retry-after" => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("retry-after", "1")],
            "later",
        )
            .into_response(),
        "slow" => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            (StatusCode::OK, "late").into_response()
        }
        _ => (StatusCode::OK, [("x-a", "b")], "{\"ok\":true}").into_response(),
    }
}

async fn start_hook() -> (SocketAddr, Received) {
    let received = Received::default();
    let app = Router::new()
        .route("/{kind}", post(hook))
        .with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, received)
}

fn pool(url: &str) -> Pool {
    let cfg = deadpool_postgres::Config {
        url: Some(url.to_owned()),
        ..Default::default()
    };
    cfg.create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)
        .unwrap()
}

async fn exec(pool: &Pool, sql: &str) {
    pool.get().await.unwrap().batch_execute(sql).await.unwrap();
}

async fn query(pool: &Pool, sql: &str) -> Value {
    let row = pool
        .get()
        .await
        .unwrap()
        .query_one(
            &format!("SELECT coalesce(json_agg(t), '[]')::text FROM ({sql}) t"),
            &[],
        )
        .await
        .unwrap();
    serde_json::from_str(row.get::<_, &str>(0)).unwrap()
}

fn trigger(
    name: &str,
    table: &QualifiedTable,
    path: &str,
    retry: Value,
    addr: SocketAddr,
) -> EventTriggerInfo {
    let conf: EventTriggerConf = serde_json::from_value(json!({
        "name": name,
        "definition": {
            "enable_manual": false,
            "insert": {"columns": "*"},
            "update": {"columns": ["name"], "payload": ["id", "name"]},
            "delete": {"columns": "*"}
        },
        "retry_conf": retry,
        "webhook": format!("http://{addr}/{path}"),
        "headers": [{"name": "X-Static", "value": "s"}, {"name": "X-Env", "value_from_env": "K"}]
    }))
    .unwrap();
    let env = BTreeMap::from([("K".to_owned(), "secret".to_owned())]);
    EventTriggerInfo::resolve(table, conf, &env).unwrap()
}

fn columns() -> Vec<TriggerColumn> {
    [
        ("id", "int4"),
        ("name", "text"),
        ("big", "int8"),
        ("price", "numeric"),
    ]
    .into_iter()
    .map(|(n, t)| TriggerColumn {
        name: n.into(),
        type_name: t.into(),
    })
    .collect()
}

async fn wait_for(pool: &Pool, sql: &str, want: usize) -> Value {
    for _ in 0..100 {
        let rows = query(pool, sql).await;
        if rows.as_array().unwrap().len() >= want {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {want} rows of: {sql}");
}

#[tokio::test(flavor = "multi_thread")]
async fn event_triggers_end_to_end() {
    let Some(db) = hge_testdb::TestDb::create("delivery").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping");
        return;
    };
    let url = db.url().to_owned();
    let pool = pool(&url);
    exec(
        &pool,
        "DROP TABLE IF EXISTS public.et_items, public.et_fails, public.et_slow CASCADE;
         DROP SCHEMA IF EXISTS hdb_catalog CASCADE;
         CREATE TABLE public.et_items (id serial primary key, name text, big bigint, price numeric);
         CREATE TABLE public.et_fails (id serial primary key, name text, big bigint, price numeric);
         CREATE TABLE public.et_slow (id serial primary key, name text, big bigint, price numeric);",
    )
    .await;
    assert_eq!(
        catalog::init_source_catalog(&pool, "public").await.unwrap(),
        CatalogInit::Created
    );
    assert_eq!(
        catalog::init_source_catalog(&pool, "public").await.unwrap(),
        CatalogInit::UpToDate
    );

    let (addr, received) = start_hook().await;
    let items = QualifiedTable::public("et_items");
    let fails = QualifiedTable::public("et_fails");
    let slow = QualifiedTable::public("et_slow");
    let t_ok = trigger(
        "items_ev",
        &items,
        "ok",
        json!({"num_retries": 0, "interval_sec": 1}),
        addr,
    );
    let t_fail = trigger(
        "fails_ev",
        &fails,
        "fail",
        json!({"num_retries": 2, "interval_sec": 1, "timeout_sec": 5}),
        addr,
    );
    let t_slow = trigger(
        "slow_ev",
        &slow,
        "slow",
        json!({"num_retries": 0, "interval_sec": 1, "timeout_sec": 1}),
        addr,
    );
    let ddl: Vec<_> = [&t_ok, &t_fail, &t_slow]
        .iter()
        .map(|t| t.ddl(columns(), false))
        .collect();
    let keep: HashSet<String> = HashSet::new();
    let report = catalog::sync_triggers(&pool, &ddl, &keep).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.functions_written, 9);
    assert_eq!(report.triggers_created, 9);
    // Idempotent: nothing to do the second time.
    let again = catalog::sync_triggers(&pool, &ddl, &keep).await.unwrap();
    assert_eq!((again.functions_written, again.triggers_created), (0, 0));

    // Writes with and without session variables.
    exec(
        &pool,
        r#"BEGIN;
           SET LOCAL "hasura.user" = '{"x-hasura-role":"user","x-hasura-user-id":"7"}';
           INSERT INTO public.et_items (name, big, price) VALUES ('a', 9007199254740993, 1.50);
           COMMIT;
           UPDATE public.et_items SET big = 1;            -- not a listened column: no event
           UPDATE public.et_items SET name = 'b';
           DELETE FROM public.et_items;
           INSERT INTO public.et_fails (name) VALUES ('x');
           INSERT INTO public.et_slow (name) VALUES ('x');"#,
    )
    .await;
    let events = query(&pool, "SELECT trigger_name, payload->>'op' AS op FROM hdb_catalog.event_log ORDER BY created_at, payload->>'op'").await;
    assert_eq!(events.as_array().unwrap().len(), 5, "{events}");

    let mut triggers = std::collections::HashMap::new();
    for t in [t_ok, t_fail, t_slow] {
        triggers.insert(t.name.clone(), Arc::new(t));
    }
    let snapshot = Arc::new(EventTriggers {
        sources: vec![Arc::new(SourceTriggers {
            source: "default".into(),
            pool: pool.clone(),
            triggers,
        })],
    });
    let config = EventingConfig {
        fetch_interval: Duration::from_millis(100),
        user_agent: "hasura-graphql-engine/test".into(),
        ..EventingConfig::default()
    };
    let shutdown = CancellationToken::new();
    let handle = processor::spawn(
        config,
        Arc::new(HttpClient::new().unwrap()),
        Arc::new(move || snapshot.clone()),
        shutdown.clone(),
    );

    // items: 3 deliveries; fails: 1 + 2 retries; slow: 1 attempt that times out.
    let logs = wait_for(
        &pool,
        "SELECT trigger_name, status, request, response FROM hdb_catalog.event_invocation_logs ORDER BY created_at",
        7,
    )
    .await;
    // Give the last bookkeeping a moment, then stop.
    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown.cancel();
    handle.await.unwrap();

    let calls = received.calls.lock().unwrap().clone();
    let ok_calls: Vec<_> = calls.iter().filter(|c| c.0 == "ok").collect();
    assert_eq!(ok_calls.len(), 3);
    let insert = ok_calls
        .iter()
        .find(|c| c.2["event"]["op"] == "INSERT")
        .unwrap();
    let body = &insert.2;
    assert_eq!(
        body["delivery_info"],
        json!({"current_retry": 0, "max_retries": 0})
    );
    assert_eq!(
        body["table"],
        json!({"name": "et_items", "schema": "public"})
    );
    assert_eq!(body["trigger"], json!({"name": "items_ev"}));
    assert_eq!(
        body["event"]["session_variables"],
        json!({"x-hasura-role": "user", "x-hasura-user-id": "7"})
    );
    assert_eq!(body["event"]["data"]["old"], Value::Null);
    assert_eq!(body["event"]["data"]["new"]["name"], "a");
    assert!(body["created_at"].as_str().unwrap().len() >= 19);
    let headers = &insert.1;
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["user-agent"], "hasura-graphql-engine/test");
    assert_eq!(headers["x-static"], "s");
    assert_eq!(headers["x-env"], "secret");
    assert_eq!(headers["x-b3-sampled"], "1");

    // The raw stored payload keeps big numbers exact and is re-encoded with sorted keys.
    let raw: Vec<_> = logs
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| {
            l["trigger_name"] == "items_ev" && l["request"]["payload"]["event"]["op"] == "INSERT"
        })
        .collect();
    assert_eq!(raw.len(), 1);
    let raw_text = query(
        &pool,
        "SELECT request::text AS r FROM hdb_catalog.event_invocation_logs WHERE trigger_name = 'items_ev' AND request::text LIKE '%\"INSERT\"%'",
    )
    .await;
    let text = raw_text[0]["r"].as_str().unwrap();
    assert!(
        text.contains(r#""new":{"big":9007199254740993,"id":1,"name":"a","price":1.5}"#),
        "{text}"
    );
    assert!(
        text.starts_with(r#"{"headers":[{"name":"Content-Type","value":"application/json"}"#),
        "{text}"
    );
    assert!(text.contains(r#"{"name":"X-Env","value_from_env":"K"}"#));
    assert_eq!(raw[0]["status"], 200);
    assert_eq!(raw[0]["response"]["type"], "webhook_response");
    assert_eq!(raw[0]["response"]["data"]["body"], "{\"ok\":true}");

    // UPDATE payload only has the payload columns.
    let update = ok_calls
        .iter()
        .find(|c| c.2["event"]["op"] == "UPDATE")
        .unwrap();
    assert_eq!(
        update.2["event"]["data"],
        json!({"old": {"id": 1, "name": "a"}, "new": {"id": 1, "name": "b"}})
    );
    assert_eq!(update.2["event"]["session_variables"], Value::Null);

    // Failing webhook: 3 attempts with current_retry 0, 1, 2, then error.
    let fail_retries: Vec<_> = calls
        .iter()
        .filter(|c| c.0 == "fail")
        .map(|c| c.2["delivery_info"]["current_retry"].clone())
        .collect();
    assert_eq!(fail_retries, vec![json!(0), json!(1), json!(2)]);
    let fail_logs: Vec<_> = logs
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["trigger_name"] == "fails_ev")
        .collect();
    assert_eq!(fail_logs[0]["status"], 500);
    assert_eq!(
        fail_logs[0]["response"],
        json!({"data": {"message": "boom"}, "type": "client_error", "version": "2"})
    );

    // Timed-out delivery. The HTTP timeout and the processing bound are both `timeout_sec`
    // and race, as in V2: either no response (status NULL, http_exception) or the engine's
    // "timed out while processing" (status 500).
    let slow_log: Vec<_> = logs
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["trigger_name"] == "slow_ev")
        .collect();
    assert_eq!(slow_log.len(), 1);
    let msg = slow_log[0]["response"]["data"]["message"].as_str().unwrap();
    match &slow_log[0]["status"] {
        Value::Null => assert!(msg.contains("http_exception"), "{msg}"),
        status => {
            assert_eq!(status, 500);
            assert!(msg.contains("timed out while processing"), "{msg}");
        }
    }

    let state = query(
        &pool,
        "SELECT trigger_name, bool_and(delivered) AS delivered, bool_and(error) AS error, max(tries) AS tries, bool_or(locked IS NOT NULL) AS locked
         FROM hdb_catalog.event_log GROUP BY trigger_name ORDER BY trigger_name",
    )
    .await;
    assert_eq!(
        state,
        json!([
            {"trigger_name": "fails_ev", "delivered": false, "error": true, "tries": 3, "locked": false},
            {"trigger_name": "items_ev", "delivered": true, "error": false, "tries": 1, "locked": false},
            {"trigger_name": "slow_ev", "delivered": false, "error": true, "tries": 1, "locked": false}
        ])
    );

    // Dropping a trigger removes its SQL objects and archives its events.
    catalog::drop_trigger(&pool, "items_ev", true)
        .await
        .unwrap();
    let left = query(
        &pool,
        "SELECT proname FROM pg_proc WHERE proname LIKE 'notify_hasura_items_ev_%'",
    )
    .await;
    assert_eq!(left, json!([]));
    let archived = query(
        &pool,
        "SELECT DISTINCT archived FROM hdb_catalog.event_log WHERE trigger_name = 'items_ev'",
    )
    .await;
    assert_eq!(archived, json!([{"archived": true}]));

    // Moving a trigger to another table drops the SQL triggers left on the old one; a trigger
    // still named in metadata (inconsistent, so not installed) keeps its objects.
    let moved = trigger(
        "fails_ev",
        &slow,
        "fail",
        json!({"num_retries": 0, "interval_sec": 1}),
        addr,
    );
    let ddl = vec![moved.ddl(columns(), false)];
    let keep: HashSet<String> = ["fails_ev", "slow_ev"].map(String::from).into();
    let report = catalog::sync_triggers(&pool, &ddl, &keep).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!((report.triggers_dropped, report.triggers_created), (3, 3));
    assert!(report.removed.is_empty());
    let installed = || {
        query(
            &pool,
            "SELECT tgname, tgrelid::regclass::text AS tbl FROM pg_trigger
             WHERE tgname LIKE 'notify_hasura_%' ORDER BY 1, 2",
        )
    };
    let rows = installed().await;
    let rows: Vec<String> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{} {}",
                r["tgname"].as_str().unwrap(),
                r["tbl"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            "notify_hasura_fails_ev_DELETE et_slow",
            "notify_hasura_fails_ev_INSERT et_slow",
            "notify_hasura_fails_ev_UPDATE et_slow",
            "notify_hasura_slow_ev_DELETE et_slow",
            "notify_hasura_slow_ev_INSERT et_slow",
            "notify_hasura_slow_ev_UPDATE et_slow",
        ]
    );

    // A trigger gone from metadata loses its objects and its events are archived.
    let keep: HashSet<String> = ["fails_ev"].map(String::from).into();
    let report = catalog::sync_triggers(&pool, &ddl, &keep).await.unwrap();
    assert_eq!(report.removed, ["slow_ev"]);
    assert_eq!(report.functions_dropped, 3);
    let left = query(
        &pool,
        "SELECT proname FROM pg_proc WHERE proname LIKE 'notify_hasura_slow_ev_%'",
    )
    .await;
    assert_eq!(left, json!([]));
    let archived = query(
        &pool,
        "SELECT DISTINCT archived FROM hdb_catalog.event_log WHERE trigger_name = 'slow_ev'",
    )
    .await;
    assert_eq!(archived, json!([{"archived": true}]));
    // Nothing left to do afterwards, including with no triggers to install.
    let again = catalog::sync_triggers(&pool, &ddl, &keep).await.unwrap();
    assert_eq!(
        (
            again.triggers_dropped,
            again.functions_dropped,
            again.functions_written
        ),
        (0, 0, 0)
    );
    let empty = catalog::sync_triggers(&pool, &[], &keep).await.unwrap();
    assert!(empty.removed.is_empty());
}
