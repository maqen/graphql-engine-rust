//! `jwk-refresh-log` (V2's `Hasura.Server.Auth.JWT.Logging.JwkRefreshLog`, logged from
//! `fetchJwk` / `determineJwkExpiryLifetime`). The capture logger is process-wide, so
//! everything runs in one test. Failing cases take the startup retries (~6 s each).

use axum::http::StatusCode;
use hge_server::telemetry::{self, Level, LogConfig, LogTypes};
use serde_json::{Value, json};

#[tokio::test]
async fn jwk_fetches_are_logged() {
    let lines = telemetry::init_capture(LogConfig {
        level: Level::Info,
        types: LogTypes::parse("jwk-refresh-log").unwrap().0,
    });
    let jwks = r#"{"keys":[{"kty":"oct","kid":"k1","k":"c2VjcmV0LXNlY3JldC1zZWNyZXQtc2VjcmV0LXNlY3JldA"}]}"#;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/500",
            axum::routing::get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        )
        .route("/garbage", axum::routing::get(|| async { "not json" }))
        .route(
            "/bad-expiry",
            axum::routing::get(move || async move { ([("cache-control", "max-age=x")], jwks) }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await });
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };

    let critical = |error: Value| ("critical", json!({"message": null, "error": error}));
    let cases: Vec<(String, bool, Value)> = vec![
        (
            format!("http://{closed}/jwks"),
            false,
            json!({"http_exception": {
                "type": "http_exception",
                "message": "Connection failure",
                "request": {
                    "host": "127.0.0.1",
                    "port": closed.port(),
                    "secure": false,
                    "path": "/jwks",
                    "queryString": "",
                    "method": "GET",
                    "responseTimeout": "ResponseTimeoutDefault",
                },
            }}),
        ),
        (
            format!("http://{addr}/500"),
            false,
            json!({"status_code": 500, "url": format!("http://{addr}/500"), "response": "boom"}),
        ),
        (
            format!("http://{addr}/garbage"),
            false,
            json!({"message": format!("Error parsing JWK from url: http://{addr}/garbage")}),
        ),
        (
            format!("http://{addr}/bad-expiry"),
            true,
            json!({
                "error": "could not parse max-age/s-maxage value",
                "message": "Failed parsing Cache-Control header from JWK response",
            }),
        ),
    ];

    for (url, succeeds, error) in cases {
        lines.lock().unwrap().clear();
        let result = hge_auth::Authenticator::new(hge_auth::AuthConfig {
            admin_secrets: vec!["secret".into()],
            jwt_secret: Some(json!({"jwk_url": url}).to_string()),
            user_agent: "test".into(),
            ..Default::default()
        })
        .await;
        assert_eq!(result.is_ok(), succeeds, "{url}: {result:?}");
        let logs: Vec<Value> = lines
            .lock()
            .unwrap()
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|l| l["type"] == "jwk-refresh-log")
            .collect();
        let got: Vec<(&str, &Value)> = logs
            .iter()
            .map(|l| (l["level"].as_str().unwrap(), &l["detail"]))
            .collect();
        let refreshing =
            json!({"message": format!("Refreshing JWK from endpoint: {url}"), "error": null});
        assert!(got.len() >= 2, "{url}: {got:#?}");
        assert_eq!(got[0], ("info", &refreshing), "{url}");
        if succeeds {
            assert_eq!(
                got[1],
                ("info", &json!({"message": null, "error": error})),
                "{url}"
            );
            assert_eq!(got.len(), 2, "{url}: {got:#?}");
        } else {
            let (level, detail) = got[1];
            let (want_level, want) = critical(error.clone());
            assert_eq!(level, want_level, "{url}");
            if detail["error"].get("error").is_some() {
                // A parse error: the parser's own text, and V2's message.
                assert!(detail["error"]["error"].is_string(), "{detail}");
                assert_eq!(detail["error"]["message"], want["error"]["message"]);
                assert_eq!(detail["message"], Value::Null);
                assert_eq!(detail.as_object().unwrap().len(), 2);
                assert_eq!(detail["error"].as_object().unwrap().len(), 2);
            } else {
                assert_eq!(detail, &want, "{url}");
            }
            // Startup retries: every attempt logs the same pair.
            assert_eq!(got.len(), 12, "{url}: {got:#?}");
        }
    }
}
