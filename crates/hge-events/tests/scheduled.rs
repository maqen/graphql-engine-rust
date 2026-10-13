//! Cron triggers and one-off scheduled events against a real Postgres
//! (`HASURA_GRAPHQL_DATABASE_URL`) and a mock webhook. Skipped without a database.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use deadpool_postgres::{Pool, Runtime};
use hge_events::processor::EventingConfig;
use hge_events::scheduled::{self, CronTriggerConf, CronTriggerInfo, ScheduledTriggers};
use hge_events::webhook::HttpClient;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

async fn hook(
    State(calls): State<Calls>,
    Path(kind): Path<String>,
    body: String,
) -> impl IntoResponse {
    calls
        .lock()
        .unwrap()
        .push((kind.clone(), serde_json::from_str(&body).unwrap()));
    if kind == "fail" {
        (StatusCode::BAD_REQUEST, "bad").into_response()
    } else {
        (StatusCode::OK, "ok").into_response()
    }
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

#[tokio::test(flavor = "multi_thread")]
async fn scheduled_events_end_to_end() {
    let Ok(url) = std::env::var("HASURA_GRAPHQL_DATABASE_URL") else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping");
        return;
    };
    let pool = deadpool_postgres::Config {
        url: Some(url),
        ..Default::default()
    }
    .create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)
    .unwrap();
    pool.get()
        .await
        .unwrap()
        .batch_execute(
            "DROP TABLE IF EXISTS hdb_catalog.hdb_cron_event_invocation_logs, hdb_catalog.hdb_cron_events,
                                 hdb_catalog.hdb_scheduled_event_invocation_logs, hdb_catalog.hdb_scheduled_events CASCADE",
        )
        .await
        .unwrap();
    scheduled::ensure_catalog(&pool).await.unwrap();
    scheduled::ensure_catalog(&pool).await.unwrap();

    let calls: Calls = Arc::default();
    let app = Router::new()
        .route("/{kind}", post(hook))
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let env = BTreeMap::from([("HOOK".to_owned(), base.clone())]);

    // A cron trigger: the generator keeps 100 upcoming events.
    let conf: CronTriggerConf = serde_json::from_value(json!({
        "name": "every_minute", "webhook": "{{HOOK}}/ok", "schedule": "* * * * *",
        "include_in_metadata": true, "payload": {"k": "v"}, "comment": "c"
    }))
    .unwrap();
    let info = CronTriggerInfo::resolve(conf, &env).unwrap();
    let triggers = Arc::new(ScheduledTriggers {
        cron: HashMap::from([("every_minute".to_owned(), Arc::new(info))]),
    });
    scheduled::generate_cron_events(&pool, &triggers)
        .await
        .unwrap();
    scheduled::generate_cron_events(&pool, &triggers)
        .await
        .unwrap();
    let count = query(&pool, "SELECT count(*) AS n, min(scheduled_time) > now() AS future, \
        bool_and(extract(second FROM scheduled_time) = 0) AS whole FROM hdb_catalog.hdb_cron_events").await;
    assert_eq!(count, json!([{"n": 100, "future": true, "whole": true}]));

    // Reconciliation drops future events of unknown triggers and off-schedule ones.
    pool.get()
        .await
        .unwrap()
        .batch_execute(
            "INSERT INTO hdb_catalog.hdb_cron_events (trigger_name, scheduled_time) VALUES
               ('gone', now() + interval '1 hour'),
               ('every_minute', date_trunc('minute', now()) + interval '1 day 30 seconds')",
        )
        .await
        .unwrap();
    scheduled::reconcile_cron_events(&pool, &triggers)
        .await
        .unwrap();
    // (Checked by kind: if a minute boundary passed, the generator has added one more event.)
    let left = query(
        &pool,
        "SELECT count(*) FILTER (WHERE trigger_name = 'gone') AS gone, \
         count(*) FILTER (WHERE extract(second FROM scheduled_time) <> 0) AS off_schedule, \
         count(*) >= 100 AS kept FROM hdb_catalog.hdb_cron_events",
    )
    .await;
    assert_eq!(left, json!([{"gone": 0, "off_schedule": 0, "kept": true}]));

    // A due cron event, plus one-off events: ok, failing with one retry, too late, bad env.
    pool.get()
        .await
        .unwrap()
        .batch_execute(&format!(
            r#"INSERT INTO hdb_catalog.hdb_cron_events (id, trigger_name, scheduled_time)
                 VALUES ('cron-due', 'every_minute', now() - interval '5 seconds');
               INSERT INTO hdb_catalog.hdb_scheduled_events (id, webhook_conf, scheduled_time, payload, retry_conf, header_conf, comment) VALUES
                 ('one-ok', '"{{{{HOOK}}}}/ok"', now() - interval '1 second', '{{"b":1,"a":2}}', NULL, '[]', 'hi'),
                 ('one-fail', '"{base}/fail"', now() - interval '1 second', NULL, '{{"num_retries":1,"retry_interval_seconds":1,"timeout_seconds":5,"tolerance_seconds":60}}', '[]', NULL),
                 ('one-late', '"{base}/ok"', now() - interval '2 hours', NULL, '{{"num_retries":0,"retry_interval_seconds":1,"timeout_seconds":5,"tolerance_seconds":60}}', '[]', NULL),
                 ('one-env', '"{{{{NOPE}}}}/x"', now() - interval '1 second', NULL, NULL, '[]', NULL);"#
        ))
        .await
        .unwrap();

    let shutdown = CancellationToken::new();
    let provider_triggers = triggers.clone();
    let handle = scheduled::spawn(
        EventingConfig::default(),
        pool.clone(),
        Arc::new(HttpClient::new().unwrap()),
        Arc::new(move || provider_triggers.clone()),
        Arc::new(env),
        shutdown.clone(),
    );
    // First iteration runs at once; the failed one-off is retried on the next (10 s later).
    let mut done = false;
    for _ in 0..150 {
        let s = query(&pool, "SELECT id, status FROM hdb_catalog.hdb_scheduled_events WHERE status <> 'scheduled' AND status <> 'locked'").await;
        if s.as_array().unwrap().len() == 4 {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    shutdown.cancel();
    handle.await.unwrap();
    assert!(done, "scheduled events did not finish");

    let status = query(
        &pool,
        "SELECT id, status, tries FROM hdb_catalog.hdb_scheduled_events ORDER BY id",
    )
    .await;
    assert_eq!(
        status,
        json!([
            {"id": "one-env", "status": "error", "tries": 1},
            {"id": "one-fail", "status": "error", "tries": 2},
            {"id": "one-late", "status": "dead", "tries": 0},
            {"id": "one-ok", "status": "delivered", "tries": 1}
        ])
    );
    let cron = query(
        &pool,
        "SELECT status, tries FROM hdb_catalog.hdb_cron_events WHERE id = 'cron-due'",
    )
    .await;
    assert_eq!(cron, json!([{"status": "delivered", "tries": 1}]));

    let calls = calls.lock().unwrap().clone();
    let cron_body = &calls.iter().find(|c| c.1["id"] == "cron-due").unwrap().1;
    assert_eq!(cron_body["name"], "every_minute");
    assert_eq!(cron_body["payload"], json!({"k": "v"}));
    assert_eq!(cron_body["comment"], "c");
    assert!(cron_body["scheduled_time"].as_str().unwrap().ends_with('Z'));
    let one = &calls.iter().find(|c| c.1["id"] == "one-ok").unwrap().1;
    assert_eq!(one["payload"], json!({"a": 2, "b": 1}));
    assert_eq!(one["comment"], "hi");
    assert!(one["created_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(calls.iter().filter(|c| c.0 == "fail").count(), 2);

    let env_log = query(&pool, "SELECT status, request, response FROM hdb_catalog.hdb_scheduled_event_invocation_logs WHERE event_id = 'one-env'").await;
    assert_eq!(
        env_log,
        json!([{
            "status": 500,
            "request": {"headers": [], "payload": {"error": "Error creating the request. The value for environment variables not found: NOPE"}, "version": "1"},
            "response": {"data": {"message": "\"The value for environment variables not found: NOPE\""}, "type": "client_error", "version": "1"}
        }])
    );
}
