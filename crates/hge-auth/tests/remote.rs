//! JWK URLs and the auth webhook, against mock servers.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use common::*;
use hge_auth::{AuthConfig, Authenticator, HookConfig, HookMode};
use serde_json::{Value, json};

#[derive(Clone, Default)]
struct Jwks {
    keys: Arc<Mutex<Value>>,
    cache_control: Arc<Mutex<Option<&'static str>>>,
    hits: Arc<AtomicUsize>,
}

async fn jwks_server(state: Jwks) -> String {
    serve(Router::new().route(
        "/jwks",
        get(move |headers: HeaderMap| {
            let state = state.clone();
            async move {
                state.hits.fetch_add(1, Ordering::SeqCst);
                assert!(
                    headers["user-agent"]
                        .to_str()
                        .unwrap()
                        .starts_with("hasura-graphql-engine/")
                );
                let mut resp = axum::Json(state.keys.lock().unwrap().clone()).into_response();
                if let Some(cc) = *state.cache_control.lock().unwrap() {
                    resp.headers_mut()
                        .insert("cache-control", cc.parse().unwrap());
                }
                resp
            }
        }),
    ))
    .await
}

#[tokio::test]
async fn jwk_url_and_rotation() {
    let state = Jwks::default();
    *state.keys.lock().unwrap() = json!({"keys": [
        rsa_jwk("k1"),
        {"kty": "RSA", "kid": "enc-only", "use": "other", "n": "AQAB", "e": "AQAB"},
        {"kty": "XYZ", "kid": "unknown"}
    ]});
    let url = jwks_server(state.clone()).await;
    // JWT-only mode (no admin secret): V2 never loads the keys here; we do.
    let a = Authenticator::new(AuthConfig {
        disable_admin_secret: true,
        jwt_secret: Some(json!({"jwk_url": format!("{url}/jwks")}).to_string()),
        ..AuthConfig::default()
    })
    .await
    .unwrap();
    assert_eq!(state.hits.load(Ordering::SeqCst), 1);

    let c = claims(&["user"], "user", json!({}));
    let token = sign(Signer::Rs256, json!({"kid": "k1"}), &c);
    assert!(run(&a, &[("authorization", &bearer(&token))]).await.is_ok());

    let token = sign(Signer::Rs256, json!({"kid": "k2"}), &c);
    let e = run(&a, &[("authorization", &bearer(&token))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: JWSError NoUsableKeys");
    // Fetches are spaced at least 60 s apart, so the unknown kid does not refetch yet.
    assert_eq!(state.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn jwk_url_startup_failure_is_fatal() {
    let url = serve(Router::new().route("/jwks", get(|| async { StatusCode::NOT_FOUND }))).await;
    let e = Authenticator::new(AuthConfig {
        admin_secrets: vec!["s".into()],
        jwt_secret: Some(json!({"jwk_url": format!("{url}/jwks")}).to_string()),
        ..AuthConfig::default()
    })
    .await
    .unwrap_err();
    assert_eq!(
        e,
        format!("Fatal Error: Non-2xx response on fetching JWK from: {url}/jwks")
    );

    let url = serve(Router::new().route("/jwks", get(|| async { "not json" }))).await;
    let e = Authenticator::new(AuthConfig {
        admin_secrets: vec!["s".into()],
        jwt_secret: Some(json!({"jwk_url": format!("{url}/jwks")}).to_string()),
        ..AuthConfig::default()
    })
    .await
    .unwrap_err();
    assert_eq!(
        e,
        format!("Fatal Error: Error parsing JWK from url: {url}/jwks")
    );
}

/// A request the webhook saw: headers and, in POST mode, the body.
type Seen = (HeaderMap, Option<Value>);

#[derive(Clone, Default)]
struct Hook {
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// A webhook that answers according to the `x-answer` / `X-Answer` request header.
async fn hook_server(state: Hook) -> String {
    fn answer(kind: &str) -> Response {
        match kind {
            "401" => StatusCode::UNAUTHORIZED.into_response(),
            "500" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            "norole" => axum::Json(json!({"X-Hasura-User-Id": "1"})).into_response(),
            "badbody" => {
                axum::Json(json!({"X-Hasura-Role": "user", "X-Hasura-User-Id": 1})).into_response()
            }
            "cached" => {
                axum::Json(json!({"X-Hasura-Role": "user", "Cache-Control": "max-age=600"}))
                    .into_response()
            }
            _ => {
                let mut r = axum::Json(json!({
                    "X-Hasura-Role": "user", "X-Hasura-User-Id": "42", "Expires": "Wed, 21 Oct 2099 07:28:00 GMT"
                }))
                .into_response();
                r.headers_mut().append("set-cookie", "a=1".parse().unwrap());
                r.headers_mut().append("set-cookie", "b=2".parse().unwrap());
                r
            }
        }
    }
    let get_state = state.clone();
    serve(
        Router::new()
            .route(
                "/hook",
                get(move |headers: HeaderMap| {
                    let state = get_state.clone();
                    async move {
                        let kind = headers
                            .get("x-answer")
                            .map(|v| v.to_str().unwrap().to_owned())
                            .unwrap_or_default();
                        state.seen.lock().unwrap().push((headers, None));
                        answer(&kind)
                    }
                })
                .post(
                    move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
                        let state = state.clone();
                        async move {
                            let kind = body["headers"]["X-Answer"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned();
                            state.seen.lock().unwrap().push((headers, Some(body)));
                            answer(&kind)
                        }
                    },
                ),
            )
            .route("/slow", post(|| async { StatusCode::OK })),
    )
    .await
}

async fn hook_auth(url: String, mode: HookMode, cache: bool) -> Authenticator {
    Authenticator::new(AuthConfig {
        admin_secrets: vec!["secret".into()],
        auth_hook: Some(HookConfig {
            cache,
            ..HookConfig::new(url, mode)
        }),
        ..AuthConfig::default()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn webhook_get() {
    let state = Hook::default();
    let url = hook_server(state.clone()).await;
    let a = hook_auth(format!("{url}/hook"), HookMode::Get, false).await;
    let ok = run(
        &a,
        &[
            ("authorization", "Bearer abc"),
            ("x-hasura-role", "admin"),
            ("accept", "text/html"),
            ("content-type", "text/plain"),
        ],
    )
    .await
    .unwrap();
    // The webhook decides the role; the requested one is ignored.
    assert_eq!(ok.session.role.as_str(), "user");
    assert_eq!(ok.session.variables.get("x-hasura-user-id"), Some("42"));
    assert_eq!(ok.set_cookies.len(), 2);
    assert!(ok.expires_at.is_some());
    let (seen, _) = state.seen.lock().unwrap().pop().unwrap();
    assert_eq!(seen["authorization"], "Bearer abc");
    assert_eq!(seen["x-hasura-role"], "admin");
    assert_ne!(seen["accept"], "text/html", "Accept is not forwarded");
    assert_eq!(seen["content-type"], "application/json");
    assert!(
        seen["user-agent"]
            .to_str()
            .unwrap()
            .starts_with("hasura-graphql-engine/")
    );

    // A valid admin secret skips the webhook.
    let before = state.seen.lock().unwrap().len();
    assert!(
        run(&a, &[("x-hasura-admin-secret", "secret")])
            .await
            .unwrap()
            .session
            .is_admin()
    );
    assert_eq!(state.seen.lock().unwrap().len(), before);
    let e = run(&a, &[("x-hasura-admin-secret", "wrong")])
        .await
        .unwrap_err();
    assert_eq!(err(e).0, 401);

    let e = run(&a, &[("x-answer", "401")]).await.unwrap_err();
    assert_eq!(
        err(e),
        (
            401,
            "access-denied",
            "Authentication hook unauthorized this request".into()
        )
    );
    let e = run(&a, &[("x-answer", "500")]).await.unwrap_err();
    assert_eq!(
        err(e),
        (
            500,
            "unexpected",
            "Invalid response from authorization hook".into()
        )
    );
    let e = run(&a, &[("x-answer", "norole")]).await.unwrap_err();
    assert_eq!(
        err(e),
        (
            400,
            "invalid-params",
            "\"x-hasura-role\" not found in session variables".into()
        )
    );
    let e = run(&a, &[("x-answer", "badbody")]).await.unwrap_err();
    let (status, code, message) = err(e);
    assert_eq!((status, code), (500, "unexpected"));
    assert!(message.starts_with("Invalid response from authorization hook: "));

    let dead = hook_auth("http://127.0.0.1:1/hook".into(), HookMode::Get, false).await;
    let e = run(&dead, &[]).await.unwrap_err();
    assert_eq!(
        err(e),
        (
            500,
            "unexpected",
            "webhook authentication request failed".into()
        )
    );
}

#[tokio::test]
async fn webhook_post() {
    let state = Hook::default();
    let url = hook_server(state.clone()).await;
    let a = hook_auth(format!("{url}/hook"), HookMode::Post, false).await;
    assert!(a.needs_request_body());
    let request = json!({"query": "{ a }", "variables": null, "operationName": null});
    let ok = a
        .authenticate(
            &headers(&[("authorization", "Bearer abc"), ("x-hasura-role", "x")]),
            Some(&request),
        )
        .await
        .unwrap();
    assert_eq!(ok.session.role.as_str(), "user");
    let (seen, body) = state.seen.lock().unwrap().pop().unwrap();
    assert_eq!(seen["content-type"], "application/json");
    let body = body.unwrap();
    assert_eq!(body["headers"]["Authorization"], "Bearer abc");
    assert_eq!(body["headers"]["X-Hasura-Role"], "x");
    assert_eq!(body["request"], request);

    // Non-GraphQL endpoints send `"request": null`.
    run(&a, &[]).await.unwrap();
    let (_, body) = state.seen.lock().unwrap().pop().unwrap();
    assert_eq!(body.unwrap()["request"], Value::Null);

    // Without send_request_body there is no `request` key.
    let a = Authenticator::new(AuthConfig {
        admin_secrets: vec!["secret".into()],
        auth_hook: Some(HookConfig {
            send_request_body: false,
            ..HookConfig::new(format!("{url}/hook"), HookMode::Post)
        }),
        ..AuthConfig::default()
    })
    .await
    .unwrap();
    assert!(!a.needs_request_body());
    run(&a, &[]).await.unwrap();
    let (_, body) = state.seen.lock().unwrap().pop().unwrap();
    assert!(body.unwrap().get("request").is_none());
}

#[tokio::test]
async fn webhook_cache() {
    let state = Hook::default();
    let url = hook_server(state.clone()).await;
    let a = hook_auth(format!("{url}/hook"), HookMode::Get, true).await;
    for _ in 0..3 {
        run(&a, &[("x-answer", "cached")]).await.unwrap();
    }
    assert_eq!(
        state.seen.lock().unwrap().len(),
        1,
        "answers with max-age are reused"
    );
    run(&a, &[("x-answer", "cached"), ("authorization", "other")])
        .await
        .unwrap();
    assert_eq!(
        state.seen.lock().unwrap().len(),
        2,
        "different headers are a different key"
    );
    // Answers that set cookies are not cached.
    run(&a, &[]).await.unwrap();
    run(&a, &[]).await.unwrap();
    assert_eq!(state.seen.lock().unwrap().len(), 4);

    let uncached = hook_auth(format!("{url}/hook"), HookMode::Get, false).await;
    run(&uncached, &[("x-answer", "cached")]).await.unwrap();
    run(&uncached, &[("x-answer", "cached")]).await.unwrap();
    assert_eq!(
        state.seen.lock().unwrap().len(),
        6,
        "V2 calls the webhook every time"
    );
}
