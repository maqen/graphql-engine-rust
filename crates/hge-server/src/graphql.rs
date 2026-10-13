//! `POST /v1/graphql` (single and batched requests) and `GET /v1/graphql`.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use hge_core::{Code, Error, Session};
use hge_ir::{Mutation, Operation, OperationKind, QueryRoot, RootField, SourceExecutor};
use http::HeaderMap;
use serde::Deserialize;
use serde_json::{Map, Value as Json};

use crate::state::{AppState, Engine};

/// One GraphQL request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphQLRequest {
    pub query: Option<String>,
    #[serde(default)]
    pub variables: Option<Json>,
    #[serde(default)]
    pub operation_name: Option<String>,
    /// Received over `GET`: mutations are refused (they would be open to CSRF).
    #[serde(skip)]
    pub read_only: bool,
}

/// A request body: one request or a batch.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum GraphQLBody {
    Single(GraphQLRequest),
    Batch(Vec<GraphQLRequest>),
}

/// A response body plus headers to add to the HTTP response.
#[derive(Debug, Default)]
pub struct GraphQLResponse {
    pub body: String,
    /// `Set-Cookie` values from mutation action handlers.
    pub set_cookies: Vec<String>,
}

/// Execute a request body and return the response body.
pub async fn execute_body(
    engine: &Engine,
    session: &Session,
    headers: &HeaderMap,
    body: GraphQLBody,
) -> GraphQLResponse {
    let state = engine.state.load_full();
    match body {
        GraphQLBody::Single(req) => {
            let (body, set_cookies) = execute_one(engine, &state, session, headers, req).await;
            GraphQLResponse { body, set_cookies }
        }
        // No response headers are forwarded for batched requests.
        GraphQLBody::Batch(reqs) => {
            let mut out = String::from("[");
            for (i, req) in reqs.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&execute_one(engine, &state, session, headers, req).await.0);
            }
            out.push(']');
            GraphQLResponse {
                body: out,
                set_cookies: vec![],
            }
        }
    }
}

/// The response body, plus `Set-Cookie` values (only for a successful response).
async fn execute_one(
    engine: &Engine,
    state: &Arc<AppState>,
    session: &Session,
    headers: &HeaderMap,
    req: GraphQLRequest,
) -> (String, Vec<String>) {
    match run(engine, state, session, headers, req).await {
        Ok(out) => out,
        Err(e) => (error_body(&e, include_internal(engine, session)), vec![]),
    }
}

/// The engine's sources, for action relationships.
struct StateSources<'a>(&'a AppState);

impl hge_actions::Sources for StateSources<'_> {
    fn executor(&self, source: &str) -> Option<&dyn SourceExecutor> {
        source_executor(self.0, source).ok()
    }
}

pub fn include_internal(engine: &Engine, session: &Session) -> bool {
    engine.config.dev_mode || (engine.config.admin_internal_errors && session.is_admin())
}

pub fn error_body(e: &Error, include_internal: bool) -> String {
    let body = serde_json::json!({ "errors": [e.to_graphql_json(include_internal)] });
    body.to_string()
}

async fn run(
    engine: &Engine,
    state: &AppState,
    session: &Session,
    client_headers: &HeaderMap,
    req: GraphQLRequest,
) -> Result<(String, Vec<String>), Error> {
    let query = req.query.clone();
    let op = plan(state, session, req)?;
    if op.kind == OperationKind::Subscription {
        return Err(Error::new(
            Code::UnexpectedPayload,
            "subscriptions are not supported over HTTP, use websockets instead",
        ));
    }
    execute(
        engine,
        state,
        session,
        client_headers,
        query.as_deref(),
        &op,
    )
    .await
}

/// Parse, validate and lower a request (everything that can fail before execution).
pub fn plan(state: &AppState, session: &Session, req: GraphQLRequest) -> Result<Operation, Error> {
    let query = req.query.as_deref().ok_or_else(|| {
        Error::new(Code::BadRequest, "the key 'query' was not present").with_path("$")
    })?;
    let variables = match req.variables {
        None | Some(Json::Null) => Map::new(),
        Some(Json::Object(m)) => m,
        Some(_) => {
            return Err(Error::validation("variables must be an object").with_path("$.variables"));
        }
    };
    let rs = state.role_schema(&session.role, session.backend_only);
    let doc = hge_schema::parse_query(query)?;
    let op = hge_schema::plan(&rs, &doc, req.operation_name.as_deref(), &variables)?;
    if req.read_only && op.kind == hge_ir::OperationKind::Mutation {
        return Err(Error::new(
            Code::NotSupported,
            "mutations are not supported over GET requests",
        ));
    }
    Ok(op)
}

/// Execute a planned query or mutation; returns the response body (`{"data":{...}}`) and
/// the `Set-Cookie` values of mutation actions. `query` is the request's query text, which
/// action handlers receive.
pub async fn execute(
    engine: &Engine,
    state: &AppState,
    session: &Session,
    client_headers: &HeaderMap,
    query: Option<&str>,
    op: &Operation,
) -> Result<(String, Vec<String>), Error> {
    let ctx = hge_actions::RequestContext {
        session,
        client_headers,
        query,
        action_log: engine.action_log.as_deref(),
        include_internal: include_internal(engine, session),
    };
    let sources = StateSources(state);
    let mut set_cookies = vec![];
    let results = if op.kind == OperationKind::Mutation {
        run_mutations(state, session, op, &ctx, &sources, &mut set_cookies).await?
    } else {
        run_concurrently(state, session, &op.root_fields, &ctx, &sources).await?
    };

    let mut body =
        String::with_capacity(64 + results.iter().flatten().map(String::len).sum::<usize>());
    body.push_str("{\"data\":{");
    for (i, (alias, rf)) in op.root_fields.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        write!(body, "{}:", Json::String(alias.clone())).expect("write to string");
        match rf {
            RootField::Static(v) => body.push_str(&v.to_string()),
            RootField::Database(_) | RootField::Mutation(_) | RootField::Action(_) => {
                body.push_str(results[i].as_deref().unwrap_or("null"));
            }
        }
    }
    body.push_str("}}");
    Ok((body, set_cookies))
}

fn source_executor<'a>(state: &'a AppState, source: &str) -> Result<&'a dyn SourceExecutor, Error> {
    state
        .sources
        .get(source)
        .map(|s| s.as_ref() as &dyn SourceExecutor)
        .ok_or_else(|| Error::unexpected(format!("source \"{source}\" is not available")))
}

/// Query root fields: database fields grouped into one round trip per source, and actions,
/// all concurrently. Query actions' `Set-Cookie` headers are discarded.
async fn run_concurrently(
    state: &AppState,
    session: &Session,
    root_fields: &[(String, RootField)],
    ctx: &hge_actions::RequestContext<'_>,
    sources: &StateSources<'_>,
) -> Result<Vec<Option<String>>, Error> {
    let mut by_source: HashMap<&str, Vec<(usize, &QueryRoot)>> = HashMap::new();
    for (i, (_, rf)) in root_fields.iter().enumerate() {
        if let RootField::Database(db) = rf {
            by_source
                .entry(&db.source)
                .or_default()
                .push((i, &db.query));
        }
    }
    let mut results: Vec<Option<String>> = vec![None; root_fields.len()];
    let futures = by_source.into_iter().map(|(source, fields)| async move {
        let executor = source_executor(state, source)?;
        let roots: Vec<&QueryRoot> = fields.iter().map(|(_, q)| *q).collect();
        let out = executor.execute_query(&roots, session).await?;
        Ok::<_, Error>(
            fields
                .into_iter()
                .map(|(i, _)| i)
                .zip(out)
                .collect::<Vec<_>>(),
        )
    });
    let action_futures = root_fields
        .iter()
        .enumerate()
        .filter_map(|(i, (_, rf))| match rf {
            RootField::Action(a) => Some((i, a)),
            _ => None,
        })
        .map(|(i, a)| async move {
            hge_actions::execute(&state.action_registry, a, ctx, sources)
                .await
                .map(|r| (i, r.value))
        });
    let (db_results, action_results) = futures::future::join(
        futures::future::join_all(futures),
        futures::future::join_all(action_futures),
    )
    .await;
    for res in db_results {
        for (i, json) in res? {
            results[i] = Some(json);
        }
    }
    for res in action_results {
        let (i, value) = res?;
        results[i] = Some(value.to_string());
    }
    Ok(results)
}

/// Run the root fields of a mutation operation in order; the first error stops the
/// operation (spec 03 §1). Like V2, when every root field is a database mutation on the same
/// source they share one transaction (a failure rolls back all of them); otherwise each runs
/// on its own. Action handlers' `Set-Cookie` headers are collected into `set_cookies`.
async fn run_mutations(
    state: &AppState,
    session: &Session,
    op: &Operation,
    ctx: &hge_actions::RequestContext<'_>,
    sources: &StateSources<'_>,
    set_cookies: &mut Vec<String>,
) -> Result<Vec<Option<String>>, Error> {
    let mut results: Vec<Option<String>> = vec![None; op.root_fields.len()];
    let mutations: Vec<(&str, &Mutation)> = op
        .root_fields
        .iter()
        .filter_map(|(_, rf)| match rf {
            RootField::Mutation(m) => Some((&*m.source, &m.mutation)),
            _ => None,
        })
        .collect();
    if let Some(&(first_source, _)) = mutations.first()
        && mutations.len() == op.root_fields.len()
        && mutations.iter().all(|(source, _)| *source == first_source)
    {
        let batch: Vec<&Mutation> = mutations.iter().map(|(_, m)| *m).collect();
        let out = source_executor(state, first_source)?
            .execute_mutations(&batch, session)
            .await?;
        for (slot, json) in results.iter_mut().zip(out) {
            *slot = Some(json);
        }
        return Ok(results);
    }
    for (slot, (_, rf)) in results.iter_mut().zip(&op.root_fields) {
        *slot = match rf {
            RootField::Static(_) => None,
            RootField::Database(db) => source_executor(state, &db.source)?
                .execute_query(&[&db.query], session)
                .await?
                .pop(),
            RootField::Mutation(m) => source_executor(state, &m.source)?
                .execute_mutations(&[&m.mutation], session)
                .await?
                .pop(),
            RootField::Action(a) => {
                let r = hge_actions::execute(&state.action_registry, a, ctx, sources).await?;
                set_cookies.extend(r.set_cookies);
                Some(r.value.to_string())
            }
        };
    }
    Ok(results)
}
