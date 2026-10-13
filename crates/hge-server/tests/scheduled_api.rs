//! The cron trigger and scheduled event metadata API replayed against responses recorded
//! from Hasura V2.
//!
//! `scheduled_api.json` was recorded from `hasura/graphql-engine:v2.51.0-ce` on a fresh
//! database (steps in `record/scheduled_api_steps.py`): creating, replacing and deleting
//! cron triggers with every argument error V2 reports, the events they generate, one-off
//! events, and the event and invocation listings. All events are in the future and nothing
//! delivers them; rows are given fixed ids and timestamps through `run_sql` so responses are
//! deterministic. See `replay/mod.rs` for how steps are compared.
//!
//! The test runs in a throwaway database on the Postgres server `HASURA_GRAPHQL_DATABASE_URL`
//! points at (see `hge-testdb`); without it the test is skipped.

mod replay;

use replay::post;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn scheduled_event_api_matches_v2() {
    let Some((_db, router)) = replay::router("scheduled_api").await else {
        return;
    };
    replay::replay(
        &router,
        include_str!("scheduled_api.json"),
        &[
            "20000000-0000-0000-0000-",
            "30000000-0000-0000-0000-",
            "40000000-0000-0000-0000-",
            "50000000-0000-0000-0000-",
        ],
    )
    .await;
    rejected_replace_keeps_events(&router).await;
}

/// Our own guarantee (V2 does not give it): cron events change only when the metadata
/// change is committed, so a rejected replace leaves the old schedule's events alone.
async fn rejected_replace_keeps_events(router: &axum::Router) {
    let cron = |schedule: &str, replace: bool| {
        json!({"type": "create_cron_trigger", "args": {"name": "c9",
            "webhook": "http://127.0.0.1:1/hook", "schedule": schedule,
            "include_in_metadata": true, "replace": replace}})
    };
    let (status, body) = post(router, "/v1/metadata", &cron("0 0 1 1 *", false)).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        router,
        "/v1/metadata",
        &json!({"type": "bulk", "args": [
            cron("* * * * *", true),
            {"type": "delete_cron_trigger", "args": {"name": "nope"}},
        ]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (_, rows) = post(
        router,
        "/v2/query",
        &json!({"type": "run_sql", "args": {"sql":
            "SELECT count(*), count(*) FILTER (WHERE scheduled_time < now() + interval '1 day') \
             FROM hdb_catalog.hdb_cron_events WHERE trigger_name = 'c9'"}}),
    )
    .await;
    assert_eq!(rows["result"][1], json!(["100", "0"]), "{rows}");
}
