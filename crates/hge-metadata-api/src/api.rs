//! `POST /v1/metadata`: envelope parsing, command classification and execution.

use hge_core::Code;
use serde_json::Value;

use crate::ctx::Ctx;
use crate::ops;
use crate::response::{ApiError, ApiResponse, ApiResult, parse_failed};
use crate::runtime::Runtime;

/// One parsed metadata command.
#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub args: Value,
    /// Envelope `version` (1 or 2).
    pub version: u8,
}

/// Commands that never take arguments; `args` may be omitted.
const NULLARY: &[&str] = &[
    "get_cron_triggers",
    "remove_api_limits",
    "remove_metrics_config",
];

/// Backend name prefixes V2 recognises.
pub const BACKENDS: &[&str] = &["pg", "postgres", "citus", "cockroach", "mssql", "bigquery"];

/// Parse an envelope `{type, args, version?, resource_version?}`.
pub fn parse_envelope(v: &Value) -> ApiResult<(Command, Option<i64>)> {
    let obj = v.as_object().ok_or_else(|| {
        parse_failed(
            "$",
            format!(
                "parsing Object failed, expected Object, but encountered {}",
                json_kind(v)
            ),
        )
    })?;
    let name = match obj.get("type") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(parse_failed(
                "$.type",
                format!(
                    "parsing Text failed, expected String, but encountered {}",
                    json_kind(other)
                ),
            ));
        }
        None => return Err(parse_failed("$", "key \"type\" not found")),
    };
    let version = match obj.get("version") {
        None | Some(Value::Null) => 1,
        Some(Value::Number(n)) if n.as_u64() == Some(1) => 1,
        Some(Value::Number(n)) if n.as_u64() == Some(2) => 2,
        Some(other) => {
            return Err(parse_failed(
                "$.version",
                format!("expected 1 or 2, encountered {other}"),
            ));
        }
    };
    let resource_version = match obj.get("resource_version") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) if n.as_i64().is_some() => n.as_i64(),
        Some(other) => {
            return Err(parse_failed(
                "$.resource_version",
                format!(
                    "parsing Int failed, expected Number, but encountered {}",
                    json_kind(other)
                ),
            ));
        }
    };
    let args = match obj.get("args") {
        Some(a) => a.clone(),
        None if NULLARY.contains(&name.as_str()) => Value::Null,
        None => return Err(parse_failed("$", "key \"args\" not found")),
    };
    if version == 2 && !matches!(name.as_str(), "replace_metadata" | "export_metadata") {
        return Err(parse_failed(
            "$",
            format!("unknown metadata command \"{name}\" for version 2"),
        ));
    }
    ops::validate_name(&name)?;
    Ok((
        Command {
            name,
            args,
            version,
        },
        resource_version,
    ))
}

pub fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

/// Split a command name into (backend, command) per V2's dispatch rules.
pub fn split_backend(name: &str) -> Option<(&str, &str)> {
    let (prefix, rest) = name.split_once('_')?;
    BACKENDS.contains(&prefix).then_some((prefix, rest))
}

/// V2's `queryModifiesMetadata`: does this command write metadata?
pub fn modifies_metadata(cmd: &Command) -> bool {
    const READ_ONLY: &[&str] = &[
        "export_metadata",
        "get_inconsistent_metadata",
        "get_catalog_state",
        "set_catalog_state",
        "dump_internal_state",
        "create_scheduled_event",
        "delete_scheduled_event",
        "get_scheduled_events",
        "get_scheduled_event_invocations",
        "get_cron_triggers",
        "list_source_kinds",
        "get_source_kind_capabilities",
        "get_table_info",
        "introspect_remote_schema",
        "test_webhook_transform",
    ];
    const BACKEND_READ_ONLY: &[&str] = &[
        "redeliver_event",
        "invoke_event_trigger",
        "get_event_logs",
        "get_event_invocation_logs",
        "get_event_by_id",
        "get_source_tables",
        "get_source_trackables",
        "get_table_info",
        "test_connection_template",
        "suggest_relationships",
        "get_native_query",
        "get_stored_procedure",
        "get_logical_model",
    ];
    match cmd.name.as_str() {
        "bulk" | "bulk_keep_going" | "bulk_atomic" => match &cmd.args {
            Value::Array(items) => items.iter().any(|item| {
                parse_envelope(item)
                    .map(|(c, _)| modifies_metadata(&c))
                    .unwrap_or(true)
            }),
            _ => true,
        },
        name if READ_ONLY.contains(&name) => false,
        name => match split_backend(name) {
            Some((_, cmd)) => !BACKEND_READ_ONLY.contains(&cmd),
            None => true,
        },
    }
}

/// Handle a `/v1/metadata` request body (the caller has checked the admin role).
pub async fn handle_metadata<R: Runtime>(rt: &R, body: &[u8]) -> ApiResponse {
    match handle(rt, body).await {
        Ok(v) => ApiResponse::ok(v),
        Err(e) => e.to_response(),
    }
}

async fn handle<R: Runtime>(rt: &R, body: &[u8]) -> ApiResult<Value> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad(Code::InvalidJson, format!("Error in $: {e}")))?;
    let (cmd, resource_version) = parse_envelope(&value)?;
    let modifies = modifies_metadata(&cmd);
    if modifies && rt.settings().read_only {
        return Err(ApiError::bad(
            Code::NotSupported,
            "metadata cannot be modified in read-only mode",
        ));
    }
    let mut ctx = Ctx::begin(rt, modifies).await?;
    if modifies {
        ctx.expect_version(resource_version)?;
    }
    match run(&mut ctx, &cmd).await {
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

/// Run one command (recursively for bulk) against the working state. Execution errors are
/// reported under `$.args`, like V2.
pub async fn run<R: Runtime>(ctx: &mut Ctx<'_, R>, cmd: &Command) -> ApiResult<Value> {
    match cmd.name.as_str() {
        "bulk" | "bulk_atomic" | "bulk_keep_going" => {
            // Every item is parsed before any runs.
            let items = bulk_items(&cmd.args)?
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    parse_envelope(item)
                        .map(|(c, _)| c)
                        .map_err(|e| e.under(&format!("$.args[{i}]")))
                })
                .collect::<ApiResult<Vec<_>>>()?;
            let keep_going = cmd.name == "bulk_keep_going";
            let mut results = Vec::with_capacity(items.len());
            for (i, sub) in items.iter().enumerate() {
                let prefix = format!("$.args[{i}]");
                let snapshot = keep_going.then(|| ctx.snapshot());
                match Box::pin(run(ctx, sub)).await {
                    Ok(v) => results.push(v),
                    Err(e) => match snapshot {
                        Some(s) => {
                            ctx.restore(s);
                            results.push(e.under(&prefix).to_response().body);
                        }
                        None => return Err(e.under(&prefix)),
                    },
                }
            }
            Ok(Value::Array(results))
        }
        name => {
            let result = match split_backend(name) {
                Some((backend, command)) => ops::run_backend(ctx, backend, command, &cmd.args)
                    .await
                    .map_err(|e| wrap_parse_error(e, command)),
                None => ops::run_agnostic(ctx, cmd).await,
            };
            result.map_err(|e| {
                if e.error.path == "$" {
                    e.with_path("$.args")
                } else {
                    e
                }
            })
        }
    }
}

fn bulk_items(args: &Value) -> ApiResult<&Vec<Value>> {
    args.as_array().ok_or_else(|| {
        parse_failed(
            "$.args",
            format!(
                "parsing [] failed, expected Array, but encountered {}",
                json_kind(args)
            ),
        )
    })
}

/// Backend-prefixed commands report argument parse errors with V2's documentation blurb.
fn wrap_parse_error(mut e: ApiError, command: &str) -> ApiError {
    if e.error.code == Code::ParseFailed
        && e.error.path.starts_with("$.args")
        && !e.error.message.starts_with("Error when parsing command")
    {
        e.error.message = format!(
            "Error when parsing command {command}.\n\
             See our documentation at https://hasura.io/docs/latest/graphql/core/api-reference/metadata-api/index.html#metadata-apis.\n\
             Internal error message: {}",
            e.error.message
        );
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_rules() {
        let (c, rv) =
            parse_envelope(&json!({"type": "export_metadata", "args": {}, "resource_version": 3}))
                .unwrap();
        assert_eq!(
            (c.name.as_str(), c.version, rv),
            ("export_metadata", 1, Some(3))
        );
        let e = parse_envelope(&json!({"type": "export_metadata"})).unwrap_err();
        assert_eq!(e.error.message, "key \"args\" not found");
        let e = parse_envelope(&json!({"type": "x", "args": {}, "version": 3})).unwrap_err();
        assert_eq!(e.error.path, "$.version");
        assert!(parse_envelope(&json!({"type": "get_cron_triggers"})).is_ok());
        let e = parse_envelope(&json!({"type": "nope_cmd", "args": {}})).unwrap_err();
        assert_eq!(
            e.error.message,
            "unknown metadata command \"cmd\" for backend nope"
        );
        let e = parse_envelope(&json!({"type": "foo", "args": {}})).unwrap_err();
        assert_eq!(
            e.error.message,
            "unknown metadata command \"\" for backend foo"
        );
        let e = parse_envelope(&json!({"type": "pg_foo", "args": {}})).unwrap_err();
        assert_eq!(
            e.error.message,
            "unknown metadata command \"foo\" for backend postgres"
        );
    }

    #[test]
    fn classification() {
        let cmd = |name: &str, args: Value| Command {
            name: name.into(),
            args,
            version: 1,
        };
        assert!(!modifies_metadata(&cmd("export_metadata", json!({}))));
        assert!(modifies_metadata(&cmd("reload_metadata", json!({}))));
        assert!(modifies_metadata(&cmd("pg_track_table", json!({}))));
        assert!(!modifies_metadata(&cmd("pg_get_source_tables", json!({}))));
        assert!(!modifies_metadata(&cmd(
            "bulk",
            json!([{"type": "export_metadata", "args": {}}])
        )));
        assert!(modifies_metadata(&cmd(
            "bulk",
            json!([{"type": "export_metadata", "args": {}}, {"type": "pg_track_table", "args": {}}])
        )));
        assert_eq!(split_backend("pg_track_table"), Some(("pg", "track_table")));
        assert_eq!(split_backend("track_table"), None);
    }
}
