//! `POST /v1/graphql/explain` and `/v1alpha1/graphql/explain` (spec 10 §3.7, spec 09 §11.4):
//! the SQL the engine would run for a query or subscription, and Postgres' plan for it.
//!
//! The SQL is this engine's, so it differs from V2's text; the response shapes and errors are
//! V2's. A subscription's `variables` keeps V2's keys, with `synthetic` holding this engine's
//! cohort values (every value of the multiplexed query, session variables included).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use hge_core::{Code, Error, RoleName, Session, SessionVariables};
use hge_ir::{OperationKind, RootField, SourceExecutor};
use serde::Deserialize;
use serde_json::{Map, Value as Json, json};

use crate::graphql::{self, GraphQLRequest};
use crate::routes::{json_response, request_id};
use crate::state::{AppState, Engine};
use crate::ws::{Plan, subscription_plan};

#[derive(Deserialize)]
struct ExplainRequest {
    query: GraphQLRequest,
    #[serde(default)]
    user: Option<Map<String, Json>>,
    /// The Relay schema; the same schema is used here.
    #[serde(default)]
    #[allow(dead_code)]
    is_relay: Option<bool>,
}

pub async fn handler(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = Some(request_id(&headers));
    match engine.auth.authenticate(&headers, None).await {
        Ok(auth) if auth.session.role.is_admin() => {}
        Ok(_) => {
            let e = Error::new(
                Code::AccessDenied,
                "You have to be an admin to access this endpoint",
            );
            // V2 answers 400 here, not the code's usual 401.
            return json_response(
                StatusCode::BAD_REQUEST,
                e.to_api_json(false).to_string(),
                rid,
            );
        }
        Err(e) => {
            return json_response(e.status, e.error.to_api_json(false).to_string(), rid);
        }
    }
    match explain(&engine, &body).await {
        Ok(v) => json_response(StatusCode::OK, v.to_string(), rid),
        Err(e) => error_response(&e, rid),
    }
}

/// V2 raises every explain error with status 400, except unexpected (internal) ones.
fn error_response(e: &Error, rid: Option<String>) -> Response {
    let status = if e.code == Code::Unexpected {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::BAD_REQUEST
    };
    json_response(status, e.to_api_json(false).to_string(), rid)
}

async fn explain(engine: &Engine, body: &[u8]) -> Result<Json, Error> {
    let body: Json = serde_json::from_slice(body)
        .map_err(|e| Error::new(Code::InvalidJson, e.to_string()).with_path("$"))?;
    if body.get("query").is_none() {
        return Err(Error::new(Code::ParseFailed, "key \"query\" not found").with_path("$"));
    }
    let req: ExplainRequest = serde_json::from_value(body)
        .map_err(|e| Error::new(Code::ParseFailed, e.to_string()).with_path("$"))?;
    let session = session(req.user.as_ref())?;
    let variables = req.query.variables.clone().unwrap_or(Json::Null);
    let state = engine.state.load_full();
    let op = graphql::plan(&state, &session, req.query)?;
    match op.kind {
        OperationKind::Mutation => Err(Error::new(
            Code::InvalidParams,
            "only queries can be explained",
        )),
        OperationKind::Query => explain_query(&state, &session, &op).await,
        OperationKind::Subscription => explain_subscription(&state, &session, &op, variables).await,
    }
}

/// The session of the `user` map: its role (`admin` when absent) and variables.
fn session(user: Option<&Map<String, Json>>) -> Result<Session, Error> {
    let Some(user) = user else {
        return Ok(Session::admin());
    };
    let mut variables = SessionVariables::new();
    for (k, v) in user {
        let Json::String(v) = v else {
            return Err(Error::new(
                Code::ParseFailed,
                "parsing Text failed, expected String, but encountered another type",
            )
            .with_path(format!("$.user.{k}")));
        };
        variables.insert(k, v.as_str());
    }
    let role = variables
        .get("x-hasura-role")
        .map(RoleName::new)
        .unwrap_or_else(RoleName::admin);
    Ok(Session {
        role,
        variables,
        backend_only: false,
    })
}

fn executor(state: &AppState, source: &str) -> Result<Arc<dyn SourceExecutor>, Error> {
    Ok(state
        .sources
        .get(source)
        .ok_or_else(|| Error::unexpected(format!("source \"{source}\" is not available")))?
        .clone())
}

async fn explain_query(
    state: &AppState,
    session: &Session,
    op: &hge_ir::Operation,
) -> Result<Json, Error> {
    // Check every field before running anything, like V2.
    for (_, rf) in &op.root_fields {
        match rf {
            RootField::Remote(_) => {
                return Err(Error::new(
                    Code::InvalidParams,
                    "only hasura queries can be explained",
                ));
            }
            RootField::Action(_) => {
                return Err(Error::new(
                    Code::InvalidParams,
                    "query actions cannot be explained",
                ));
            }
            _ => {}
        }
    }
    let mut out = vec![];
    for (alias, rf) in &op.root_fields {
        let (sql, plan) = match rf {
            RootField::Database(db) => {
                let (sql, plan) = executor(state, &db.source)?
                    .explain_query(&db.query, session)
                    .await?;
                (json!(sql), json!(plan))
            }
            _ => (Json::Null, Json::Null),
        };
        out.push(json!({"field": alias, "sql": sql, "plan": plan}));
    }
    Ok(Json::Array(out))
}

async fn explain_subscription(
    state: &AppState,
    session: &Session,
    op: &hge_ir::Operation,
    variables: Json,
) -> Result<Json, Error> {
    let (source, fields) = match subscription_plan(op)? {
        Plan::Live { source, fields, .. } => (source, fields),
        Plan::AsyncActions { live: false, .. } => {
            return Err(Error::new(
                Code::NotSupported,
                "async action query fields without relationships to table cannot be explained",
            ));
        }
        // V2 explains these as a live query over the action log; they run differently here.
        Plan::AsyncActions { live: true, .. } => {
            return Err(Error::new(
                Code::NotSupported,
                "async action query fields with relationships cannot be explained",
            ));
        }
    };
    let executor = executor(state, source)?;
    let query = executor.multiplexed_query(&fields, session)?;
    let synthetic = Json::from(
        query
            .variables
            .iter()
            .map(|v| v.clone().map_or(Json::Null, Json::String))
            .collect::<Vec<_>>(),
    );
    let cohorts = Json::Array(vec![synthetic.clone()]).to_string();
    let plan = executor.explain_multiplexed(&query.sql, &cohorts).await?;
    let query_vars = match variables {
        Json::Null => json!({}),
        v => v,
    };
    Ok(json!({
        "sql": query.sql,
        "plan": plan,
        "variables": {
            "session": session.variables.to_json(),
            "query": query_vars,
            "synthetic": synthetic,
            "cursor": {},
        },
    }))
}
