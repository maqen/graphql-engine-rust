//! `/healthz`, `/v1/version`, `/v1alpha1/config`, `/dev/*`, API gating and fallbacks
//! (spec 10 §6-7, §11-12). No database needed.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use hge_server::ServerConfig;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn router(config: ServerConfig) -> axum::Router {
    hge_server::routes::router(hge_server::build_engine(config).await.unwrap())
}

async fn call(
    router: &axum::Router,
    method: Method,
    path: &str,
    admin: bool,
) -> (StatusCode, Option<String>, String) {
    let mut req = Request::builder().method(method).uri(path);
    if admin {
        req = req.header("x-hasura-admin-secret", "secret");
    }
    let resp = router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned());
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

fn config() -> ServerConfig {
    ServerConfig {
        admin_secrets: vec!["secret".into()],
        ..ServerConfig::default()
    }
}

#[tokio::test]
async fn health_and_version() {
    let r = router(config()).await;
    for path in ["/healthz", "/hasura/healthz", "/healthz?strict=true"] {
        let (status, ct, body) = call(&r, Method::GET, path, false).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(ct.as_deref(), Some("text/plain; charset=utf-8"));
        assert_eq!(body, "OK");
    }
    let (status, ct, body) = call(&r, Method::GET, "/v1/version", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("application/json; charset=utf-8"));
    assert!(
        body.starts_with(r#"{"server_type":"ce","version":"v2."#),
        "{body}"
    );
}

#[tokio::test]
async fn healthz_fails_without_metadata_database() {
    let mut engine = hge_server::build_engine(config()).await.unwrap();
    // A metadata database nobody listens on.
    let catalog = hge_metadata_api::PgCatalog::connect("postgres://u@127.0.0.1:1/db").unwrap();
    std::sync::Arc::get_mut(&mut engine).unwrap().metadata_db = Some(catalog.pool().clone());
    let r = hge_server::routes::router(engine);
    let (status, _, body) = call(&r, Method::GET, "/healthz", false).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, "ERROR");
}

#[tokio::test]
async fn config_endpoint() {
    let r = router(ServerConfig {
        jwt_secret: Some(r#"{"type":"HS256","key":"0123456789abcdef0123456789abcdef"}"#.into()),
        ..config()
    })
    .await;
    let (status, _, body) = call(&r, Method::GET, "/v1alpha1/config", false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _, body) = call(&r, Method::GET, "/v1alpha1/config", true).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "version",
            "is_function_permissions_inferred",
            "is_remote_schema_permissions_enabled",
            "is_admin_secret_set",
            "is_auth_hook_set",
            "is_jwt_set",
            "jwt",
            "is_allow_list_enabled",
            "live_queries",
            "streaming_queries",
            "console_assets_dir",
            "experimental_features",
            "is_prometheus_metrics_enabled",
            "default_naming_convention",
            "is_apollo_federation_enabled"
        ]
    );
    assert_eq!(v["is_admin_secret_set"], true);
    assert_eq!(v["is_jwt_set"], true);
    assert_eq!(
        v["jwt"],
        json!([{"claims_namespace": "https://hasura.io/jwt/claims", "claims_format": "json", "claims_map": null}])
    );
    assert_eq!(
        v["live_queries"],
        json!({"batch_size": 100, "refetch_delay": 1.0})
    );
}

#[tokio::test]
async fn api_gating_and_fallbacks() {
    let not_found = r#"{"error":"resource does not exist","path":"$","code":"not-found"}"#;
    let internal = r#"{"error":"Internal Server Error","path":"$","code":"unexpected"}"#;
    // Developer endpoints are off by default.
    let r = router(config()).await;
    for (method, path, status, body) in [
        (Method::GET, "/dev/ekg", StatusCode::NOT_FOUND, not_found),
        (Method::GET, "/nope", StatusCode::NOT_FOUND, not_found),
        (Method::POST, "/nope", StatusCode::NOT_FOUND, not_found),
        (
            Method::GET,
            "/v1/metadata",
            StatusCode::NOT_FOUND,
            not_found,
        ),
        (Method::POST, "/healthz", StatusCode::NOT_FOUND, not_found),
        (
            Method::PUT,
            "/v1/graphql",
            StatusCode::INTERNAL_SERVER_ERROR,
            internal,
        ),
        (
            Method::DELETE,
            "/healthz",
            StatusCode::INTERNAL_SERVER_ERROR,
            internal,
        ),
    ] {
        let (s, _, b) = call(&r, method.clone(), path, true).await;
        assert_eq!((s, b.as_str()), (status, body), "{method} {path}");
    }
    // No `Allow` header on a wrong method (V2 sends none).
    let resp = r
        .clone()
        .oneshot(Request::get("/v1/metadata").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(resp.headers().get(header::ALLOW).is_none());
    // Deliberate deviation: `HEAD` on a `GET` route answers like `GET` (V2: 500), which
    // only helps load balancers that probe with `HEAD`.
    let (s, _, _) = call(&r, Method::HEAD, "/healthz", false).await;
    assert_eq!(s, StatusCode::OK);
    // Only GraphQL and the developer API.
    let r = router(ServerConfig {
        enabled_apis: vec!["graphql".into(), "developer".into()],
        ..config()
    })
    .await;
    for path in ["/v1/metadata", "/v2/query", "/v1/query"] {
        let (s, _, b) = call(&r, Method::POST, path, true).await;
        assert_eq!(
            (s, b.as_str()),
            (StatusCode::NOT_FOUND, not_found),
            "{path}"
        );
    }
    let (s, _, _) = call(&r, Method::GET, "/v1alpha1/config", true).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, b) = call(&r, Method::GET, "/dev/plan_cache", true).await;
    assert_eq!((s, b.as_str()), (StatusCode::OK, "null"));
    let (s, _, b) = call(&r, Method::GET, "/dev/subscriptions", true).await;
    assert_eq!(s, StatusCode::OK);
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["live_queries"]["pollers"], 0);
    let (s, _, b) = call(&r, Method::GET, "/dev/ekg", false).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{b}");
}
