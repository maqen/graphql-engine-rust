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
}
