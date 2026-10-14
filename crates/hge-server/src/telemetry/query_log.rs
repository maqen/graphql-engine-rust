//! `query-log`: V2's `QueryLog` (`GraphQL/Logging/QueryLog.hs`), one line per execution step
//! of a GraphQL operation, written before it runs.
//!
//! `{"query": <the request>, "generated_sql": {"<alias>": {"query", "prepared_arguments"}} | null,
//! "request_id", "kind": "database" | "action" | "remote-schema" | "introspection"}`. Only
//! database queries carry their SQL; mutations (one line, or one per field when they do not
//! share a transaction), subscriptions (one line when they start), actions, remote schema
//! fields and introspection carry `null`.

use std::collections::HashSet;
use std::sync::OnceLock;

use hge_core::Session;
use hge_ir::{Operation, OperationKind, RootField, SourceExecutor};
use serde::Serialize;
use serde_json::{Map, Value as Json, json};

use super::{Level, LogType};
use crate::graphql;
use crate::state::AppState;

/// `HASURA_GRAPHQL_LOG_MASKED_VARIABLES`, set once at startup.
static MASKED: OnceLock<HashSet<String>> = OnceLock::new();

/// Set the variable names whose values are masked (`HASURA_GRAPHQL_LOG_MASKED_VARIABLES`).
/// Only the first call has an effect.
pub fn set_masked_variables(names: HashSet<String>) {
    let _ = MASKED.set(names);
}

/// Whether `query-log` lines are written; check before building a request with [`request`].
pub fn enabled() -> bool {
    super::enabled(LogType::QueryLog, Level::Info)
}

/// The request as V2 logs it (`GQLReq`): `operationName` and `variables` only when given.
pub fn request(query: &str, operation_name: Option<&str>, variables: Option<&Json>) -> Json {
    let mut o = Map::new();
    if let Some(name) = operation_name {
        o.insert("operationName".into(), name.into());
    }
    o.insert("query".into(), query.into());
    if let Some(v) = variables.filter(|v| !v.is_null()) {
        o.insert("variables".into(), v.clone());
    }
    Json::Object(o)
}

/// A `query-log` line, borrowing the request so that each line serialises it without a copy.
#[derive(Serialize)]
struct Line<'a> {
    query: &'a Json,
    generated_sql: Option<Json>,
    request_id: &'a str,
    kind: &'a str,
}

/// Write the `query-log` lines of `op`, before running it. `request` comes from [`request`].
///
/// V2 writes each line just before its step runs; here every line is written up front, so a
/// mutation whose first field fails still logs its later fields.
pub fn log(state: &AppState, session: &Session, request_id: &str, request: &Json, op: &Operation) {
    if !enabled() {
        return;
    }
    let masked = MASKED.get().filter(|m| !m.is_empty());
    let masked_request;
    let query = match masked {
        Some(masked) => {
            masked_request = mask_request(request, masked);
            &masked_request
        }
        None => request,
    };
    let write = |generated_sql: Option<Json>, kind: &str| {
        let line = Line {
            query,
            generated_sql,
            request_id,
            kind,
        };
        super::emit(LogType::QueryLog, Level::Info, &line);
    };
    match op.kind {
        // V2 logs a subscription once, when it starts, without its SQL.
        OperationKind::Subscription => {
            let actions = op
                .root_fields
                .iter()
                .any(|(_, rf)| matches!(rf, RootField::Action(_)));
            write(None, if actions { "action" } else { "database" });
            return;
        }
        // V2's single-transaction path logs once for all the fields.
        OperationKind::Mutation if graphql::one_transaction(op).is_some() => {
            write(None, "database");
            return;
        }
        _ => {}
    }
    for (alias, rf) in &op.root_fields {
        match rf {
            RootField::Database(db) => {
                let sql = state
                    .sources
                    .get(&*db.source)
                    // A field that fails to translate fails when it runs, with its error in
                    // the response and `http-log`; its line here just has no SQL.
                    .and_then(|s| s.generated_sql(&db.query, session).ok().flatten())
                    .map(|g| {
                        let mut args = Json::Array(g.prepared_arguments);
                        if let Some(masked) = masked {
                            mask(&mut args, masked);
                        }
                        json!({ alias: {"query": g.query, "prepared_arguments": args} })
                    });
                write(sql, "database");
            }
            RootField::Mutation(_) => write(None, "database"),
            RootField::Action(_) => write(None, "action"),
            RootField::Remote(_) => write(None, "remote-schema"),
            RootField::Static(_) => write(None, "introspection"),
        }
    }
}

/// V2's `maskQueryLog`: top-level variables named in `masked`, and any object key in `masked`
/// at any depth inside the variables, become `"[MASKED]"`.
fn mask_request(request: &Json, masked: &HashSet<String>) -> Json {
    let mut request = request.clone();
    if let Some(vars) = request.get_mut("variables") {
        mask(vars, masked);
    }
    request
}

/// Replace the value of every object key in `masked`, at any depth, with `"[MASKED]"`.
fn mask(value: &mut Json, masked: &HashSet<String>) {
    match value {
        Json::Object(o) => {
            for (k, v) in o.iter_mut() {
                if masked.contains(k) {
                    *v = Json::String("[MASKED]".into());
                } else {
                    mask(v, masked);
                }
            }
        }
        Json::Array(a) => a.iter_mut().for_each(|v| mask(v, masked)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_omits_absent_fields() {
        assert_eq!(request("{ a }", None, None), json!({"query": "{ a }"}));
        assert_eq!(
            request("query Q { a }", Some("Q"), Some(&json!({"x": 1}))),
            json!({"operationName": "Q", "query": "query Q { a }", "variables": {"x": 1}})
        );
    }

    #[test]
    fn masks_at_any_depth() {
        let masked: HashSet<String> = ["password".to_owned()].into();
        let req = request(
            "q",
            None,
            Some(
                &json!({"password": "p", "input": {"password": "p", "list": [{"password": 1}], "name": "n"}}),
            ),
        );
        assert_eq!(
            mask_request(&req, &masked)["variables"],
            json!({"password": "[MASKED]", "input": {"password": "[MASKED]", "list": [{"password": "[MASKED]"}], "name": "n"}})
        );
    }
}
