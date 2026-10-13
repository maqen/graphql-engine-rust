//! Whole-metadata commands: export, replace, clear, reload, inconsistencies, catalog state.

use std::collections::HashSet;

use hge_core::{Code, InconsistentObject};
use hge_metadata::store::CatalogStateKind;
use hge_metadata::{Metadata, Source, SourceConfiguration, TableEntry};
use serde_json::{Map, Value, json};

use crate::ctx::{Check, Ctx};
use crate::response::{
    ApiError, ApiResult, already_exists, consistency, not_exists, not_supported, parse_failed,
    success,
};
use crate::runtime::{Reload, Runtime};

/// Cron triggers marked `include_in_metadata: false` live in metadata but are not exported.
fn excluded_cron(trigger: &Value) -> bool {
    trigger.get("include_in_metadata") == Some(&Value::Bool(false))
}

pub async fn export_metadata<R: Runtime>(ctx: &mut Ctx<'_, R>, version: u8) -> ApiResult<Value> {
    let mut md = ctx.metadata.clone();
    md.cron_triggers.retain(|t| !excluded_cron(t));
    let doc = hge_metadata::export_json(&md);
    Ok(if version == 2 {
        json!({ "resource_version": ctx.resource_version, "metadata": doc })
    } else {
        doc
    })
}

/// Parse a metadata document given to `replace_metadata`, upgrading v1/v2 (no sources)
/// documents onto the current `default` source like V2.
fn parse_payload(payload: &Value, current: &Metadata, path: &str) -> ApiResult<Metadata> {
    let version = match payload.get("version") {
        None | Some(Value::Null) => 1,
        Some(v) => v.as_u64().filter(|v| (1..=3).contains(v)).ok_or_else(|| {
            parse_failed(
                "$.args.version",
                format!("expected 1, 2 or 3, encountered {v}"),
            )
        })?,
    };
    if version == 3 {
        return hge_metadata::from_json(payload.clone()).map_err(|e| {
            let status = e.code.http_status();
            ApiError { status, error: e }.under(path)
        });
    }
    let default = current.source("default").ok_or_else(|| {
        ApiError::bad(
            Code::Unexpected,
            "cannot import metadata without sources since no default source is defined",
        )
    })?;
    let obj = payload
        .as_object()
        .ok_or_else(|| parse_failed("$.args", "expected an object"))?;
    let tables: Vec<TableEntry> = match obj.get("tables") {
        Some(t) => serde_json::from_value(t.clone())
            .map_err(|e| parse_failed("$.args.tables", e.to_string()))?,
        None => vec![],
    };
    let functions = match obj.get("functions") {
        Some(Value::Array(f)) => f.clone(),
        _ => vec![],
    };
    let mut doc = Map::new();
    doc.insert("version".into(), json!(3));
    for key in [
        "remote_schemas",
        "query_collections",
        "allowlist",
        "actions",
        "custom_types",
        "cron_triggers",
    ] {
        if let Some(v) = obj.get(key) {
            doc.insert(key.into(), v.clone());
        }
    }
    let mut md = hge_metadata::from_json(Value::Object(doc))
        .map_err(|e| ApiError::from(e).with_path("$.args"))?;
    md.sources = vec![Source {
        tables,
        functions,
        ..default.clone()
    }];
    // V1/V2 documents cannot express REST endpoints, so V2 keeps the current ones.
    md.rest_endpoints = current.rest_endpoints.clone();
    Ok(md)
}

/// V2's event trigger name rules, checked on replace.
fn validate_event_triggers(md: &Metadata) -> ApiResult<()> {
    for source in &md.sources {
        let mut seen = HashSet::new();
        let mut dups = vec![];
        for table in &source.tables {
            for trigger in &table.event_triggers {
                let Some(name) = trigger.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if name.len() > 42 {
                    return Err(not_supported(
                        "event trigger name can be at most 42 characters",
                    ));
                }
                let mut chars = name.chars();
                let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                    && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                if !valid {
                    return Err(not_supported(
                        "Starting in v2.50 only alphanumeric and underscore and hyphens allowed for name",
                    ));
                }
                if !seen.insert(name.to_owned()) {
                    dups.push(name.to_owned());
                }
            }
        }
        if !dups.is_empty() {
            return Err(not_supported(format!(
                "Event trigger with duplicate names not allowed: {}",
                dups.join(", ")
            )));
        }
    }
    Ok(())
}

/// Keep the current `include_in_metadata: false` cron triggers.
fn merge_hidden_cron(md: &mut Metadata, current: &Metadata) -> ApiResult<()> {
    for hidden in current.cron_triggers.iter().filter(|t| excluded_cron(t)) {
        let name = hidden
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if md
            .cron_triggers
            .iter()
            .any(|t| t.get("name").and_then(Value::as_str) == Some(name))
        {
            return Err(already_exists(format!(
                "cron trigger with name {name} already exists as a cron trigger with \"included_in_metadata\" as false"
            )));
        }
        md.cron_triggers.push(hidden.clone());
    }
    Ok(())
}

pub async fn replace_metadata<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
    version: u8,
) -> ApiResult<Value> {
    // The argument shape (not the envelope version) decides the response format.
    let v2_args = args.as_object().is_some_and(|o| o.contains_key("metadata"));
    let (payload, allow_inconsistent, v2_response, path) = if v2_args {
        let flag = |k: &str, default: bool| match args.get(k) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => Err(parse_failed(
                &format!("$.args.{k}"),
                format!(
                    "parsing Bool failed, expected Boolean, but encountered {}",
                    crate::api::json_kind(other)
                ),
            )),
        };
        let allow = flag("allow_inconsistent_metadata", false)?;
        flag("allow_warnings", true)?;
        (&args["metadata"], allow, true, "$.args.metadata")
    } else {
        (args, false, version == 2, "$.args")
    };
    let mut md = parse_payload(payload, &ctx.metadata, path)?;
    merge_hidden_cron(&mut md, &ctx.metadata)?;
    validate_event_triggers(&md)?;
    ctx.metadata = md;
    ctx.reload.merge(Reload::Introspect);
    ctx.check(if allow_inconsistent {
        Check::Allow
    } else {
        Check::None
    })
    .await?;
    Ok(if v2_response {
        consistency(&ctx.inconsistencies)
    } else {
        success()
    })
}

pub async fn clear_metadata<R: Runtime>(ctx: &mut Ctx<'_, R>) -> ApiResult<Value> {
    let mut md = Metadata::empty();
    if let Some(default) = ctx.metadata.source("default") {
        md.sources.push(Source {
            name: default.name.clone(),
            kind: default.kind.clone(),
            tables: vec![],
            functions: vec![],
            configuration: SourceConfiguration {
                ..default.configuration.clone()
            },
            customization: None,
            extra: Map::new(),
        });
    }
    md.cron_triggers = ctx
        .metadata
        .cron_triggers
        .iter()
        .filter(|t| excluded_cron(t))
        .cloned()
        .collect();
    ctx.metadata = md;
    ctx.reload.merge(Reload::Introspect);
    ctx.check(Check::None).await?;
    Ok(success())
}

/// `true`/`false` or a list of names.
fn reload_selector(args: &Value, key: &str, default: bool) -> ApiResult<Option<Vec<String>>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default.then(Vec::new)),
        Some(Value::Bool(true)) => Ok(Some(vec![])),
        Some(Value::Bool(false)) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| parse_failed(&format!("$.args.{key}"), "expected a name"))
            })
            .collect::<ApiResult<Vec<_>>>()
            .map(Some),
        Some(_) => Err(parse_failed(
            &format!("$.args.{key}"),
            "expected a boolean or a list of names",
        )),
    }
}

pub async fn reload_metadata<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    if !args.is_object() {
        return Err(parse_failed(
            "$.args",
            format!(
                "parsing ReloadMetadata failed, expected Object, but encountered {}",
                crate::api::json_kind(args)
            ),
        ));
    }
    let reload = match reload_selector(args, "reload_sources", true)? {
        None => Reload::Introspect,
        Some(names) if names.is_empty() => Reload::All,
        Some(names) => {
            for n in &names {
                if ctx.metadata.source(n).is_none() {
                    return Err(not_exists(format!(
                        "Source with name \"{n}\" not found in metadata"
                    )));
                }
            }
            Reload::Sources(names.into_iter().collect())
        }
    };
    if let Some(names) = reload_selector(args, "reload_remote_schemas", true)? {
        for n in &names {
            let known = ctx
                .metadata
                .remote_schemas
                .iter()
                .any(|r| r.get("name").and_then(Value::as_str) == Some(n));
            if !known {
                return Err(not_exists(format!(
                    "Remote schema with name \"{n}\" not found in metadata"
                )));
            }
        }
    }
    ctx.refresh(reload).await;
    let mut out = Map::new();
    if !ctx.inconsistencies.is_empty() {
        out.insert(
            "inconsistent_objects".into(),
            hge_core::inconsistency::inconsistencies_json(&ctx.inconsistencies),
        );
    }
    out.insert(
        "is_consistent".into(),
        Value::Bool(ctx.inconsistencies.is_empty()),
    );
    out.insert("message".into(), Value::String("success".into()));
    Ok(Value::Object(out))
}

pub async fn get_inconsistent_metadata<R: Runtime>(ctx: &mut Ctx<'_, R>) -> ApiResult<Value> {
    Ok(consistency(&ctx.inconsistencies))
}

/// `"… in source <s>"` → `s`.
fn source_of(name: &str) -> Option<&str> {
    name.rsplit_once(" in source ").map(|(_, s)| s)
}

/// Remove one inconsistent object from the metadata. Returns whether it was handled.
fn drop_object(md: &mut Metadata, obj: &InconsistentObject) -> bool {
    let def = &obj.definition;
    let table_of = |def: &Value| -> Option<hge_core::QualifiedTable> {
        serde_json::from_value(def.get("table")?.clone()).ok()
    };
    match obj.kind.as_str() {
        "source" => {
            let name = def
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| obj.name.strip_prefix("source ").map(str::to_owned));
            match name {
                Some(n) => {
                    md.sources.retain(|s| s.name != n);
                    true
                }
                None => false,
            }
        }
        "table" => {
            let (Some(source), Ok(table)) = (
                source_of(&obj.name),
                serde_json::from_value::<hge_core::QualifiedTable>(def.clone()),
            ) else {
                return false;
            };
            if let Some(s) = md.source_mut(source) {
                s.tables.retain(|t| t.table != table);
            }
            true
        }
        // V2's definition is `{configuration, table}`; the source is in the object's name.
        "event_trigger" => {
            let (Some(source), Some(table), Some(name)) = (
                source_of(&obj.name),
                table_of(def),
                def.pointer("/configuration/name").and_then(Value::as_str),
            ) else {
                return false;
            };
            if let Some(entry) = md
                .source_mut(source)
                .and_then(|s| s.tables.iter_mut().find(|t| t.table == table))
            {
                entry
                    .event_triggers
                    .retain(|c| c.get("name").and_then(Value::as_str) != Some(name));
            }
            true
        }
        kind => {
            let (Some(source), Some(table)) =
                (def.get("source").and_then(Value::as_str), table_of(def))
            else {
                return false;
            };
            let Some(entry) = md
                .source_mut(source)
                .and_then(|s| s.tables.iter_mut().find(|t| t.table == table))
            else {
                return true;
            };
            let name = def.get("name").and_then(Value::as_str).unwrap_or_default();
            let role = def.get("role").and_then(Value::as_str).unwrap_or_default();
            match kind {
                "object_relation" => entry.object_relationships.retain(|r| r.name != name),
                "array_relation" => entry.array_relationships.retain(|r| r.name != name),
                "insert_permission" => entry.insert_permissions.retain(|p| p.role != role),
                "select_permission" => entry.select_permissions.retain(|p| p.role != role),
                "update_permission" => entry.update_permissions.retain(|p| p.role != role),
                "delete_permission" => entry.delete_permissions.retain(|p| p.role != role),
                "computed_field" => entry
                    .computed_fields
                    .retain(|c| c.get("name").and_then(Value::as_str) != Some(name)),
                "remote_relationship" => entry
                    .remote_relationships
                    .retain(|c| c.get("name").and_then(Value::as_str) != Some(name)),
                _ => return false,
            }
            true
        }
    }
}

pub async fn drop_inconsistent_metadata<R: Runtime>(ctx: &mut Ctx<'_, R>) -> ApiResult<Value> {
    if ctx.inconsistencies.is_empty() {
        return Ok(success());
    }
    let objects = ctx.inconsistencies.clone();
    // Dependents first (table sub-objects, then tables, then sources).
    let rank = |o: &InconsistentObject| match o.kind.as_str() {
        "source" => 2,
        "table" => 1,
        _ => 0,
    };
    let mut ordered: Vec<&InconsistentObject> = objects.iter().collect();
    ordered.sort_by_key(|o| rank(o));
    for obj in ordered {
        drop_object(&mut ctx.metadata, obj);
    }
    ctx.check(Check::Allow).await?;
    if !ctx.inconsistencies.is_empty() {
        return Err(ApiError::bad(
            Code::Unexpected,
            "cannot continue due to new inconsistent metadata",
        )
        .with_internal(hge_core::inconsistency::inconsistencies_json(
            &ctx.inconsistencies,
        )));
    }
    Ok(success())
}

pub async fn get_catalog_state<R: Runtime>(ctx: &mut Ctx<'_, R>) -> ApiResult<Value> {
    let mut state = ctx.rt.store().catalog_state().await?;
    // jsonb reorders keys; V2 re-encodes them sorted.
    state.cli_state = hge_metadata::export::sorted(state.cli_state);
    state.console_state = hge_metadata::export::sorted(state.console_state);
    Ok(state.to_json())
}

pub async fn set_catalog_state<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let kind = match args.get("type").and_then(Value::as_str) {
        Some("cli") => CatalogStateKind::Cli,
        Some("console") => CatalogStateKind::Console,
        Some(other) => {
            return Err(parse_failed(
                "$.args.type",
                format!(
                    "parsing Hasura.RQL.Types.Metadata.Common.CatalogStateType failed, expected one of the tags [\"cli\",\"console\"], but found tag \"{other}\""
                ),
            ));
        }
        None => return Err(parse_failed("$.args", "key \"type\" not found")),
    };
    let state = args
        .get("state")
        .ok_or_else(|| parse_failed("$.args", "key \"state\" not found"))?;
    ctx.rt.store().set_catalog_state(kind, state).await?;
    Ok(success())
}

pub fn list_source_kinds() -> Value {
    let kind = |k: &str| json!({"available": true, "builtin": true, "display_name": k, "kind": k, "release_name": "GA"});
    json!({ "sources": [kind("pg")] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_payload_moves_tables_to_default_source() {
        let current = hge_metadata::from_json(json!({
            "version": 3,
            "sources": [{"name": "default", "kind": "postgres", "tables": [],
                         "configuration": {"connection_info": {"database_url": "x"}}}],
            "rest_endpoints": [{"name": "e", "url": "e", "methods": ["GET"],
                                "definition": {"query": {"collection_name": "c", "query_name": "q"}}}]
        }))
        .unwrap();
        let md = parse_payload(
            &json!({"version": 2, "tables": [{"table": {"schema": "public", "name": "t"}}]}),
            &current,
            "$.args",
        )
        .unwrap();
        assert_eq!(md.sources[0].name, "default");
        assert_eq!(md.sources[0].tables[0].table.name, "t");
        assert_eq!(md.rest_endpoints.len(), 1);
        let err = parse_payload(&json!({"tables": []}), &Metadata::empty(), "$.args").unwrap_err();
        assert_eq!(
            err.error.message,
            "cannot import metadata without sources since no default source is defined"
        );
    }

    #[test]
    fn event_trigger_names() {
        let md = |name: &str| {
            hge_metadata::from_json(json!({"version": 3, "sources": [{"name": "default",
                "tables": [{"table": "a", "event_triggers": [{"name": name}]},
                           {"table": "b", "event_triggers": [{"name": "dup"}]}],
                "configuration": {}}]}))
            .unwrap()
        };
        assert!(validate_event_triggers(&md("ok_name-1")).is_ok());
        assert!(validate_event_triggers(&md("1bad")).is_err());
        assert_eq!(
            validate_event_triggers(&md("dup"))
                .unwrap_err()
                .error
                .message,
            "Event trigger with duplicate names not allowed: dup"
        );
    }
}
