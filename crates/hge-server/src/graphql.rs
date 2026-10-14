//! `POST /v1/graphql` (single and batched requests) and `GET /v1/graphql`.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use hge_core::{Code, Error, Session};
use hge_ir::{Mutation, Operation, OperationKind, QueryRoot, RootField, SourceExecutor};
use hge_remote_schema::{RemoteError, RemoteSettings, RequestContext};
use http::HeaderMap;
use serde::Deserialize;
use serde_json::{Map, Value as Json};

use crate::response_cache;
use crate::state::{AppState, Engine, SchemaKey};
use crate::telemetry::{metrics, query_log};

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
    /// `Set-Cookie` values from mutation action handlers and remote schemas.
    pub set_cookies: Vec<String>,
    /// The error of a single request (for `http-log`).
    pub error: Option<Error>,
    /// Per operation of a batch: its response size and time, or its error (`http-log`'s
    /// `batched_operations`).
    pub batch: Vec<(usize, f64, Option<Error>)>,
}

/// Execute a request body and return the response body. `request_id` is for `query-log`.
pub async fn execute_body(
    engine: &Engine,
    session: &Session,
    headers: &HeaderMap,
    request_id: &str,
    body: GraphQLBody,
) -> GraphQLResponse {
    let state = engine.state.load_full();
    match body {
        GraphQLBody::Single(req) => {
            let (body, set_cookies, error) =
                execute_one(engine, &state, session, headers, request_id, req).await;
            GraphQLResponse {
                body,
                set_cookies,
                error,
                batch: vec![],
            }
        }
        // No response headers are forwarded for batched requests.
        GraphQLBody::Batch(reqs) => {
            let mut out = String::from("[");
            let mut batch = Vec::with_capacity(reqs.len());
            for (i, req) in reqs.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let started = std::time::Instant::now();
                let (body, _, error) =
                    execute_one(engine, &state, session, headers, request_id, req).await;
                batch.push((body.len(), started.elapsed().as_secs_f64(), error));
                out.push_str(&body);
            }
            out.push(']');
            GraphQLResponse {
                body: out,
                set_cookies: vec![],
                error: None,
                batch,
            }
        }
    }
}

/// Why executing a request failed.
#[derive(Debug)]
pub enum Failure {
    Error(Error),
    /// A remote schema's GraphQL errors: `{"data": null, "errors": [...]}` (spec 08 §6.5).
    Remote(Vec<Json>),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Error(e)
    }
}

impl From<RemoteError> for Failure {
    fn from(e: RemoteError) -> Self {
        match e {
            RemoteError::Hasura(e) => Failure::Error(e),
            RemoteError::Graphql(errors) => Failure::Remote(errors),
        }
    }
}

/// The response body, plus `Set-Cookie` values (only for a successful response) and the
/// error.
async fn execute_one(
    engine: &Engine,
    state: &Arc<AppState>,
    session: &Session,
    headers: &HeaderMap,
    request_id: &str,
    req: GraphQLRequest,
) -> (String, Vec<String>, Option<Error>) {
    let started = std::time::Instant::now();
    let mut op_type = metrics::OperationType::Unknown;
    let result = run(
        engine,
        state,
        session,
        headers,
        request_id,
        req,
        &mut op_type,
    )
    .await;
    metrics::graphql_request(op_type, result.is_ok(), started.elapsed());
    match result {
        Ok((body, cookies)) => (body, cookies, None),
        Err(Failure::Error(e)) => {
            let body = error_body(&e, include_internal(engine, session));
            (body, vec![], Some(e))
        }
        Err(Failure::Remote(errors)) => (
            serde_json::json!({ "data": null, "errors": errors }).to_string(),
            vec![],
            None,
        ),
    }
}

/// Database access for action relationships (also used by async action subscriptions).
pub(crate) struct StateSources<'a>(pub(crate) &'a AppState);

impl hge_actions::Sources for StateSources<'_> {
    fn executor(&self, source: &str) -> Option<&dyn SourceExecutor> {
        source_executor(self.0, source).ok()
    }
}

pub fn remote_settings(engine: &Engine) -> RemoteSettings {
    RemoteSettings {
        prioritize_data: engine.config.remote_schema_prioritize_data,
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
    request_id: &str,
    req: GraphQLRequest,
    op_type: &mut metrics::OperationType,
) -> Result<(String, Vec<String>), Failure> {
    // Introspection-only responses are cached per schema. A cached response skips planning,
    // so with `query-log` on every request is planned (and logged) instead.
    let logging = query_log::enabled();
    let schema_key = state.schema_key(&session.role, session.backend_only);
    let cache_key = cache_key(state, session, &schema_key, &req);
    if let Some(key) = cache_key.as_ref().filter(|_| !logging)
        && let Some(body) = state.responses.get(key)
    {
        *op_type = metrics::OperationType::Query;
        return Ok((body.to_string(), vec![]));
    }
    let op = plan_request(state, session, &req)?;
    *op_type = op.kind.into();
    if op.kind == OperationKind::Subscription {
        return Err(Error::new(
            Code::UnexpectedPayload,
            "subscriptions are not supported over HTTP, use websockets instead",
        )
        .into());
    }
    if logging {
        let request = query_log::request(
            req.query.as_deref().unwrap_or_default(),
            req.operation_name.as_deref(),
            req.variables.as_ref(),
        );
        query_log::log(state, session, request_id, &request, &op);
    }
    let query = req.query.as_deref();
    let (body, cookies) = execute(engine, state, session, client_headers, query, &op).await?;
    if let Some(key) = &cache_key {
        state.responses.insert_if_static(key, &op, &body);
    }
    Ok((body, cookies))
}

/// The response cache key of a request that may be answered from the cache: one without
/// variables, with a short query, that the allowlist does not apply to (it is checked on the
/// parsed query, so those requests always take the full path).
fn cache_key<'a>(
    state: &AppState,
    session: &Session,
    schema: &'a SchemaKey,
    req: &'a GraphQLRequest,
) -> Option<response_cache::Key<'a>> {
    let no_variables = match &req.variables {
        None | Some(Json::Null) => true,
        Some(Json::Object(m)) => m.is_empty(),
        Some(_) => false,
    };
    let query = req.query.as_deref()?;
    let allowlisted = state.prepared.allowlist.is_some() && !session.role.is_admin();
    (no_variables && !allowlisted && query.len() <= response_cache::MAX_QUERY).then_some(
        response_cache::Key {
            schema,
            query,
            operation_name: req.operation_name.as_deref(),
        },
    )
}

/// Parse, validate and lower a request (everything that can fail before execution).
pub fn plan(state: &AppState, session: &Session, req: GraphQLRequest) -> Result<Operation, Error> {
    plan_request(state, session, &req)
}

fn plan_request(
    state: &AppState,
    session: &Session,
    req: &GraphQLRequest,
) -> Result<Operation, Error> {
    let query = req.query.as_deref().ok_or_else(|| {
        Error::new(Code::BadRequest, "the key 'query' was not present").with_path("$")
    })?;
    let no_variables = Map::new();
    let variables = match &req.variables {
        None | Some(Json::Null) => &no_variables,
        Some(Json::Object(m)) => m,
        Some(_) => {
            return Err(Error::validation("variables must be an object").with_path("$.variables"));
        }
    };
    let rs = state.role_schema(&session.role, session.backend_only);
    let doc = hge_schema::parse_query(query)?;
    crate::policies::check_allowlist(state, session, &doc, query)?;
    let op = hge_schema::plan(&rs, &doc, req.operation_name.as_deref(), variables)?;
    if req.read_only && op.kind == OperationKind::Mutation {
        return Err(Error::new(
            Code::NotSupported,
            "mutations are not supported over GET requests",
        ));
    }
    Ok(op)
}

/// Execute a planned query or mutation; returns the response body (`{"data":{...}}`) and
/// the `Set-Cookie` values of mutation actions and remote schemas. `query` is the request's
/// query text, which action handlers receive.
pub async fn execute(
    engine: &Engine,
    state: &AppState,
    session: &Session,
    client_headers: &HeaderMap,
    query: Option<&str>,
    op: &Operation,
) -> Result<(String, Vec<String>), Failure> {
    let ctx = Context {
        session,
        actions: hge_actions::RequestContext {
            session,
            client_headers,
            query,
            action_log: engine.action_log.as_deref(),
            include_internal: include_internal(engine, session),
        },
        sources: StateSources(state),
        remote: RequestContext {
            session,
            client_headers,
        },
        settings: remote_settings(engine),
    };
    let mut set_cookies = vec![];
    let results = if op.kind == OperationKind::Mutation {
        run_mutations(state, op, &ctx, &mut set_cookies).await?
    } else {
        run_concurrently(state, &op.root_fields, &ctx, &mut set_cookies).await?
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
            RootField::Database(_)
            | RootField::Mutation(_)
            | RootField::Action(_)
            | RootField::Remote(_) => {
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

/// What executing root fields needs besides the state.
struct Context<'a> {
    session: &'a Session,
    actions: hge_actions::RequestContext<'a>,
    sources: StateSources<'a>,
    remote: RequestContext<'a>,
    settings: RemoteSettings,
}

/// Query root fields: database fields grouped into one round trip per source, actions and
/// remote schema fields, all concurrently. Errors are reported in root field order. Query
/// actions' `Set-Cookie` headers are discarded (remote schemas' are kept, like V2).
async fn run_concurrently(
    state: &AppState,
    root_fields: &[(String, RootField)],
    ctx: &Context<'_>,
    set_cookies: &mut Vec<String>,
) -> Result<Vec<Option<String>>, Failure> {
    let session = ctx.session;
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
        // A source's error is reported at its first field.
        let first = fields.first().map_or(0, |(i, _)| *i);
        let run = async {
            let executor = source_executor(state, source)?;
            let roots: Vec<&QueryRoot> = fields.iter().map(|(_, q)| *q).collect();
            executor.execute_query(&roots, session).await
        };
        match run.await {
            Ok(out) => Ok(fields.iter().map(|(i, _)| *i).zip(out).collect::<Vec<_>>()),
            Err(e) => Err((first, e)),
        }
    });
    let remote_futures = root_fields
        .iter()
        .enumerate()
        .filter_map(|(i, (_, rf))| match rf {
            RootField::Remote(r) => Some((i, r)),
            _ => None,
        })
        .map(|(i, r)| async move {
            let mut cookies = vec![];
            let out = crate::remote::execute(
                &state.prepared.remotes,
                r,
                &ctx.remote,
                ctx.settings,
                false,
                &mut cookies,
            )
            .await;
            (i, out, cookies)
        });
    let action_futures = root_fields
        .iter()
        .enumerate()
        .filter_map(|(i, (_, rf))| match rf {
            RootField::Action(a) => Some((i, a)),
            _ => None,
        })
        .map(|(i, a)| async move {
            let out =
                hge_actions::execute(&state.action_registry, a, &ctx.actions, &ctx.sources).await;
            (i, out)
        });
    let (db_results, remote_results, action_results) = futures::future::join3(
        futures::future::join_all(futures),
        futures::future::join_all(remote_futures),
        futures::future::join_all(action_futures),
    )
    .await;
    // The first failing root field, in query order, decides the error.
    let mut first_error: Option<(usize, Failure)> = None;
    let mut fail = |i: usize, f: Failure| {
        if first_error.as_ref().is_none_or(|(j, _)| i < *j) {
            first_error = Some((i, f));
        }
    };
    for res in db_results {
        match res {
            Ok(rows) => {
                for (i, json) in rows {
                    results[i] = Some(json);
                }
            }
            Err((i, e)) => fail(i, e.into()),
        }
    }
    for (i, res, cookies) in remote_results {
        match res {
            Ok(json) => {
                results[i] = Some(json);
                set_cookies.extend(cookies);
            }
            Err(e) => fail(i, e.into()),
        }
    }
    for (i, res) in action_results {
        match res {
            Ok(r) => results[i] = Some(r.value.to_string()),
            Err(e) => fail(i, e.into()),
        }
    }
    if let Some((_, f)) = first_error {
        return Err(f);
    }
    Ok(results)
}

/// The source of a mutation whose root fields all run in one transaction: every field is a
/// database mutation on that one source. Otherwise each field runs on its own.
pub(crate) fn one_transaction(op: &Operation) -> Option<&str> {
    let mut sources = op.root_fields.iter().map(|(_, rf)| match rf {
        RootField::Mutation(m) => Some(&*m.source),
        _ => None,
    });
    let first = sources.next()??;
    sources.all(|s| s == Some(first)).then_some(first)
}

/// Call the root field's `validate_input` webhooks (spec 04 §10).
async fn validate_input(
    state: &AppState,
    m: &hge_ir::MutationRootField,
    ctx: &Context<'_>,
) -> Result<(), Error> {
    if m.validations.is_empty() {
        return Ok(());
    }
    crate::input_validation::validate(
        &state.action_registry,
        &m.validations,
        ctx.session,
        ctx.actions.client_headers,
    )
    .await
}

/// Run the root fields of a mutation operation in order; the first error stops the
/// operation (spec 03 §1). Like V2, when every root field is a database mutation on the same
/// source they share one transaction (a failure rolls back all of them); otherwise each runs
/// on its own. Action handlers' and remote schemas' `Set-Cookie` headers are collected into
/// `set_cookies`.
async fn run_mutations(
    state: &AppState,
    op: &Operation,
    ctx: &Context<'_>,
    set_cookies: &mut Vec<String>,
) -> Result<Vec<Option<String>>, Failure> {
    let session = ctx.session;
    // Like V2, every root field's `validate_input` webhooks run while the operation is
    // planned, before any root field executes.
    for (_, rf) in &op.root_fields {
        if let RootField::Mutation(m) = rf {
            validate_input(state, m, ctx).await?;
        }
    }
    let mut results: Vec<Option<String>> = vec![None; op.root_fields.len()];
    if let Some(first_source) = one_transaction(op) {
        let batch: Vec<&Mutation> = op
            .root_fields
            .iter()
            .filter_map(|(_, rf)| match rf {
                RootField::Mutation(m) => Some(&m.mutation),
                _ => None,
            })
            .collect();
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
                let r = hge_actions::execute(&state.action_registry, a, &ctx.actions, &ctx.sources)
                    .await?;
                set_cookies.extend(r.set_cookies);
                Some(r.value.to_string())
            }
            RootField::Remote(r) => Some(
                crate::remote::execute(
                    &state.prepared.remotes,
                    r,
                    &ctx.remote,
                    ctx.settings,
                    true,
                    set_cookies,
                )
                .await?,
            ),
        };
    }
    Ok(results)
}
