//! Executing actions: calling HTTP handlers, validating and projecting their responses, and
//! joining action outputs to tables.
//!
//! The schema side (custom types, root fields, lowering to IR) lives in
//! `hge_schema::actions`; this crate receives [`hge_ir::ActionRootField`]s and looks the
//! handler configuration up by action name in an [`ActionRegistry`].

pub mod env;
pub mod project;
pub mod registry;
pub mod validate;
pub mod webhook;

use std::collections::HashMap;

use hge_core::{Error, Session};
use hge_ir::action::{ActionCall, ActionRelationship};
use hge_ir::{ActionRootField, BoolExp, CompareOp, QueryRoot, SourceExecutor, Value as IrValue};
use serde_json::Value;

pub use env::{Environment, ProcessEnv};
pub use registry::{ActionHandler, ActionRegistry, BuiltRegistry, ConfiguredHeader, HandlerConfig};

/// Request-scoped inputs of an action call.
pub struct RequestContext<'a> {
    pub session: &'a Session,
    /// The client's request headers, for `forward_client_headers` (only read by actions that
    /// forward them).
    pub client_headers: &'a http::HeaderMap,
    /// The GraphQL query text (sent as `request_query`).
    pub query: Option<&'a str>,
}

/// Database access for action relationships.
pub trait Sources: Sync {
    fn executor(&self, source: &str) -> Option<&dyn SourceExecutor>;
}

/// The value of an action root field, plus response headers for the HTTP response.
#[derive(Debug)]
pub struct ActionResult {
    pub value: Value,
    /// `Set-Cookie` values from the handler (forwarded for mutations).
    pub set_cookies: Vec<String>,
}

/// Run one action root field.
pub async fn execute(
    registry: &ActionRegistry,
    field: &ActionRootField,
    ctx: &RequestContext<'_>,
    sources: &dyn Sources,
) -> Result<ActionResult, Error> {
    let handler = registry.get(&field.action).ok_or_else(|| {
        Error::unexpected(format!("action \"{}\" is not available", field.action))
    })?;
    match &field.call {
        ActionCall::Sync {
            input,
            output,
            selection,
        } => {
            let call = webhook::Call {
                input,
                session: &ctx.session.variables,
                request_query: ctx.query,
                client_headers: ctx.client_headers,
            };
            let resp = webhook::call(
                &registry.config.client,
                handler,
                &registry.config,
                registry.env.as_ref(),
                &call,
            )
            .await?;
            let mut joins = project::Joins::default();
            project::collect_joins(output, selection, &resp.body, &mut joins);
            let joined = if joins.is_empty() {
                HashMap::new()
            } else {
                fetch_joins(&joins, ctx.session, sources).await?
            };
            Ok(ActionResult {
                value: project::project(output, selection, resp.body, &joined),
                set_cookies: resp.set_cookies,
            })
        }
        ActionCall::AsyncMutation { .. } | ActionCall::AsyncQuery { .. } => Err(Error::new(
            hge_core::Code::NotSupported,
            "asynchronous actions are not supported yet",
        )),
    }
}

/// The text form of a join key value, as bound into SQL.
fn key_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The relationship's table selection restricted to one key.
fn keyed_select(rel: &ActionRelationship, key: &[Value]) -> QueryRoot {
    let mut select = rel.select.clone();
    let mut conds: Vec<BoolExp> = rel
        .mapping
        .iter()
        .zip(key)
        .map(|((_, column, ty), v)| BoolExp::Compare {
            column: column.clone(),
            ty: ty.clone(),
            op: CompareOp::Eq(IrValue::literal(key_text(v), ty.clone())),
        })
        .collect();
    if let Some(w) = select.args.where_.take() {
        conds.push(w);
    }
    select.args.where_ = Some(BoolExp::And(conds));
    if rel.array {
        QueryRoot::Select(select)
    } else {
        QueryRoot::SelectOne(select)
    }
}

/// One keyed relationship query: `(relationship, key, query)`.
type JoinQuery = (project::RelId, Vec<Value>, QueryRoot);

/// The most keyed queries per statement. Each one is a column of a single `SELECT`, and
/// Postgres allows at most 1664 target-list entries.
const MAX_ROOTS_PER_STATEMENT: usize = 500;

/// Fetch every joined row: one round trip per source and chunk of keys, concurrently.
async fn fetch_joins(
    joins: &project::Joins<'_>,
    session: &Session,
    sources: &dyn Sources,
) -> Result<project::Joined, Error> {
    let mut by_source: HashMap<&str, Vec<JoinQuery>> = HashMap::new();
    for (rel, keys) in &joins.keys {
        for key in keys {
            by_source.entry(&rel.source).or_default().push((
                project::rel_id(rel),
                key.clone(),
                keyed_select(rel, key),
            ));
        }
    }
    let mut joined = project::Joined::new();
    let chunks = by_source.iter().flat_map(|(source, items)| {
        items
            .chunks(MAX_ROOTS_PER_STATEMENT)
            .map(move |chunk| (*source, chunk))
    });
    let futures = chunks.map(|(source, items)| async move {
        let executor = sources
            .executor(source)
            .ok_or_else(|| Error::unexpected(format!("source \"{source}\" is not available")))?;
        let roots: Vec<&QueryRoot> = items.iter().map(|(_, _, q)| q).collect();
        let out = executor.execute_query(&roots, session).await?;
        Ok::<_, Error>((items, out))
    });
    for res in futures::future::join_all(futures).await {
        let (items, out) = res?;
        for ((id, key, _), text) in items.iter().zip(out) {
            let value: Value = serde_json::from_str(&text)
                .map_err(|e| Error::unexpected(format!("invalid JSON from database: {e}")))?;
            joined.insert((*id, key.clone()), value);
        }
    }
    Ok(joined)
}

pub(crate) fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}
