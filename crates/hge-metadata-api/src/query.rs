//! `POST /v2/query` (the schema API): `run_sql`, `bulk`, `concurrent_bulk` and JSON DML.

use hge_core::{Code, Session};
use serde_json::Value;

use crate::api::json_kind;
use crate::ctx::Ctx;
use crate::response::{ApiError, ApiResponse, ApiResult, parse_failed};
use crate::run_sql::{self, RunSql};
use crate::runtime::Runtime;

/// A parsed `/v2/query` request.
#[derive(Debug, Clone)]
pub enum Query {
    RunSql(RunSql),
    Bulk(Vec<Query>),
    ConcurrentBulk(Vec<Query>),
    /// `select`, `insert`, `update`, `delete`, `count`.
    Dml {
        kind: String,
        args: Value,
    },
}

const DML: &[&str] = &["select", "insert", "update", "delete", "count"];

impl Query {
    /// Parse an envelope; `path` is where it sits in the request (for error paths).
    pub fn parse(v: &Value, path: &str) -> ApiResult<Query> {
        let obj = v.as_object().ok_or_else(|| {
            parse_failed(
                path,
                format!(
                    "parsing Object failed, expected Object, but encountered {}",
                    json_kind(v)
                ),
            )
        })?;
        let kind = match obj.get("type") {
            Some(Value::String(s)) => s.as_str(),
            Some(_) => return Err(parse_failed(&format!("{path}.type"), "expected a string")),
            None => return Err(parse_failed(path, "key \"type\" not found")),
        };
        let args_path = format!("{path}.args");
        let args = obj
            .get("args")
            .ok_or_else(|| parse_failed(path, "key \"args\" not found"))?;
        match kind {
            "run_sql" | "pg_run_sql" | "citus_run_sql" | "cockroach_run_sql" => {
                serde_json::from_value(args.clone())
                    .map(Query::RunSql)
                    .map_err(|e| parse_failed(&args_path, crate::response::aeson_message(&e)))
            }
            "bulk" | "concurrent_bulk" => {
                let items = args.as_array().ok_or_else(|| {
                    parse_failed(
                        &args_path,
                        format!(
                            "parsing [] failed, expected Array, but encountered {}",
                            json_kind(args)
                        ),
                    )
                })?;
                let parsed = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| Query::parse(item, &format!("{args_path}[{i}]")))
                    .collect::<ApiResult<Vec<_>>>()?;
                Ok(if kind == "bulk" {
                    Query::Bulk(parsed)
                } else {
                    Query::ConcurrentBulk(parsed)
                })
            }
            k if DML.contains(&k) => Ok(Query::Dml {
                kind: k.to_owned(),
                args: args.clone(),
            }),
            other => Err(parse_failed(
                path,
                format!("Unrecognised RQLQuery type: {other}"),
            )),
        }
    }

    /// V2's `queryModifiesSchema`: may this change the schema (and so the metadata)?
    pub fn modifies_schema(&self) -> bool {
        match self {
            Query::RunSql(r) => r.checks_metadata(),
            Query::Bulk(items) | Query::ConcurrentBulk(items) => {
                items.iter().any(Query::modifies_schema)
            }
            Query::Dml { .. } => false,
        }
    }

    /// V2's `queryModifiesUserDB` (for read-only mode).
    pub fn writes(&self) -> bool {
        match self {
            Query::RunSql(r) => !r.read_only,
            Query::Bulk(items) => items.iter().any(Query::writes),
            Query::ConcurrentBulk(items) => items.iter().any(Query::writes),
            Query::Dml { kind, .. } => matches!(kind.as_str(), "insert" | "update" | "delete"),
        }
    }
}

/// Handle a `/v2/query` body (the caller has checked the admin role).
pub async fn handle_query<R: Runtime>(rt: &R, session: &Session, body: &[u8]) -> ApiResponse {
    match handle(rt, session, body).await {
        Ok(v) => ApiResponse::ok(v),
        Err(e) => e.to_response(),
    }
}

async fn handle<R: Runtime>(rt: &R, session: &Session, body: &[u8]) -> ApiResult<Value> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad(Code::InvalidJson, format!("Error in $: {e}")))?;
    let query = Query::parse(&value, "$")?;
    if rt.settings().read_only && query.writes() {
        return Err(ApiError::bad(
            Code::NotSupported,
            "Cannot run write queries when read-only mode is enabled",
        ));
    }
    if let Query::ConcurrentBulk(items) = &query
        && items.iter().any(Query::modifies_schema)
    {
        return Err(ApiError::new(
            500,
            Code::Unexpected,
            "Only read-only queries are allowed in a concurrent_bulk",
        ));
    }
    if !query.modifies_schema() {
        return run_plain(rt, session, &query, "$").await;
    }
    // Schema changes go through the metadata pipeline; the metadata is written once at the
    // end, and only if every query succeeded.
    let mut ctx = Ctx::begin(rt, session, true).await?;
    match run_checked(&mut ctx, &query, "$").await {
        Ok(result) => {
            ctx.commit().await?;
            Ok(result)
        }
        Err(e) => {
            ctx.discard().await;
            Err(e)
        }
    }
}

async fn run_plain<R: Runtime>(
    rt: &R,
    session: &Session,
    query: &Query,
    path: &str,
) -> ApiResult<Value> {
    match query {
        Query::RunSql(args) => run_sql::run_plain(rt, session, args)
            .await
            .map_err(|e| e.with_path(path)),
        Query::Bulk(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                out.push(Box::pin(run_plain(rt, session, item, &format!("$[{i}]"))).await?);
            }
            Ok(Value::Array(out))
        }
        Query::ConcurrentBulk(items) => {
            let futures = items.iter().enumerate().map(|(i, item)| async move {
                run_plain(rt, session, item, &format!("$[{i}]")).await
            });
            let results = futures::future::join_all(futures).await;
            results
                .into_iter()
                .collect::<ApiResult<Vec<_>>>()
                .map(Value::Array)
        }
        Query::Dml { kind, .. } => Err(ApiError::bad(
            Code::NotSupported,
            format!("the \"{kind}\" query is not supported yet"),
        )
        .with_path(path)),
    }
}

async fn run_checked<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    query: &Query,
    path: &str,
) -> ApiResult<Value> {
    match query {
        Query::RunSql(args) if args.checks_metadata() => run_sql::run_checked(ctx, args)
            .await
            .map_err(|e| e.with_path(path)),
        Query::Bulk(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                out.push(Box::pin(run_checked(ctx, item, &format!("$[{i}]"))).await?);
            }
            Ok(Value::Array(out))
        }
        other => run_plain(ctx.rt, ctx.session, other, path).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_queries() {
        let q = Query::parse(
            &json!({"type": "bulk", "args": [
                {"type": "run_sql", "args": {"sql": "select 1"}},
                {"type": "run_sql", "args": {"sql": "alter table a add column b int"}}
            ]}),
            "$",
        )
        .unwrap();
        assert!(q.modifies_schema());
        assert!(q.writes());
        let e = Query::parse(&json!({"type": "nope", "args": {}}), "$").unwrap_err();
        assert_eq!(e.error.message, "Unrecognised RQLQuery type: nope");
        let e = Query::parse(
            &json!({"type": "bulk", "args": [{"type": "run_sql", "args": {}}]}),
            "$",
        )
        .unwrap_err();
        assert_eq!(e.error.path, "$.args[0].args");
    }
}
