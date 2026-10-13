//! Metadata commands. Each module implements a family; this file only dispatches.
//!
//! A command is a function `(ctx, args) -> ApiResult<Value>` that edits `ctx.metadata` and
//! then calls `ctx.check(..)` (modifying commands) or just reads state.

pub mod actions;
mod args;
pub mod event_triggers;
pub mod functions;
pub mod metadata;
pub mod objects;
pub(crate) mod parse;
pub mod relationships_permissions;
pub mod remote_schemas;
pub mod scheduled;
pub mod sources_tables;

use hge_core::Code;
use serde_json::Value;

use crate::api::Command;
use crate::ctx::Ctx;
use crate::response::{ApiError, ApiResult, parse_failed};
use crate::runtime::Runtime;

/// Backend-agnostic commands V2 knows. Those not implemented yet answer `not-supported`.
const AGNOSTIC: &[&str] = &[
    "replace_metadata",
    "export_metadata",
    "clear_metadata",
    "reload_metadata",
    "get_inconsistent_metadata",
    "drop_inconsistent_metadata",
    "get_catalog_state",
    "set_catalog_state",
    "dump_internal_state",
    "rename_source",
    "add_remote_schema",
    "update_remote_schema",
    "remove_remote_schema",
    "reload_remote_schema",
    "introspect_remote_schema",
    "add_remote_schema_permissions",
    "drop_remote_schema_permissions",
    "create_remote_schema_remote_relationship",
    "update_remote_schema_remote_relationship",
    "delete_remote_schema_remote_relationship",
    "create_cron_trigger",
    "delete_cron_trigger",
    "create_scheduled_event",
    "delete_scheduled_event",
    "get_scheduled_events",
    "get_scheduled_event_invocations",
    "get_cron_triggers",
    "cleanup_event_trigger_logs",
    "pause_event_trigger_cleanups",
    "resume_event_trigger_cleanups",
    "create_action",
    "drop_action",
    "update_action",
    "create_action_permission",
    "drop_action_permission",
    "set_custom_types",
    "create_query_collection",
    "rename_query_collection",
    "drop_query_collection",
    "add_query_to_collection",
    "drop_query_from_collection",
    "add_collection_to_allowlist",
    "drop_collection_from_allowlist",
    "update_scope_of_collection_in_allowlist",
    "create_rest_endpoint",
    "drop_rest_endpoint",
    "set_api_limits",
    "remove_api_limits",
    "set_metrics_config",
    "remove_metrics_config",
    "add_inherited_role",
    "drop_inherited_role",
    "add_host_to_tls_allowlist",
    "drop_host_from_tls_allowlist",
    "set_graphql_schema_introspection_options",
    "set_query_tags",
    "set_opentelemetry_config",
    "set_opentelemetry_status",
    "test_webhook_transform",
    "dc_add_agent",
    "dc_delete_agent",
    "list_source_kinds",
    "get_source_kind_capabilities",
    "get_table_info",
];

/// Postgres commands V2 knows (after the `pg_` prefix).
const PG_COMMANDS: &[&str] = &[
    "add_source",
    "drop_source",
    "update_source",
    "set_table_customization",
    "set_apollo_federation_config",
    "test_connection_template",
    "get_source_tables",
    "get_table_info",
    "track_table",
    "track_tables",
    "untrack_table",
    "untrack_tables",
    "set_table_is_enum",
    "create_insert_permission",
    "create_select_permission",
    "create_update_permission",
    "create_delete_permission",
    "drop_insert_permission",
    "drop_select_permission",
    "drop_update_permission",
    "drop_delete_permission",
    "set_permission_comment",
    "create_object_relationship",
    "create_array_relationship",
    "drop_relationship",
    "set_relationship_comment",
    "rename_relationship",
    "suggest_relationships",
    "create_remote_relationship",
    "update_remote_relationship",
    "delete_remote_relationship",
    "track_function",
    "untrack_function",
    "set_function_customization",
    "create_function_permission",
    "drop_function_permission",
    "add_computed_field",
    "drop_computed_field",
    "create_event_trigger",
    "delete_event_trigger",
    "redeliver_event",
    "invoke_event_trigger",
    "get_event_logs",
    "get_event_invocation_logs",
    "get_event_by_id",
    "get_native_query",
    "track_native_query",
    "untrack_native_query",
    "get_logical_model",
    "track_logical_model",
    "untrack_logical_model",
    "create_logical_model_select_permission",
    "drop_logical_model_select_permission",
];

pub(crate) fn not_implemented(name: &str) -> ApiError {
    ApiError::bad(
        Code::NotSupported,
        format!("metadata command \"{name}\" is not supported yet"),
    )
}

/// Reject command names V2 does not know, with V2's message: anything that is not a
/// backend-agnostic command is `<backend>_<command>`, and unknown backends are taken to be
/// data connector names.
pub fn validate_name(name: &str) -> ApiResult<()> {
    if AGNOSTIC.contains(&name) || matches!(name, "bulk" | "bulk_keep_going" | "bulk_atomic") {
        return Ok(());
    }
    let (prefix, command) = name.split_once('_').unwrap_or((name, ""));
    let backend = match prefix {
        "pg" | "postgres" => {
            if PG_COMMANDS.contains(&command) {
                return Ok(());
            }
            "postgres"
        }
        "citus" | "cockroach" | "mssql" | "bigquery" => {
            if PG_COMMANDS.contains(&command) {
                return Ok(());
            }
            prefix
        }
        other => other,
    };
    Err(parse_failed(
        "$",
        format!("unknown metadata command \"{command}\" for backend {backend}"),
    ))
}

pub async fn run_agnostic<R: Runtime>(ctx: &mut Ctx<'_, R>, cmd: &Command) -> ApiResult<Value> {
    let args = &cmd.args;
    match cmd.name.as_str() {
        "export_metadata" => metadata::export_metadata(ctx, cmd.version).await,
        "replace_metadata" => metadata::replace_metadata(ctx, args, cmd.version).await,
        "clear_metadata" => metadata::clear_metadata(ctx).await,
        "reload_metadata" => metadata::reload_metadata(ctx, args).await,
        "get_inconsistent_metadata" => metadata::get_inconsistent_metadata(ctx).await,
        "drop_inconsistent_metadata" => metadata::drop_inconsistent_metadata(ctx).await,
        "get_catalog_state" => metadata::get_catalog_state(ctx).await,
        "set_catalog_state" => metadata::set_catalog_state(ctx, args).await,
        "list_source_kinds" => Ok(metadata::list_source_kinds()),
        "cleanup_event_trigger_logs" => event_triggers::cleanup_event_trigger_logs(),
        "pause_event_trigger_cleanups" => {
            event_triggers::switch_event_trigger_cleanups(ctx, args, true).await
        }
        "resume_event_trigger_cleanups" => {
            event_triggers::switch_event_trigger_cleanups(ctx, args, false).await
        }
        "test_webhook_transform" => actions::test_webhook_transform(args),
        "create_action" => actions::create_action(ctx, args).await,
        "update_action" => actions::update_action(ctx, args).await,
        "drop_action" => actions::drop_action(ctx, args).await,
        "create_action_permission" => actions::create_action_permission(ctx, args).await,
        "drop_action_permission" => actions::drop_action_permission(ctx, args).await,
        "set_custom_types" => actions::set_custom_types(ctx, args).await,
        "add_remote_schema" => remote_schemas::add_remote_schema(ctx, args).await,
        "update_remote_schema" => remote_schemas::update_remote_schema(ctx, args).await,
        "remove_remote_schema" => remote_schemas::remove_remote_schema(ctx, args).await,
        "reload_remote_schema" => remote_schemas::reload_remote_schema(ctx, args).await,
        "introspect_remote_schema" => remote_schemas::introspect_remote_schema(ctx, args).await,
        "add_remote_schema_permissions" => {
            remote_schemas::add_remote_schema_permissions(ctx, args).await
        }
        "drop_remote_schema_permissions" => {
            remote_schemas::drop_remote_schema_permissions(ctx, args).await
        }
        "create_cron_trigger" => scheduled::create_cron_trigger(ctx, args).await,
        "delete_cron_trigger" => scheduled::delete_cron_trigger(ctx, args).await,
        "get_cron_triggers" => scheduled::get_cron_triggers(ctx),
        "create_scheduled_event" => scheduled::create_scheduled_event(ctx, args).await,
        "delete_scheduled_event" => scheduled::delete_scheduled_event(ctx, args).await,
        "get_scheduled_events" => scheduled::get_scheduled_events(ctx, args).await,
        "get_scheduled_event_invocations" => {
            scheduled::get_scheduled_event_invocations(ctx, args).await
        }
        "get_table_info" => sources_tables::get_table_info_agnostic(ctx, args).await,
        name if objects::COMMANDS.contains(&name) => objects::run(ctx, name, args).await,
        name if sources_tables::COMMANDS.contains(&name) => {
            sources_tables::run(ctx, name, args).await
        }
        name => Err(not_implemented(name)),
    }
}

/// A backend command (`pg_<command>`). Modules see the command without its prefix; the
/// source defaults to `"default"` when `args.source` is absent.
pub async fn run_backend<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    backend: &str,
    command: &str,
    args: &Value,
) -> ApiResult<Value> {
    if !matches!(backend, "pg" | "postgres") {
        return Err(not_implemented(&format!("{backend}_{command}")));
    }
    match command {
        "create_event_trigger" => event_triggers::create_event_trigger(ctx, args).await,
        "delete_event_trigger" => event_triggers::delete_event_trigger(ctx, args).await,
        "invoke_event_trigger" => event_triggers::invoke_event_trigger(ctx, args).await,
        "redeliver_event" => event_triggers::redeliver_event(ctx, args).await,
        "get_event_logs" => event_triggers::get_event_logs(ctx, args).await,
        "get_event_invocation_logs" => event_triggers::get_event_invocation_logs(ctx, args).await,
        "get_event_by_id" => event_triggers::get_event_by_id(ctx, args).await,
        c if sources_tables::COMMANDS.contains(&c) => sources_tables::run(ctx, c, args).await,
        c if relationships_permissions::COMMANDS.contains(&c) => {
            relationships_permissions::run(ctx, c, args).await
        }
        c if functions::COMMANDS.contains(&c) => functions::run(ctx, c, args).await,
        _ => Err(not_implemented(&format!("{backend}_{command}"))),
    }
}
