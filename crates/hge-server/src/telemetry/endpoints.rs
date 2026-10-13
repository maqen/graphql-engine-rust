//! Operational endpoints (spec 10 §6-7, §11-12): `/healthz`, `/v1/version`,
//! `/v1alpha1/config`, the `/dev/*` developer endpoints, and V2's responses for unknown
//! routes and methods.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hge_core::{Code, Error};
use serde_json::{Value, json};

use crate::routes::{VERSION, json_response};
use crate::state::Engine;

/// Register the operational routes on `router` (gated by `HASURA_GRAPHQL_ENABLED_APIS`
/// where V2 gates them).
pub fn register(router: Router<Arc<Engine>>, engine: &Engine) -> Router<Arc<Engine>> {
    let mut router = router
        .route("/healthz", get(healthz))
        .route("/hasura/healthz", get(healthz))
        .route("/v1/version", get(version));
    if engine.config.api_enabled("config") {
        router = router.route("/v1alpha1/config", get(config));
    }
    if engine.config.api_enabled("developer") {
        router = router
            .route("/dev/ekg", get(dev_ekg))
            .route("/dev/plan_cache", get(dev_plan_cache))
            .route("/dev/subscriptions", get(dev_subscriptions))
            .route("/dev/subscriptions/extended", get(dev_subscriptions));
    }
    router
}

/// V2's answers for unknown paths and methods. Must be applied after every route is
/// registered: axum applies the method fallback only to routes that already exist.
pub fn fallbacks(router: Router<Arc<Engine>>) -> Router<Arc<Engine>> {
    router
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
}

/// axum's method router adds `Allow` to a wrong-method answer even with a custom fallback
/// (outside any `Router::layer`); V2 sends none. Wrap the finished router with this.
pub fn without_allow(app: Router) -> Router {
    Router::new()
        .fallback_service(app)
        .layer(axum::middleware::from_fn(strip_allow))
}

async fn strip_allow(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut resp = next.run(req).await;
    if matches!(
        resp.status(),
        StatusCode::NOT_FOUND | StatusCode::INTERNAL_SERVER_ERROR
    ) {
        resp.headers_mut().remove(header::ALLOW);
    }
    resp
}

/// V2 has catch-alls only for `GET` and `POST` (404 with a QErr); any other unmatched
/// method ends in its error handler, which always answers 500.
async fn not_found(method: Method) -> Response {
    let (status, err) = if method == Method::GET || method == Method::POST {
        (
            StatusCode::NOT_FOUND,
            Error::new(Code::NotFound, "resource does not exist"),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Error::new(Code::Unexpected, "Internal Server Error"),
        )
    };
    let mut resp = json_response(status, err.to_api_json(false).to_string(), None);
    // V2 logs the error, with a fresh request id it does not return.
    let log = super::http::wanted().then(|| {
        let id = uuid::Uuid::new_v4().to_string();
        super::http::HttpLogInfo::start(&id).failed(&err, super::http::RequestMode::Error)
    });
    super::http::attach(&mut resp, log);
    resp
}

/// How long `/healthz` waits for the metadata database before reporting an error, so probes
/// get an answer instead of timing out.
const HEALTH_DB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn text(status: StatusCode, body: &'static str) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// `GET /healthz?strict=<bool>`: the metadata database must answer; inconsistent metadata
/// is a warning, or an error in strict mode.
async fn healthz(State(engine): State<Arc<Engine>>, uri: Uri) -> Response {
    if let Some(pool) = &engine.metadata_db {
        let check = async {
            match pool.get().await {
                Ok(client) => client.simple_query("SELECT 1").await.is_ok(),
                Err(_) => false,
            }
        };
        let ok = tokio::time::timeout(HEALTH_DB_TIMEOUT, check)
            .await
            .unwrap_or(false);
        if !ok {
            return text(StatusCode::INTERNAL_SERVER_ERROR, "ERROR");
        }
    }
    let strict = strict_param(uri.query());
    if engine.state.load().inconsistencies.is_empty() {
        text(StatusCode::OK, "OK")
    } else if strict {
        text(
            StatusCode::INTERNAL_SERVER_ERROR,
            "ERROR: inconsistent objects in schema",
        )
    } else {
        text(StatusCode::OK, "WARN: inconsistent objects in schema")
    }
}

/// `strict` as a boolean (the first occurrence; anything unparsable is false, as V2).
fn strict_param(query: Option<&str>) -> bool {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|kv| match kv.split_once('=') {
            Some(("strict", v)) => Some(v),
            _ if kv == "strict" => Some(""),
            _ => None,
        })
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

async fn version() -> Response {
    json_response(
        StatusCode::OK,
        json!({"server_type": "ce", "version": VERSION}).to_string(),
        None,
    )
}

/// V2's `onlyAdmin`: the endpoint is for the admin role only.
async fn only_admin(engine: &Engine, headers: &HeaderMap) -> Result<(), Response> {
    match engine.auth.authenticate(headers, None).await {
        Ok(auth) if auth.session.role.is_admin() => Ok(()),
        Ok(_) => {
            let err = Error::new(
                Code::AccessDenied,
                "You have to be an admin to access this endpoint",
            );
            Err(json_response(
                StatusCode::BAD_REQUEST,
                err.to_api_json(false).to_string(),
                None,
            ))
        }
        Err(e) => Err(json_response(
            e.status,
            e.error.to_api_json(false).to_string(),
            None,
        )),
    }
}

/// `GET /v1alpha1/config` (admin only).
async fn config(State(engine): State<Arc<Engine>>, headers: HeaderMap) -> Response {
    if let Err(resp) = only_admin(&engine, &headers).await {
        return resp;
    }
    json_response(StatusCode::OK, config_json(&engine).to_string(), None)
}

fn config_json(engine: &Engine) -> Value {
    let c = &engine.config;
    let poller = |o: &hge_subscriptions::poller::PollerOptions| {
        json!({
            "batch_size": o.batch_size,
            "refetch_delay": o.refetch_interval.as_secs_f64(),
        })
    };
    let no_auth = c.admin_secrets.is_empty()
        && c.jwt_secret.is_none()
        && c.jwt_secrets.is_none()
        && c.auth_hook.is_none();
    let jwt = jwt_info(c.jwt_secret.as_deref(), c.jwt_secrets.as_deref());
    json!({
        "version": VERSION,
        "is_function_permissions_inferred": c.infer_function_permissions,
        "is_remote_schema_permissions_enabled": false,
        "is_admin_secret_set": !no_auth,
        "is_auth_hook_set": c.auth_hook.is_some(),
        "is_jwt_set": !jwt.is_empty(),
        "jwt": jwt,
        "is_allow_list_enabled": c.enable_allowlist,
        "live_queries": poller(&c.websocket.live_queries),
        "streaming_queries": poller(&c.websocket.streaming),
        "console_assets_dir": null,
        "experimental_features": [],
        "is_prometheus_metrics_enabled": c.api_enabled("metrics"),
        "default_naming_convention": "hasura-default",
        "is_apollo_federation_enabled": false,
    })
}

/// The `jwt` entries of `/v1alpha1/config`: namespace (or namespace path), format and claims
/// map of each configured secret; keys are never shown.
fn jwt_info(single: Option<&str>, list: Option<&str>) -> Vec<Value> {
    let configs = single
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .into_iter()
        .chain(
            list.and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
                .unwrap_or_default(),
        );
    configs
        .map(|c| {
            let claims_map = c.get("claims_map").filter(|m| !m.is_null()).cloned();
            let default_ns = Value::from("https://hasura.io/jwt/claims");
            let (namespace, format) = if claims_map.is_some() {
                (default_ns, Value::from("json"))
            } else {
                let ns = c
                    .get("claims_namespace_path")
                    .or_else(|| c.get("claims_namespace"))
                    .filter(|v| v.is_string())
                    .cloned()
                    .unwrap_or(default_ns);
                let format = c
                    .get("claims_format")
                    .filter(|v| v.is_string())
                    .cloned()
                    .unwrap_or_else(|| "json".into());
                (ns, format)
            };
            json!({
                "claims_namespace": namespace,
                "claims_format": format,
                "claims_map": claims_map,
            })
        })
        .collect()
}

/// `GET /dev/ekg`: V2 dumps its runtime counters here; the content is implementation
/// specific.
async fn dev_ekg(State(engine): State<Arc<Engine>>, headers: HeaderMap) -> Response {
    if let Err(resp) = only_admin(&engine, &headers).await {
        return resp;
    }
    let body = json!({
        "uptime_seconds": super::uptime(),
        "websocket_connections": engine.ws.connection_count(),
        "live_query_pollers": engine.ws.live_queries.poller_count(),
        "streaming_query_pollers": engine.ws.streaming.poller_count(),
        "inconsistent_objects": engine.state.load().inconsistencies.len(),
    });
    json_response(StatusCode::OK, body.to_string(), None)
}

/// `GET /dev/plan_cache`: deprecated in V2, always `null`.
async fn dev_plan_cache(State(engine): State<Arc<Engine>>, headers: HeaderMap) -> Response {
    if let Err(resp) = only_admin(&engine, &headers).await {
        return resp;
    }
    json_response(StatusCode::OK, "null".to_owned(), None)
}

/// `GET /dev/subscriptions[/extended]`: the subscription pollers.
async fn dev_subscriptions(State(engine): State<Arc<Engine>>, headers: HeaderMap) -> Response {
    if let Err(resp) = only_admin(&engine, &headers).await {
        return resp;
    }
    let registry = |r: &hge_subscriptions::poller::Registry| {
        json!({
            "options": {
                "batch_size": r.options().batch_size,
                "refetch_delay": r.options().refetch_interval.as_secs_f64(),
            },
            "pollers": r.poller_count(),
            "cohorts": r.cohort_count(),
        })
    };
    let body = json!({
        "live_queries": registry(&engine.ws.live_queries),
        "streaming_queries": registry(&engine.ws.streaming),
    });
    json_response(StatusCode::OK, body.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict() {
        assert!(strict_param(Some("strict=true")));
        assert!(strict_param(Some("a=1&strict=TRUE&strict=false")));
        assert!(!strict_param(Some("strict=1")));
        assert!(!strict_param(Some("strict")));
        assert!(!strict_param(None));
    }

    #[test]
    fn jwt_entries() {
        let entries = jwt_info(
            Some(r#"{"type":"HS256","key":"k","claims_namespace_path":"$.hasura"}"#),
            Some(
                r#"[{"type":"HS256","key":"k","claims_format":"stringified_json"},
                    {"type":"HS256","key":"k","claims_namespace":"x","claims_map":{"x-hasura-allowed-roles":["user"]}}]"#,
            ),
        );
        assert_eq!(
            Value::from(entries),
            json!([
                {"claims_namespace": "$.hasura", "claims_format": "json", "claims_map": null},
                {"claims_namespace": "https://hasura.io/jwt/claims", "claims_format": "stringified_json", "claims_map": null},
                {"claims_namespace": "https://hasura.io/jwt/claims", "claims_format": "json", "claims_map": {"x-hasura-allowed-roles": ["user"]}},
            ])
        );
        assert!(jwt_info(None, None).is_empty());
    }
}
