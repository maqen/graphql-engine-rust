//! `POST /v1/query`: V2's legacy combined schema/metadata API (deprecated, still served).
//!
//! The JSON DML (`select`, `count`, …) is lowered to the same IR as GraphQL queries and runs
//! through the source's executor, so there is one SQL generator. Errors use V2's `QErr`
//! body with per-error HTTP statuses; paths point into the request (`$.args.where`), and
//! inside `bulk` at the sub-query (`$.args[1].args.table`).

mod boolexp;
mod count;
mod delete;
mod insert;
mod json;
mod lower;
mod query;
mod select;
mod update;

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use hge_core::{Code, Error, Session};
use hge_ir::{Mutation, QueryRoot, SourceExecutor};
use hge_metadata_api::Runtime;
use hge_metadata_api::ctx::Ctx;
use hge_metadata_api::run_sql;

use crate::graphql::include_internal;
use crate::routes::{json_response, request_id};
use crate::state::{AppState, Engine};
use hge_metadata_api::ApiError;
use json::{Path, err400};
use lower::Lowerer;
use query::Query;

/// The route handler.
pub async fn handler(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id(&headers);
    // Authenticate before looking at the body, as V2 does.
    let auth = match engine.auth.authenticate(&headers, None).await {
        Ok(auth) => auth,
        Err(e) => {
            let body = e.error.to_api_json(false).to_string();
            return json_response(e.status, body, Some(rid));
        }
    };
    let session = auth.session;
    let internal = include_internal(&engine, &session);
    match run(&engine, &session, &body).await {
        Ok(out) => {
            let mut resp = json_response(StatusCode::OK, out, Some(rid));
            for cookie in auth.set_cookies {
                resp.headers_mut().append(header::SET_COOKIE, cookie);
            }
            resp
        }
        Err(e) => {
            let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_REQUEST);
            json_response(status, e.error.to_api_json(internal).to_string(), Some(rid))
        }
    }
}

/// Parse and run one request body, returning the response JSON.
pub async fn run(engine: &Engine, session: &Session, body: &[u8]) -> Result<String, ApiError> {
    let request = parse_body(body)?;
    let query = Query::parse(&request, &Path::root())?;
    if query.requires_admin() && !session.is_admin() {
        return Err(err400(
            Code::AccessDenied,
            "restricted access : admin only",
            &Path::root().key("args"),
        ));
    }
    if engine.settings().read_only && query.writes_data() {
        return Err(ApiError::bad(
            Code::NotSupported,
            "Cannot run write queries when read-only mode is enabled",
        ));
    }
    // Metadata changes (and `run_sql` that may change the schema) share one unit of work:
    // one lock, and the metadata stored once at the end, only if every query succeeded.
    let ctx = match query.metadata_access() {
        Some(lock) => Some(Ctx::begin(engine, session, lock).await?),
        None => None,
    };
    let mut exec = Executor {
        engine,
        state: engine.state.load_full(),
        session,
        ctx,
    };
    let out = exec.run(&query).await?;
    if let Some(ctx) = exec.ctx {
        ctx.commit().await?;
    }
    Ok(out)
}

/// V2 maps every control character to a space before decoding (clients embed raw newlines
/// in JSON strings).
fn parse_body(body: &[u8]) -> Result<serde_json::Value, ApiError> {
    let cleaned: Vec<u8> = body
        .iter()
        .map(|&b| if b < 0x20 { b' ' } else { b })
        .collect();
    serde_json::from_slice(&cleaned)
        .map_err(|e| err400(Code::InvalidJson, format!("Error in $: {e}"), &Path::root()))
}

struct Executor<'a> {
    engine: &'a Engine,
    state: Arc<AppState>,
    session: &'a Session,
    ctx: Option<Ctx<'a, Engine>>,
}

impl<'a> Executor<'a> {
    fn run<'q>(&'q mut self, query: &'q Query) -> hge_ir::BoxFuture<'q, Result<String, ApiError>> {
        Box::pin(async move {
            match query {
                Query::Select(q) => {
                    let lw = self.lowerer(&q.source);
                    let select = lw.select(q, self.session)?;
                    self.execute(&q.source, QueryRoot::Select(select), &q.path)
                        .await
                }
                Query::Count(q) => {
                    let lw = self.lowerer(&q.source);
                    let count = lw.count(q, self.session)?;
                    let n = self
                        .execute(&q.source, QueryRoot::Count(count), &q.path)
                        .await?;
                    Ok(format!("{{\"count\":{n}}}"))
                }
                Query::Insert(q) => {
                    let mutation = self.lowerer(&q.source).insert(q, self.session)?;
                    self.execute_mutation(&q.source, mutation, &q.path).await
                }
                Query::Update(q) => {
                    let mutation = self.lowerer(&q.source).update(q, self.session)?;
                    self.execute_mutation(&q.source, mutation, &q.path).await
                }
                Query::Delete(q) => {
                    let mutation = self.lowerer(&q.source).delete(q, self.session)?;
                    self.execute_mutation(&q.source, mutation, &q.path).await
                }
                Query::Bulk(queries) => {
                    // Not transactional, like V2: each sub-query (DML included: V2's
                    // `runInsert` & co. each open their own transaction) commits on its own.
                    let mut out = String::from("[");
                    for (i, q) in queries.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        out.push_str(&self.run(q).await?);
                    }
                    out.push(']');
                    Ok(out)
                }
                Query::RunSql { args, path } => {
                    let result = match &mut self.ctx {
                        Some(ctx) if args.checks_metadata() => {
                            run_sql::run_checked(ctx, args).await
                        }
                        _ => run_sql::run_plain(self.engine, self.session, args).await,
                    };
                    result
                        .map(|v| v.to_string())
                        .map_err(|e| nest_error(e, path))
                }
                Query::Metadata { command, path } => {
                    // `run` begins a ctx whenever the request has a metadata query.
                    let ctx = self.ctx.as_mut().ok_or_else(|| {
                        ApiError::from(Error::unexpected("no metadata context for the request"))
                    })?;
                    let result = hge_metadata_api::api::run(ctx, command).await;
                    result.map(|v| v.to_string()).map_err(|e| {
                        hge_metadata_api::ops::actions::relative_to_args(&command.name, e)
                            .under(path.as_str())
                    })
                }
                Query::Other { ty, path, .. } => Err(err400(
                    Code::NotSupported,
                    format!("\"{ty}\" is not supported on /v1/query yet"),
                    path,
                )),
            }
        })
    }

    /// The request's working metadata, once a metadata query in it has changed something:
    /// later DML in the same bulk sees it, as in V2.
    fn working(&self) -> Option<&crate::load::Prepared> {
        self.ctx.as_ref().and_then(|c| c.prepared.as_ref())
    }

    fn lowerer<'s>(&'s self, source: &str) -> Lowerer<'s> {
        let tables = match self.working() {
            Some(p) => &p.tables,
            None => &self.state.tables,
        };
        Lowerer::new(tables, source, &self.session.role, &self.state.options)
    }

    /// Run one mutation on a source, in its own transaction; DB errors (and failed
    /// permission checks) are reported at the query's `args`.
    async fn execute_mutation(
        &self,
        source: &str,
        mutation: Mutation,
        path: &Path,
    ) -> Result<String, ApiError> {
        let executor = self.executor(source, path)?;
        let mut out = executor
            .execute_mutations(&[&mutation], self.session)
            .await
            .map_err(|e| dml_error(e, path))?;
        Ok(out.pop().unwrap_or_else(|| "null".to_owned()))
    }

    fn executor(&self, source: &str, path: &Path) -> Result<Arc<hge_postgres::PgSource>, ApiError> {
        let executor = match self.working() {
            Some(p) => p.sources.get(source).map(|s| s.pg.clone()),
            None => self.state.sources.get(source).cloned(),
        };
        executor.ok_or_else(|| {
            err400(
                Code::NotExists,
                format!("source with name \"{source}\" does not exist"),
                path,
            )
        })
    }

    /// Run one root field on a source; DB errors are reported at the query's `args`.
    async fn execute(
        &self,
        source: &str,
        root: QueryRoot,
        path: &Path,
    ) -> Result<String, ApiError> {
        let executor = self.executor(source, path)?;
        let mut out = executor
            .execute_query(&[&root], self.session)
            .await
            .map_err(|e| dml_error(e, path))?;
        Ok(out.pop().unwrap_or_else(|| "null".to_owned()))
    }
}

/// `run_sql` reports errors at `$`, which on `/v1/query` is the query's `args`.
fn nest_error(e: ApiError, args_path: &Path) -> ApiError {
    e.under(args_path.as_str())
}

/// V2's `dmlTxErrorHandler` statuses: expected DB errors are 400, anything else is a 500
/// `unexpected` "database query error".
fn dml_error(e: Error, path: &Path) -> ApiError {
    let e = e.with_path(path.as_str());
    match e.code {
        Code::PostgresError => ApiError {
            status: 500,
            error: Error {
                code: Code::Unexpected,
                ..e
            },
        },
        Code::Unexpected => ApiError {
            status: 500,
            error: e,
        },
        _ => ApiError {
            status: 400,
            error: e,
        },
    }
}
