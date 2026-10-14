//! `GET /v1/metrics`: V2 EE's Prometheus metrics, on with `metrics` in
//! `HASURA_GRAPHQL_ENABLED_APIS`. Needs the Postgres server `HASURA_GRAPHQL_DATABASE_URL`
//! points at (skipped without it). Metrics are process-wide, so this file has one test.

use std::path::Path;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hge_server::ServerConfig;
use serde_json::{Value, json};
use tower::ServiceExt;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/subscriptions");
const SECRET: &str = "s3cret";
const METRICS_SECRET: &str = "m3trics";

async fn engine(db: &str, apis: &[&str]) -> axum::Router {
    let engine = hge_server::build_engine(ServerConfig {
        database_url: Some(db.to_owned()),
        metadata_dir: Some(Path::new(FIXTURE).join("metadata")),
        admin_secrets: vec![SECRET.into()],
        enabled_apis: apis.iter().map(|a| (*a).to_owned()).collect(),
        metrics_secret: Some(METRICS_SECRET.into()),
        ..ServerConfig::default()
    })
    .await
    .unwrap();
    hge_server::routes::router(engine)
}

async fn graphql(router: &axum::Router, query: &str) {
    let req = Request::post("/v1/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-hasura-admin-secret", SECRET)
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
}

async fn get(router: &axum::Router, path: &str, auth: Option<&str>) -> (StatusCode, String) {
    let mut req = Request::get(path);
    if let Some(auth) = auth {
        req = req.header(header::AUTHORIZATION, auth);
    }
    let resp = router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// The value of the sample `name` (with its labels, exactly as rendered).
fn sample(metrics: &str, name: &str) -> f64 {
    metrics
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("no {name} in\n{metrics}"))
        .parse()
        .unwrap()
}

async fn scrape(router: &axum::Router) -> String {
    let (status, body) = get(
        router,
        "/v1/metrics",
        Some(&format!("Bearer {METRICS_SECRET}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn metrics() {
    let Some(db) = hge_testdb::TestDb::create("metrics").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping");
        return;
    };
    let (client, conn) = tokio_postgres::connect(db.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(Path::new(FIXTURE).join("schema.sql")).unwrap();
    client.batch_execute(&sql).await.unwrap();

    // Off by default: no route.
    let off = engine(db.url(), &["graphql", "metadata"]).await;
    assert_eq!(
        get(&off, "/v1/metrics", None).await.0,
        StatusCode::NOT_FOUND
    );

    let router = engine(db.url(), &["graphql", "metadata", "metrics"]).await;

    // The secret is required as a bearer token.
    let (status, body) = get(&router, "/v1/metrics", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["code"], "access-denied");
    let wrong = get(&router, "/v1/metrics", Some("Bearer nope")).await;
    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED);
    let resp = router
        .clone()
        .oneshot(
            Request::get("/v1/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {METRICS_SECRET}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "text/plain; version=0.0.4; charset=utf-8"
    );

    let before = scrape(&router).await;
    graphql(&router, "{ message { id } }").await;
    graphql(&router, "{ __typename }").await;
    graphql(&router, "{ __typename }").await; // answered from the cache
    graphql(
        &router,
        "mutation { delete_message(where: {id: {_eq: 0}}) { affected_rows } }",
    )
    .await;
    graphql(&router, "{ nope }").await;
    graphql(&router, "{ message {").await;
    let after = scrape(&router).await;
    let delta = |name: &str| sample(&after, name) - sample(&before, name);

    assert_eq!(
        delta(r#"hasura_graphql_requests_total{operation_type="query",response_status="success"}"#),
        3.0
    );
    assert_eq!(
        delta(
            r#"hasura_graphql_requests_total{operation_type="mutation",response_status="success"}"#
        ),
        1.0
    );
    // Parse and validation errors have no operation type.
    assert_eq!(
        delta(
            r#"hasura_graphql_requests_total{operation_type="unknown",response_status="failed"}"#
        ),
        2.0
    );
    assert_eq!(
        delta(r#"hasura_graphql_execution_time_seconds_count{operation_type="query"}"#),
        3.0
    );
    assert_eq!(
        delta(
            r#"hasura_graphql_execution_time_seconds_bucket{operation_type="mutation",le="+Inf"}"#
        ),
        1.0
    );
    assert!(after.contains("# TYPE hasura_graphql_execution_time_seconds histogram\n"));
    assert!(delta("hasura_http_request_bytes_total") > 0.0);
    assert!(delta("hasura_http_response_bytes_total") > 0.0);
    // The scrape itself is in flight.
    assert_eq!(sample(&after, "hasura_http_connections"), 1.0);

    // /healthz is left out of the byte counters.
    let before = scrape(&router).await;
    assert_eq!(get(&router, "/healthz", None).await.0, StatusCode::OK);
    let after = scrape(&router).await;
    let scrape_bytes = sample(&after, "hasura_http_response_bytes_total")
        - sample(&before, "hasura_http_response_bytes_total");
    assert_eq!(scrape_bytes, before.len() as f64);

    websocket(&router).await;
}

async fn websocket(router: &axum::Router) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router.clone();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let before = scrape(router).await;
    let mut req = format!("ws://{addr}/v1/graphql")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        "graphql-transport-ws".parse().unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut send = async |msg: Value, until: Value| {
        socket.send(Message::text(msg.to_string())).await.unwrap();
        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next());
            let Ok(Some(Ok(Message::Text(t)))) = next.await else {
                panic!("no {until} in time");
            };
            let v: Value = serde_json::from_str(&t).unwrap();
            if until.as_object().unwrap().iter().all(|(k, x)| v[k] == *x) {
                return;
            }
        }
    };
    send(
        json!({"type": "connection_init", "payload": {"headers": {"x-hasura-admin-secret": SECRET}}}),
        json!({"type": "connection_ack"}),
    )
    .await;
    send(
        json!({"type": "subscribe", "id": "1", "payload": {"query": "{ message { id } }"}}),
        json!({"type": "complete", "id": "1"}),
    )
    .await;
    send(
        json!({"type": "subscribe", "id": "2", "payload": {"query": "subscription { message { id } }"}}),
        json!({"type": "next", "id": "2"}),
    )
    .await;
    send(
        json!({"type": "subscribe", "id": "3", "payload": {"query": "subscription { message_feed(batch_size: 1, cursor: {initial_value: {id: 0}}) { id } }"}}),
        json!({"type": "next", "id": "3"}),
    )
    .await;
    let during = scrape(router).await;
    let delta = |name: &str| sample(&during, name) - sample(&before, name);
    assert_eq!(sample(&during, "hasura_websocket_connections"), 1.0);
    assert_eq!(
        sample(
            &during,
            r#"hasura_active_subscriptions{subscription_kind="live-query"}"#
        ),
        1.0
    );
    assert_eq!(
        sample(
            &during,
            r#"hasura_active_subscriptions{subscription_kind="streaming"}"#
        ),
        1.0
    );
    assert_eq!(
        delta(r#"hasura_graphql_requests_total{operation_type="query",response_status="success"}"#),
        1.0
    );
    assert_eq!(
        delta(
            r#"hasura_graphql_requests_total{operation_type="subscription",response_status="success"}"#
        ),
        2.0
    );
    assert!(delta("hasura_websocket_messages_received_bytes_total") > 0.0);
    assert!(delta("hasura_websocket_messages_sent_bytes_total") > 0.0);
    // WebSocket traffic is not HTTP traffic.
    assert_eq!(sample(&during, "hasura_http_connections"), 1.0);

    // Stopping a subscription and closing the connection bring the gauges back down.
    socket
        .send(Message::text(
            json!({"type": "complete", "id": "2"}).to_string(),
        ))
        .await
        .unwrap();
    socket.close(None).await.unwrap();
    drop(socket);
    let mut settled = String::new();
    for _ in 0..100 {
        settled = scrape(router).await;
        if sample(&settled, "hasura_websocket_connections") == 0.0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(sample(&settled, "hasura_websocket_connections"), 0.0);
    assert_eq!(
        sample(
            &settled,
            r#"hasura_active_subscriptions{subscription_kind="live-query"}"#
        ),
        0.0
    );
    assert_eq!(
        sample(
            &settled,
            r#"hasura_active_subscriptions{subscription_kind="streaming"}"#
        ),
        0.0
    );
}
