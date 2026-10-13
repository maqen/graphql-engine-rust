//! `POST /v1/graphql` (single and batched requests) and `GET /v1/graphql`.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use hge_core::{Code, Error, Session};
use hge_ir::{Mutation, Operation, QueryRoot, RootField, SourceExecutor};
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
}

/// A request body: one request or a batch.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum GraphQLBody {
    Single(GraphQLRequest),
    Batch(Vec<GraphQLRequest>),
}

/// Execute a request body and return the response body.
pub async fn execute_body(engine: &Engine, session: &Session, body: GraphQLBody) -> String {
    let state = engine.state.load_full();
    match body {
        GraphQLBody::Single(req) => execute_one(engine, &state, session, req).await,
        GraphQLBody::Batch(reqs) => {
            let mut out = String::from("[");
            for (i, req) in reqs.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&execute_one(engine, &state, session, req).await);
            }
            out.push(']');
            out
        }
    }
}

async fn execute_one(
    engine: &Engine,
    state: &Arc<AppState>,
    session: &Session,
    req: GraphQLRequest,
) -> String {
    match run(state, session, req).await {
        Ok(body) => body,
        Err(e) => error_body(&e, include_internal(engine, session)),
    }
}

pub fn include_internal(engine: &Engine, session: &Session) -> bool {
    engine.config.dev_mode || (engine.config.admin_internal_errors && session.is_admin())
}

pub fn error_body(e: &Error, include_internal: bool) -> String {
    let body = serde_json::json!({ "errors": [e.to_graphql_json(include_internal)] });
    body.to_string()
}

async fn run(state: &AppState, session: &Session, req: GraphQLRequest) -> Result<String, Error> {
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
    let rs = state.role_schema(&session.role);
    let doc = hge_schema::parse_query(query)?;
    let op = hge_schema::plan(&rs, &doc, req.operation_name.as_deref(), &variables)?;

    // Group database root fields by source; one round trip per source.
    let mut by_source: HashMap<&str, Vec<(usize, &QueryRoot)>> = HashMap::new();
    for (i, (_, rf)) in op.root_fields.iter().enumerate() {
        if let RootField::Database(db) = rf {
            by_source
                .entry(&db.source)
                .or_default()
                .push((i, &db.query));
        }
    }
    let mut results: Vec<Option<String>> = vec![None; op.root_fields.len()];
    run_mutations(state, session, &op, &mut results).await?;
    let futures = by_source.into_iter().map(|(source, fields)| async move {
        let executor = state
            .sources
            .get(source)
            .ok_or_else(|| Error::unexpected(format!("source \"{source}\" is not available")))?;
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
    for res in futures::future::join_all(futures).await {
        for (i, json) in res? {
            results[i] = Some(json);
        }
    }

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
            RootField::Database(_) | RootField::Mutation(_) => {
                body.push_str(results[i].as_deref().unwrap_or("null"));
            }
        }
    }
    body.push_str("}}");
    Ok(body)
}

/// Run the mutation root fields of `op` in order. Like V2, when every root field is a
/// mutation on the same source they share one transaction (a failure rolls back all of them);
/// otherwise each runs in its own transaction.
async fn run_mutations(
    state: &AppState,
    session: &Session,
    op: &Operation,
    results: &mut [Option<String>],
) -> Result<(), Error> {
    let fields: Vec<(usize, &str, &Mutation)> = op
        .root_fields
        .iter()
        .enumerate()
        .filter_map(|(i, (_, rf))| match rf {
            RootField::Mutation(m) => Some((i, &*m.source, &m.mutation)),
            _ => None,
        })
        .collect();
    let Some(&(_, first_source, _)) = fields.first() else {
        return Ok(());
    };
    let executor = |source: &str| {
        state
            .sources
            .get(source)
            .ok_or_else(|| Error::unexpected(format!("source \"{source}\" is not available")))
    };
    let one_transaction = fields.len() == op.root_fields.len()
        && fields.iter().all(|(_, source, _)| *source == first_source);
    if one_transaction {
        let mutations: Vec<&Mutation> = fields.iter().map(|(_, _, m)| *m).collect();
        let out = executor(first_source)?
            .execute_mutations(&mutations, session)
            .await?;
        for ((i, _, _), json) in fields.iter().zip(out) {
            results[*i] = Some(json);
        }
    } else {
        for (i, source, m) in fields {
            let mut out = executor(source)?.execute_mutations(&[m], session).await?;
            results[i] = out.pop();
        }
    }
    Ok(())
}
