//! Executing actions: calling HTTP handlers, validating and projecting their responses,
//! joining action outputs to tables, and asynchronous actions ([`async_action`]).
//!
//! The schema side (custom types, root fields, lowering to IR) lives in
//! `hge_schema::actions`; this crate receives [`hge_ir::ActionRootField`]s and looks the
//! handler configuration up by action name in an [`ActionRegistry`].

pub mod async_action;
pub mod env;
pub mod project;
pub mod registry;
pub mod validate;
pub mod webhook;

use std::collections::HashMap;

use hge_core::{Error, Session};
use hge_ir::action::{ActionCall, ActionRelTarget, ActionRelationship};
use hge_ir::{ActionRootField, BoolExp, CompareOp, QueryRoot, SourceExecutor, Value as IrValue};
use serde_json::Value;

pub use async_action::{ActionLogStore, ProcessorConfig};
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
    /// Where asynchronous actions are recorded (`None` without a metadata database).
    pub action_log: Option<&'a dyn ActionLogStore>,
    /// Whether `internal` error details are shown to this request (async `errors` field).
    pub include_internal: bool,
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
                value: project::project(
                    output,
                    selection,
                    resp.body,
                    &joined,
                    project::Missing::Omit,
                ),
                set_cookies: resp.set_cookies,
            })
        }
        ActionCall::AsyncMutation { input } => Ok(ActionResult {
            value: async_action::submit(&handler.name, input, ctx).await?,
            set_cookies: vec![],
        }),
        ActionCall::AsyncQuery {
            id,
            output,
            fields,
            has_relationships,
        } => {
            let poll = async_action::poll_async_query(
                &field.action,
                id,
                output,
                fields,
                *has_relationships,
                ctx,
                sources,
            )
            .await?;
            Ok(ActionResult {
                value: poll.value,
                set_cookies: vec![],
            })
        }
    }
}

/// Subscriptions hook (spec §9.5): the current value of an async action query root field
/// (`<action>(id: uuid!)`, lowered to [`ActionCall::AsyncQuery`]) and whether the action has
/// finished. A subscription polls this with the subscriber's [`RequestContext`] and sends
/// `value` whenever it changes, completing once `finished` is true. Relationship fields in
/// `output` must be rejected before subscribing (`not-supported`), as in V2. Errors are the
/// same as for the query; any other root field kind is an `unexpected` error.
pub async fn poll(
    field: &ActionRootField,
    ctx: &RequestContext<'_>,
    sources: &dyn Sources,
) -> Result<async_action::AsyncQueryPoll, Error> {
    match &field.call {
        ActionCall::AsyncQuery {
            id,
            output,
            fields,
            has_relationships,
        } => {
            async_action::poll_async_query(
                &field.action,
                id,
                output,
                fields,
                *has_relationships,
                ctx,
                sources,
            )
            .await
        }
        _ => Err(Error::unexpected(format!(
            "\"{}\" is not an asynchronous action query",
            field.action
        ))),
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
    let mut restrict = |where_: &mut Option<BoolExp>| {
        if let Some(w) = where_.take() {
            conds.push(w);
        }
        *where_ = Some(BoolExp::And(std::mem::take(&mut conds)));
    };
    match &rel.target {
        ActionRelTarget::Object(select) => {
            let mut select = select.clone();
            restrict(&mut select.args.where_);
            QueryRoot::SelectOne(select)
        }
        ActionRelTarget::Array(select) => {
            let mut select = select.clone();
            restrict(&mut select.args.where_);
            QueryRoot::Select(select)
        }
        ActionRelTarget::Aggregate(select) => {
            let mut select = select.clone();
            restrict(&mut select.args.where_);
            QueryRoot::Aggregate(select)
        }
    }
}

/// One keyed relationship query: `(relationship, key, query)`.
type JoinQuery = (project::RelId, Vec<Value>, QueryRoot);

/// The most keyed queries per statement. Each one is a column of a single `SELECT`, and
/// Postgres allows at most 1664 target-list entries.
const MAX_ROOTS_PER_STATEMENT: usize = 500;

/// Fetch every joined row: one round trip per source and chunk of keys, concurrently.
pub(crate) async fn fetch_joins(
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
