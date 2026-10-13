//! HTTP routes. Workstreams register their endpoints in [`router`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hge_core::{Code, Error};
use serde::Deserialize;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::graphql::{self, GraphQLBody, GraphQLRequest};
use crate::state::Engine;

pub const VERSION: &str = concat!("v2.48.0-rs.", env!("CARGO_PKG_VERSION"));

pub fn router(engine: Arc<Engine>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/version", get(version));
    if engine.config.api_enabled("graphql") {
        router = router
            .route("/v1/graphql", get(graphql_get).post(graphql_post))
            .route("/v1alpha1/graphql", axum::routing::post(graphql_post));
    }
    let mut router = router
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(CompressionLayer::new());
    if !engine.config.disable_cors {
        router = router.layer(cors(&engine.config.cors_domain));
    }
    router.with_state(engine)
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

fn request_id(headers: &HeaderMap) -> String {
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

fn json_response(status: StatusCode, body: String, request_id: Option<String>) -> Response {
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

async fn graphql_post(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id(&headers);
    let parsed: Result<GraphQLBody, _> = serde_json::from_slice(&body);
    match parsed {
        Ok(body) => handle(&engine, &headers, body, rid).await,
        Err(e) => {
            let err = Error::new(Code::InvalidJson, format!("Error in $: {e}"));
            json_response(StatusCode::OK, graphql::error_body(&err, false), Some(rid))
        }
    }
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
    Query(params): Query<GetParams>,
) -> Response {
    let rid = request_id(&headers);
    let variables = match params.variables.as_deref().map(serde_json::from_str) {
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
    };
    handle(&engine, &headers, GraphQLBody::Single(req), rid).await
}

async fn handle(engine: &Engine, headers: &HeaderMap, body: GraphQLBody, rid: String) -> Response {
    let session = match hge_auth::authenticate(&engine.auth, headers) {
        Ok(s) => s,
        Err(e) => {
            let status = if engine.config.preserve_401_errors {
                StatusCode::from_u16(e.code.http_status()).unwrap_or(StatusCode::UNAUTHORIZED)
            } else {
                StatusCode::OK
            };
            return json_response(status, graphql::error_body(&e, false), Some(rid));
        }
    };
    let out = graphql::execute_body(engine, &session, body).await;
    json_response(StatusCode::OK, out, Some(rid))
}
