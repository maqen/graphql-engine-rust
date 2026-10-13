//! `/api/rest/<url>`: RESTified endpoints (spec 10 §5, spec 13 §3.3).
//!
//! Any method; not gated by `HASURA_GRAPHQL_ENABLED_APIS`. Errors are QErr bodies with their
//! real HTTP status; success is the GraphQL `data` object without the envelope.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use hge_core::{Error, Session};
use hge_rest::{RequestMethod, parse_query_string};

use crate::graphql;
use crate::routes::{json_response, request_id};
use crate::state::Engine;

pub async fn handler(
    State(engine): State<Arc<Engine>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id(&headers);
    // GET-style authentication: the webhook never sees the body.
    let auth = match engine.auth.authenticate(&headers, None).await {
        Ok(auth) => auth,
        Err(e) => return error_response(&e.error, e.status, false, rid),
    };
    let session = auth.session;
    let include_internal = graphql::include_internal(&engine, &session);
    let mut resp = match run(&engine, &session, &headers, &method, &uri, &body).await {
        Ok((data, cookies)) => {
            let mut resp = json_response(StatusCode::OK, data, Some(rid));
            for cookie in cookies {
                if let Ok(v) = HeaderValue::from_str(&cookie) {
                    resp.headers_mut().append(header::SET_COOKIE, v);
                }
            }
            resp
        }
        Err(e) => {
            let status =
                StatusCode::from_u16(e.code.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
            error_response(&e, status, include_internal, rid)
        }
    };
    for cookie in auth.set_cookies {
        resp.headers_mut().append(header::SET_COOKIE, cookie);
    }
    resp
}

fn error_response(e: &Error, status: StatusCode, include_internal: bool, rid: String) -> Response {
    json_response(
        status,
        e.to_api_json(include_internal).to_string(),
        Some(rid),
    )
}

/// The response body (`data` without the envelope) and the `Set-Cookie` values of
/// mutation actions.
async fn run(
    engine: &Engine,
    session: &Session,
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
    body: &[u8],
) -> Result<(String, Vec<String>), Error> {
    let state = engine.state.load_full();
    // The raw path keeps percent-escapes, so an encoded `/` stays inside its segment.
    let path = uri.path().strip_prefix("/api/rest").unwrap_or_default();
    let (endpoint, path_vars) = state
        .prepared
        .rest
        .lookup(RequestMethod::parse(method.as_str()), path)?;
    let query = parse_query_string(uri.query().unwrap_or_default());
    // Only a JSON object body contributes variables; anything else is ignored.
    let body = (!body.is_empty())
        .then(|| serde_json::from_slice::<serde_json::Value>(body).ok())
        .flatten();
    let variables = endpoint.variables(
        &query,
        body.as_ref().and_then(|b| b.as_object()),
        &path_vars,
    )?;
    crate::policies::check_allowlist(&state, session, &endpoint.document, &endpoint.query)?;
    let rs = state.role_schema(&session.role, session.backend_only);
    let op = hge_schema::plan(&rs, &endpoint.document, None, &variables)?;
    match graphql::execute(engine, &state, session, headers, Some(&endpoint.query), &op).await {
        Ok((body, cookies)) => Ok((strip_data_envelope(body), cookies)),
        Err(graphql::Failure::Error(e)) => Err(e),
        // A remote schema's errors, as the GraphQL endpoint returns them.
        Err(graphql::Failure::Remote(errors)) => Ok((
            serde_json::json!({ "data": null, "errors": errors }).to_string(),
            vec![],
        )),
    }
}

/// `{"data":X}` -> `X`
fn strip_data_envelope(mut body: String) -> String {
    const PREFIX: &str = "{\"data\":";
    if body.starts_with(PREFIX) && body.ends_with('}') {
        body.truncate(body.len() - 1);
        body.drain(..PREFIX.len());
    }
    body
}
