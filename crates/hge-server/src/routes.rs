//! HTTP routes. Workstreams register their endpoints in [`router`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hge_core::{Code, Error, Session};
use serde::Deserialize;
use tower_http::CompressionLevel;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::SizeAbove;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::graphql::{self, GraphQLBody, GraphQLRequest};
use crate::state::Engine;
use crate::ws::{self, MaybeUpgrade};

pub const VERSION: &str = concat!("v2.48.0-rs.", env!("CARGO_PKG_VERSION"));

pub fn router(engine: Arc<Engine>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/version", get(version));
    if engine.config.api_enabled("graphql") {
        router = router
            .route("/v1/graphql", get(graphql_get).post(graphql_post))
            .route(
                "/v1alpha1/graphql",
                get(graphql_ws_v1alpha1).post(graphql_post_v1alpha1),
            );
    }
    if engine.config.api_enabled("metadata") {
        router = router
            .route("/v1/metadata", axum::routing::post(metadata_post))
            .route("/v2/query", axum::routing::post(query_post))
            .route("/v1/query", axum::routing::post(crate::v1_query::handler));
    }
    let mut router = router
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(compression());
    if !engine.config.disable_cors {
        router = router.layer(cors(&engine.config.cors_domain));
    }
    router.with_state(engine)
}

/// gzip like V2: responses under 700 bytes are sent uncompressed, at a fast compression
/// level (V2 uses level 1, or 6 above 20kB).
fn compression() -> CompressionLayer<SizeAbove> {
    CompressionLayer::new()
        .quality(CompressionLevel::Fastest)
        .compress_when(SizeAbove::new(700))
}

fn cors(domain: &str) -> CorsLayer {
    let layer = CorsLayer::new()
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
        .max_age(std::time::Duration::from_secs(1728000));
    if domain.trim() == "*" {
        layer.allow_origin(tower_http::cors::Any)
    } else {
        let origins: Vec<HeaderValue> = domain
            .split(',')
            .filter_map(|d| d.trim().parse().ok())
            .collect();
        layer.allow_origin(AllowOrigin::list(origins))
    }
}

async fn healthz(State(engine): State<Arc<Engine>>) -> Response {
    let state = engine.state.load();
    if state.inconsistencies.is_empty() {
        (StatusCode::OK, "OK").into_response()
    } else {
        (StatusCode::OK, "WARN: inconsistent objects in schema").into_response()
    }
}

async fn version() -> Response {
    json_response(
        StatusCode::OK,
        serde_json::json!({ "version": VERSION }).to_string(),
        None,
    )
}

pub(crate) fn request_id(headers: &HeaderMap) -> String {
    if let Some(id) = headers.get("x-request-id").and_then(|v| v.to_str().ok()) {
        return id.to_owned();
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "{:016x}-{:08x}",
        nanos as u64,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) fn json_response(
    status: StatusCode,
    body: String,
    request_id: Option<String>,
) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    if let Some(id) = request_id.and_then(|id| HeaderValue::from_str(&id).ok()) {
        resp.headers_mut().insert("x-request-id", id);
    }
    resp
}

/// Which GraphQL endpoint: `/v1/graphql` reports errors with status 200 (except 401 with
/// `HASURA_GRAPHQL_PRESERVE_401_ERRORS`); `/v1alpha1/graphql` keeps the real status.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    V1,
    V1Alpha1,
}

async fn graphql_post(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    post(&engine, &headers, &body, Endpoint::V1).await
}

async fn graphql_post_v1alpha1(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    post(&engine, &headers, &body, Endpoint::V1Alpha1).await
}

async fn post(engine: &Engine, headers: &HeaderMap, body: &[u8], endpoint: Endpoint) -> Response {
    let rid = request_id(headers);
    // The POST auth webhook gets the request as JSON; parse it only in that mode.
    let raw_request = if engine.auth.needs_request_body() {
        serde_json::from_slice::<serde_json::Value>(body).ok()
    } else {
        None
    };
    // Authenticate before reporting a malformed body, as V2 does.
    let auth = match authenticate(engine, headers, raw_request.as_ref(), endpoint, &rid).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let parsed: Result<GraphQLBody, _> = serde_json::from_slice(body);
    let resp = match parsed {
        Ok(body) => run(engine, &auth.session, headers, body, rid).await,
        Err(e) => {
            let err = Error::new(Code::InvalidJson, format!("Error in $: {e}"));
            json_response(StatusCode::OK, graphql::error_body(&err, false), Some(rid))
        }
    };
    with_cookies(resp, auth.set_cookies)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetParams {
    query: Option<String>,
    variables: Option<String>,
    operation_name: Option<String>,
}

async fn graphql_get(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    MaybeUpgrade(upgrade): MaybeUpgrade,
    Query(params): Query<GetParams>,
) -> Response {
    if let Some(upgrade) = upgrade {
        return ws::upgrade(engine, upgrade, headers, ws::Endpoint::V1);
    }
    let rid = request_id(&headers);
    let variables = params
        .variables
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>);
    let raw_request = engine.auth.needs_request_body().then(|| {
        serde_json::json!({
            "query": params.query,
            "variables": variables.as_ref().and_then(|v| v.as_ref().ok()),
            "operationName": params.operation_name,
        })
    });
    let auth = match authenticate(&engine, &headers, raw_request.as_ref(), Endpoint::V1, &rid).await
    {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let variables = match variables {
        None => None,
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => {
            let err = Error::new(Code::InvalidJson, format!("invalid variables: {e}"));
            return json_response(StatusCode::OK, graphql::error_body(&err, false), Some(rid));
        }
    };
    let req = GraphQLRequest {
        query: params.query,
        variables,
        operation_name: params.operation_name,
        read_only: true,
    };
    let resp = run(
        &engine,
        &auth.session,
        &headers,
        GraphQLBody::Single(req),
        rid,
    )
    .await;
    with_cookies(resp, auth.set_cookies)
}

/// `/v1alpha1/graphql` takes WebSockets on `GET`, not queries.
async fn graphql_ws_v1alpha1(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    MaybeUpgrade(upgrade): MaybeUpgrade,
) -> Response {
    match upgrade {
        Some(upgrade) => ws::upgrade(engine, upgrade, headers, ws::Endpoint::V1Alpha1),
        None => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

/// Authenticate a GraphQL request, or build the error response (spec 05 §9.2).
async fn authenticate(
    engine: &Engine,
    headers: &HeaderMap,
    request: Option<&serde_json::Value>,
    endpoint: Endpoint,
    rid: &str,
) -> Result<hge_auth::Authenticated, Response> {
    engine
        .auth
        .authenticate(headers, request)
        .await
        .map_err(|e| {
            tracing::info!(target: "http-log", request_id = rid, error = %e, "authentication failed");
            let status = match endpoint {
                Endpoint::V1Alpha1 => e.status,
                Endpoint::V1
                    if engine.config.preserve_401_errors
                        && e.status == StatusCode::UNAUTHORIZED =>
                {
                    e.status
                }
                Endpoint::V1 => StatusCode::OK,
            };
            json_response(
                status,
                graphql::error_body(&e.error, false),
                Some(rid.to_owned()),
            )
        })
}

/// Add the auth webhook's `Set-Cookie` headers.
fn with_cookies(mut resp: Response, cookies: Vec<HeaderValue>) -> Response {
    for cookie in cookies {
        resp.headers_mut().append(header::SET_COOKIE, cookie);
    }
    resp
}

async fn run(
    engine: &Engine,
    session: &Session,
    headers: &HeaderMap,
    body: GraphQLBody,
    rid: String,
) -> Response {
    let out = graphql::execute_body(engine, session, headers, body).await;
    let mut resp = json_response(StatusCode::OK, out.body, Some(rid));
    // Action handlers' `Set-Cookie` headers (mutations only, spec 06 §8.6).
    for cookie in out.set_cookies {
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

/// Admin-only APIs (`/v1/metadata`, `/v2/query`): resolve the session and insist on `admin`.
async fn require_admin(engine: &Engine, headers: &HeaderMap) -> Result<(), Box<Response>> {
    let api_error = |status: u16, err: &Error| {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
        Box::new(json_response(
            status,
            err.to_api_json(false).to_string(),
            None,
        ))
    };
    match engine.auth.authenticate(headers, None).await {
        Ok(auth) if auth.session.role.is_admin() => Ok(()),
        Ok(_) => Err(api_error(
            400,
            &Error::new(Code::AccessDenied, "restricted access : admin only").with_path("$.args"),
        )),
        Err(e) => Err(api_error(e.status.as_u16(), &e.error)),
    }
}

fn api_response(resp: hge_metadata_api::ApiResponse, rid: String) -> Response {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    json_response(status, resp.body.to_string(), Some(rid))
}

async fn metadata_post(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id(&headers);
    if let Err(resp) = require_admin(&engine, &headers).await {
        return *resp;
    }
    // Run to completion even if the client goes away: a metadata change must not stop
    // between writing the store and installing the new schema.
    let before = engine.state.load().resource_version;
    let task_engine = engine.clone();
    let resp =
        tokio::spawn(async move { hge_metadata_api::handle_metadata(&*task_engine, &body).await })
            .await
            .unwrap_or_else(|e| panic_response(&e));
    // Like V2, subscriptions do not outlive a metadata change made on this instance.
    if engine.config.websocket.close_on_metadata_change
        && engine.state.load().resource_version != before
    {
        engine
            .ws
            .close_all("Server state changed, restarting the server");
    }
    api_response(resp, rid)
}

/// A handler that panicked inside its own task.
fn panic_response(e: &tokio::task::JoinError) -> hge_metadata_api::ApiResponse {
    tracing::error!("metadata request failed: {e}");
    hge_metadata_api::ApiError::new(500, Code::Unexpected, "internal error").to_response()
}

async fn query_post(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id(&headers);
    if let Err(resp) = require_admin(&engine, &headers).await {
        return *resp;
    }
    // See `metadata_post`: run_sql commits DDL before the metadata catches up.
    let resp = tokio::spawn(async move { hge_metadata_api::handle_query(&*engine, &body).await })
        .await
        .unwrap_or_else(|e| panic_response(&e));
    api_response(resp, rid)
}
