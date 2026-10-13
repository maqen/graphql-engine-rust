//! `webhook-log` (V2's `Hasura.Server.Logging.WebHookLog`, logged from
//! `Hasura.Server.Auth.WebHook`). No database needed. The capture logger is process-wide,
//! so everything runs in one test.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hge_server::ServerConfig;
use hge_server::telemetry::{self, LogConfig};
use serde_json::{Value, json};
use tower::ServiceExt;

const BAD_EXPIRY: &str = r#"{"X-Hasura-Role":"user","Cache-Control":"public","Expires":"soon"}"#;

#[tokio::test]
async fn webhook_calls_are_logged() {
    let lines = telemetry::init_capture(LogConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hook = axum::Router::new()
        .route(
            "/500",
            axum::routing::get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        )
        .route(
            "/401",
            axum::routing::get(|| async { (StatusCode::UNAUTHORIZED, "go away") }),
        )
        .route(
            "/invalid",
            axum::routing::get(|| async { r#"{"X-Hasura-Role":1}"# }),
        )
        .route(
            "/ok",
            axum::routing::get(|| async { r#"{"X-Hasura-Role":"user"}"# }),
        )
        .route("/bad-expiry", axum::routing::get(|| async { BAD_EXPIRY }));
    tokio::spawn(async move { axum::serve(listener, hook).await });
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };

    let row = |status: u16, response: Option<&str>, message: Option<&str>| json!({"status_code": status, "http_error": null, "response": response, "message": message});
    let cases: Vec<(String, Vec<(&str, Value)>)> = vec![
        (
            format!("http://{addr}/500"),
            vec![("error", row(500, Some("boom"), None))],
        ),
        (
            format!("http://{addr}/401"),
            vec![("error", row(401, Some("go away"), None))],
        ),
        (
            format!("http://{addr}/invalid"),
            vec![("error", row(200, Some(r#"{"X-Hasura-Role":1}"#), None))],
        ),
        (
            format!("http://{addr}/ok"),
            vec![("info", row(200, None, None))],
        ),
        (
            format!("http://{addr}/bad-expiry"),
            vec![
                ("info", row(200, None, None)),
                (
                    "warn",
                    row(
                        200,
                        Some(BAD_EXPIRY),
                        Some("could not find max-age/s-maxage"),
                    ),
                ),
                (
                    "warn",
                    row(
                        200,
                        Some(BAD_EXPIRY),
                        Some("Value of Expires header is not a valid timestamp"),
                    ),
                ),
            ],
        ),
        (
            format!("http://{closed}/hook?x=1"),
            vec![(
                "error",
                json!({
                    "status_code": null,
                    "http_error": {
                        "type": "http_exception",
                        "message": "Connection failure",
                        "request": {
                            "host": "127.0.0.1",
                            "port": closed.port(),
                            "secure": false,
                            "path": "/hook",
                            "queryString": "?x=1",
                            "method": "GET",
                            "responseTimeout": "ResponseTimeoutDefault",
                        },
                    },
                    "response": null,
                    "message": null,
                }),
            )],
        ),
    ];

    for (url, expected) in cases {
        let engine = hge_server::build_engine(ServerConfig {
            admin_secrets: vec!["secret".into()],
            auth_hook: Some(hge_auth::HookConfig {
                url: url.clone(),
                mode: hge_auth::HookMode::Get,
                send_request_body: false,
                cache: false,
            }),
            ..ServerConfig::default()
        })
        .await
        .unwrap();
        let router = hge_server::routes::router(engine);
        lines.lock().unwrap().clear();
        let req = Request::post("/v1/graphql")
            .header("authorization", "Bearer secret-token")
            .body(Body::from(r#"{"query":"{ __typename }"}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
        let logs: Vec<Value> = lines
            .lock()
            .unwrap()
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|l| l["type"] == "webhook-log")
            .collect();
        assert_eq!(logs.len(), expected.len(), "{url}: {logs:#?}");
        for (log, (level, fields)) in logs.iter().zip(&expected) {
            assert_eq!(log["level"], *level, "{url}: {log}");
            let mut want = fields.clone();
            want["url"] = Value::from(url.as_str());
            want["method"] = Value::from("GET");
            // Exactly V2's keys, no more.
            assert_eq!(log["detail"], want, "{url}");
            let text = log["detail"].to_string();
            assert!(!text.contains("secret-token"), "no request headers: {text}");
        }
    }
}
