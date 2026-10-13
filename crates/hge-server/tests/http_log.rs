//! `http-log`: one V2-shaped log per HTTP request (spec 11 §5.5). No database needed.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, header};
use hge_server::ServerConfig;
use hge_server::telemetry::{self, Level, LogConfig, LogTypes};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn send(router: &axum::Router, req: Request<Body>) -> axum::http::Response<Body> {
    let resp = router.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    // The log is written once the body has been sent.
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    axum::http::Response::from_parts(parts, Body::from(bytes))
}

fn http_logs(lines: &Arc<Mutex<Vec<String>>>) -> Vec<Value> {
    let mut lines = lines.lock().unwrap();
    let logs = lines
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).expect("JSON line"))
        .filter(|l| l["type"] == "http-log")
        .collect();
    lines.clear();
    logs
}

fn post(path: &str, body: Value) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-forwarded-for", "1.2.3.4, 10.0.0.1")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn http_log() {
    let lines = telemetry::init_capture(LogConfig {
        level: Level::Info,
        types: LogTypes::DEFAULT,
    });
    let engine = hge_server::build_engine(ServerConfig {
        admin_secrets: vec!["secret".into()],
        ..ServerConfig::default()
    })
    .await
    .unwrap();
    let router = hge_server::routes::router(engine);
    http_logs(&lines);

    // A successful GraphQL request.
    let mut req = post("/v1/graphql", json!({"query": "{ __typename }"}));
    req.headers_mut()
        .insert("x-hasura-admin-secret", "secret".parse().unwrap());
    req.headers_mut()
        .insert("x-request-id", "rid-1".parse().unwrap());
    let resp = send(&router, req).await;
    assert_eq!(resp.headers()["x-request-id"], "rid-1");
    let logs = http_logs(&lines);
    assert_eq!(logs.len(), 1, "{logs:?}");
    let log = &logs[0];
    assert_eq!(log["level"], "info");
    let detail = &log["detail"];
    assert_eq!(
        detail["http_info"],
        json!({"status": 200, "method": "POST", "ip": "1.2.3.4, 10.0.0.1", "url": "/v1/graphql", "http_version": "HTTP/1.1", "content_encoding": null})
    );
    let op = &detail["operation"];
    assert_eq!(op["request_id"], "rid-1");
    assert_eq!(detail["request_id"], "rid-1");
    assert_eq!(op["user_vars"], json!({"x-hasura-role": "admin"}));
    assert_eq!(op["query"], json!({"type": null}));
    assert_eq!(op["request_mode"], "single");
    assert_eq!(op["response_size"], op["uncompressed_response_size"]);
    assert!(op["response_size"].as_u64().unwrap() > 0);
    assert!(op["query_execution_time"].is_f64());
    assert!(op["request_read_time"].is_f64());
    assert!(op.get("error").is_none());
    let keys: Vec<&str> = op.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "request_id",
            "user_vars",
            "response_size",
            "uncompressed_response_size",
            "request_read_time",
            "query_execution_time",
            "query",
            "request_mode"
        ]
    );

    // A GraphQL error is logged at error level with the error, still with status 200.
    let mut req = post("/v1/graphql", json!({"query": "{ nope }"}));
    req.headers_mut()
        .insert("x-hasura-admin-secret", "secret".parse().unwrap());
    send(&router, req).await;
    let log = &http_logs(&lines)[0];
    assert_eq!(log["level"], "error");
    assert_eq!(log["detail"]["http_info"]["status"], 200);
    assert_eq!(
        log["detail"]["operation"]["error"]["code"],
        "validation-failed"
    );

    // Failed authentication: no user_vars, request id generated.
    let resp = send(
        &router,
        post("/v1/graphql", json!({"query": "{ __typename }"})),
    )
    .await;
    let rid = resp.headers()["x-request-id"].to_str().unwrap().to_owned();
    let log = &http_logs(&lines)[0];
    assert_eq!(log["level"], "error");
    let op = &log["detail"]["operation"];
    assert!(op.get("user_vars").is_none());
    assert_eq!(op["request_id"], rid.as_str());
    assert_eq!(op["error"]["code"], "access-denied");
    assert_eq!(op["request_mode"], "error");

    // A batch.
    let mut req = post(
        "/v1/graphql",
        json!([{"query": "{ __typename }"}, {"query": "{ nope }"}]),
    );
    req.headers_mut()
        .insert("x-hasura-admin-secret", "secret".parse().unwrap());
    send(&router, req).await;
    let log = &http_logs(&lines)[0];
    assert_eq!(log["detail"]["operation"]["request_mode"], "batched");
    let ops = log["detail"]["batched_operations"].as_array().unwrap();
    assert!(ops[0]["response_size"].is_u64());
    assert_eq!(ops[1]["error"]["code"], "validation-failed");

    // Metadata: the query is summarised by its type.
    let mut req = post(
        "/v1/metadata",
        json!({"type": "export_metadata", "args": {}}),
    );
    req.headers_mut()
        .insert("x-hasura-admin-secret", "secret".parse().unwrap());
    send(&router, req).await;
    let log = &http_logs(&lines)[0];
    assert_eq!(
        log["detail"]["operation"]["query"],
        json!({"type": "export_metadata"})
    );
    assert_eq!(log["detail"]["operation"]["request_mode"], "non-graphql");

    // No logs for CORS preflights (V2 answers them before its request logging).
    let req = Request::options("/v1/graphql")
        .header(header::ORIGIN, "http://example.com")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .body(Body::empty())
        .unwrap();
    send(&router, req).await;
    assert!(http_logs(&lines).is_empty());

    // /healthz: no timings, no query body.
    send(
        &router,
        Request::get("/healthz").body(Body::empty()).unwrap(),
    )
    .await;
    let log = &http_logs(&lines)[0];
    let op = &log["detail"]["operation"];
    assert_eq!(log["detail"]["http_info"]["url"], "/healthz");
    assert!(op.get("query_execution_time").is_none());
    assert_eq!(op["request_mode"], "non-graphql");

    // Compressed responses report both sizes.
    let mut req = post(
        "/v1/metadata",
        json!({"type": "export_metadata", "args": {}}),
    );
    let big = format!(
        "{{ {} }}",
        vec!["__typename"; 200]
            .iter()
            .enumerate()
            .map(|(i, f)| format!("a{i}: {f}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    *req.body_mut() = Body::from(json!({"query": big}).to_string());
    *req.uri_mut() = "/v1/graphql".parse().unwrap();
    req.headers_mut()
        .insert("x-hasura-admin-secret", "secret".parse().unwrap());
    req.headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip".parse().unwrap());
    let resp = send(&router, req).await;
    assert_eq!(resp.headers()[header::CONTENT_ENCODING], "gzip");
    let log = &http_logs(&lines)[0];
    assert_eq!(log["detail"]["http_info"]["content_encoding"], "gzip");
    let op = &log["detail"]["operation"];
    assert!(
        op["response_size"].as_u64().unwrap() < op["uncompressed_response_size"].as_u64().unwrap(),
        "{op}"
    );
}
