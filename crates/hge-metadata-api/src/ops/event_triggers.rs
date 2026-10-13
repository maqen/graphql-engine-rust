//! Event trigger commands: `pg_create_event_trigger`, `pg_delete_event_trigger`, the
//! event-log commands (`pg_invoke_event_trigger`, `pg_redeliver_event`, `pg_get_event_logs`,
//! `pg_get_event_invocation_logs`, `pg_get_event_by_id`) and the cleanup switches.
//!
//! Creating or deleting a trigger only edits metadata: the check that follows resolves it,
//! and that is what installs or drops the SQL triggers in the source (as V2's schema cache
//! build does).

use std::sync::Arc;

use hge_core::{Code, QualifiedTable};
use hge_events::admin::{self, InvocationsOf, LogStatus, Redelivery};
use hge_events::cron::CronSchedule;
use hge_events::model::{MAX_TRIGGER_NAME_LENGTH, is_legal_trigger_name};
use hge_postgres::PgSource;
use serde_json::{Map, Value, json};

use super::args::{
    Obj, boolean, db_error, field, headers, int, missing, object, required_string, required_text,
    text, typed,
};
use crate::api::json_kind;
use crate::ctx::{Check, Ctx};
use crate::response::{
    ApiError, ApiResult, already_exists, not_exists, not_supported, parse_failed, success,
};
use crate::runtime::Runtime;

const DEFAULT_LIMIT: i64 = 100;

fn source_name(obj: &Obj, path: &str) -> ApiResult<String> {
    Ok(text(obj, "source", path)?.unwrap_or_else(|| "default".to_owned()))
}

/// `"*"` or a list of column names.
#[derive(Debug, Clone)]
enum Columns {
    Star,
    List(Vec<String>),
}

impl Columns {
    fn parse(v: &Value, path: &str) -> ApiResult<Columns> {
        match v {
            Value::String(s) if s == "*" => Ok(Columns::Star),
            Value::Array(_) => typed(v, path).map(Columns::List),
            _ => Err(parse_failed(path, "only * or [] allowed")),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Columns::Star => json!("*"),
            Columns::List(c) => json!(c),
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Columns::List(c) if c.is_empty())
    }
}

#[derive(Debug, Clone)]
struct OpSpec {
    columns: Columns,
    payload: Option<Columns>,
}

impl OpSpec {
    fn parse(v: &Value, path: &str) -> ApiResult<OpSpec> {
        let obj = v.as_object().ok_or_else(|| {
            parse_failed(
                path,
                format!(
                    "parsing Hasura.RQL.Types.EventTrigger.SubscribeOpSpec(SubscribeOpSpec) failed, expected Object, but encountered {}",
                    json_kind(v)
                ),
            )
        })?;
        let columns = obj.get("columns").ok_or_else(|| missing(path, "columns"))?;
        Ok(OpSpec {
            columns: Columns::parse(columns, &format!("{path}.columns"))?,
            payload: field(obj, "payload")
                .map(|p| Columns::parse(p, &format!("{path}.payload")))
                .transpose()?,
        })
    }

    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("columns".into(), self.columns.to_json());
        if let Some(p) = &self.payload {
            m.insert("payload".into(), p.to_json());
        }
        Value::Object(m)
    }
}

/// `pg_create_event_trigger` arguments.
#[derive(Debug, Clone)]
struct CreateEventTrigger {
    source: String,
    name: String,
    table: QualifiedTable,
    /// insert, update, delete
    ops: [Option<OpSpec>; 3],
    enable_manual: bool,
    retry_conf: Value,
    webhook: Option<String>,
    webhook_from_env: Option<String>,
    headers: Option<Vec<Value>>,
    replace: bool,
    request_transform: Option<Value>,
    response_transform: Option<Value>,
    cleanup_config: Option<Value>,
    trigger_on_replication: bool,
}

const OP_KEYS: [&str; 3] = ["insert", "update", "delete"];

impl CreateEventTrigger {
    fn parse(args: &Value) -> ApiResult<Self> {
        let p = "$.args";
        let obj = object(args, p)?;
        let source = source_name(obj, p)?;
        let name = required_string(obj, "name", p, "String")?;
        let table: QualifiedTable = typed(
            obj.get("table").ok_or_else(|| missing(p, "table"))?,
            &format!("{p}.table"),
        )?;
        let mut ops = [None, None, None];
        for (slot, key) in ops.iter_mut().zip(OP_KEYS) {
            *slot = field(obj, key)
                .map(|v| OpSpec::parse(v, &format!("{p}.{key}")))
                .transpose()?;
        }
        let retry_conf = match field(obj, "retry_conf") {
            Some(v) => retry_conf(v, &format!("{p}.retry_conf"))?,
            None => json!({"num_retries": 0, "interval_sec": 10, "timeout_sec": 60}),
        };
        let webhook = text(obj, "webhook", p)?;
        let webhook_from_env = text(obj, "webhook_from_env", p)?;
        let headers = headers(obj, p)?;
        let replace = boolean(obj, "replace", p)?.unwrap_or(false);
        let cleanup_config = field(obj, "cleanup_config")
            .map(|v| cleanup_config(v, &format!("{p}.cleanup_config")))
            .transpose()?;
        let trigger_on_replication = boolean(obj, "trigger_on_replication", p)?.unwrap_or(false);
        let enable_manual = boolean(obj, "enable_manual", p)?.unwrap_or(false);
        let args = CreateEventTrigger {
            source,
            name,
            table,
            ops,
            enable_manual,
            retry_conf,
            webhook,
            webhook_from_env,
            headers,
            replace,
            request_transform: field(obj, "request_transform").cloned(),
            response_transform: field(obj, "response_transform").cloned(),
            cleanup_config,
            trigger_on_replication,
        };
        args.validate()?;
        Ok(args)
    }

    /// V2's checks after decoding, in its order.
    fn validate(&self) -> ApiResult<()> {
        let fail = |msg: &str| Err(parse_failed("$.args", msg));
        if !is_legal_trigger_name(&self.name) {
            return fail("only alphanumeric and underscore and hyphens allowed for name");
        }
        if self.name.len() > MAX_TRIGGER_NAME_LENGTH {
            return fail("event trigger name can be at most 42 characters");
        }
        if self.ops.iter().all(Option::is_none) && !self.enable_manual {
            return fail(
                "atleast one amongst insert/update/delete/enable_manual spec must be provided",
            );
        }
        match (&self.webhook, &self.webhook_from_env) {
            (Some(_), Some(_)) => {
                return fail("only one of webhook or webhook_from_env should be given");
            }
            (None, None) => return fail("must provide webhook or webhook_from_env"),
            _ => {}
        }
        for spec in self.ops.iter().flatten() {
            // V2 only looks at the payload when the columns are `*`.
            match &spec.columns {
                c if c.is_empty() => return fail("found empty column specification"),
                Columns::Star if spec.payload.as_ref().is_some_and(Columns::is_empty) => {
                    return fail("found empty payload specification");
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The metadata entry, as V2 stores it (`enable_manual` always present).
    fn to_metadata(&self) -> Value {
        let mut definition = Map::new();
        definition.insert("enable_manual".into(), Value::Bool(self.enable_manual));
        for (spec, key) in self.ops.iter().zip(OP_KEYS) {
            if let Some(spec) = spec {
                definition.insert(key.into(), spec.to_json());
            }
        }
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("definition".into(), Value::Object(definition));
        m.insert("retry_conf".into(), self.retry_conf.clone());
        if let Some(w) = &self.webhook {
            m.insert("webhook".into(), json!(w));
        }
        if let Some(w) = &self.webhook_from_env {
            m.insert("webhook_from_env".into(), json!(w));
        }
        if let Some(h) = &self.headers {
            m.insert("headers".into(), Value::Array(h.clone()));
        }
        for (key, value) in [
            ("request_transform", &self.request_transform),
            ("response_transform", &self.response_transform),
            ("cleanup_config", &self.cleanup_config),
        ] {
            if let Some(v) = value {
                m.insert(key.into(), v.clone());
            }
        }
        if self.trigger_on_replication {
            m.insert("trigger_on_replication".into(), Value::Bool(true));
        }
        Value::Object(m)
    }

    /// Columns that fire the trigger (V2 checks only these when creating).
    fn listen_columns(&self) -> impl Iterator<Item = &String> {
        self.ops.iter().flatten().flat_map(|s| match &s.columns {
            Columns::Star => [].iter(),
            Columns::List(c) => c.iter(),
        })
    }
}

/// `RetryConf`: `num_retries` and `interval_sec` required, `timeout_sec` optional.
fn retry_conf(v: &Value, path: &str) -> ApiResult<Value> {
    let fail = |msg: String| {
        parse_failed(
            path,
            format!("parsing Hasura.RQL.Types.EventTrigger.RetryConf(RetryConf) failed, {msg}"),
        )
    };
    let obj = object(v, path)?;
    let mut out = Map::new();
    for key in ["num_retries", "interval_sec", "timeout_sec"] {
        match int(obj, key, path)? {
            Some(n) => {
                out.insert(key.into(), json!(n));
            }
            None if key == "timeout_sec" => {}
            None => return Err(fail(format!("key \"{key}\" not found"))),
        }
    }
    Ok(Value::Object(out))
}

/// `AutoTriggerLogCleanupConfig`, with V2's defaults filled in.
fn cleanup_config(v: &Value, path: &str) -> ApiResult<Value> {
    let obj = object(v, path)?;
    let schedule = required_text(obj, "schedule", path)?;
    if let Err(e) = CronSchedule::parse_v2(&schedule) {
        return Err(parse_failed(&format!("{path}.schedule"), e));
    }
    let clear_older_than =
        int(obj, "clear_older_than", path)?.ok_or_else(|| missing(path, "clear_older_than"))?;
    Ok(json!({
        "schedule": schedule,
        "batch_size": int(obj, "batch_size", path)?.unwrap_or(10000),
        "clear_older_than": clear_older_than,
        "timeout": int(obj, "timeout", path)?.unwrap_or(60),
        "clean_invocation_logs": boolean(obj, "clean_invocation_logs", path)?.unwrap_or(false),
        "paused": boolean(obj, "paused", path)?.unwrap_or(false),
    }))
}

// ---------------------------------------------------------------------------------------
// Lookups.

/// Where a trigger lives in the working metadata: (table index, trigger index).
fn find_trigger<R: Runtime>(ctx: &Ctx<'_, R>, source: &str, name: &str) -> Option<(usize, usize)> {
    let src = ctx.metadata.source(source)?;
    src.tables.iter().enumerate().find_map(|(ti, entry)| {
        entry
            .event_triggers
            .iter()
            .position(|t| t.get("name").and_then(Value::as_str) == Some(name))
            .map(|i| (ti, i))
    })
}

fn trigger_missing(name: &str) -> ApiError {
    not_exists(format!("event trigger \"{name}\" does not exist"))
}

fn source_pool<R: Runtime>(ctx: &Ctx<'_, R>, source: &str) -> ApiResult<Arc<PgSource>> {
    ctx.rt
        .source(ctx.prepared.as_ref(), source)
        .ok_or_else(|| not_exists(format!("source with name \"{source}\" does not exist")))
}

// ---------------------------------------------------------------------------------------
// Commands.

pub async fn create_event_trigger<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let q = CreateEventTrigger::parse(args)?;
    let table_index = ctx
        .metadata
        .source(&q.source)
        .and_then(|s| s.tables.iter().position(|t| t.table == q.table))
        .ok_or_else(|| {
            not_exists(format!(
                "table \"{}\" does not exist in source: {}",
                q.table.v2_text(),
                q.source
            ))
        })?;
    let existing = find_trigger(ctx, &q.source, &q.name);
    if q.replace {
        match existing {
            None => return Err(trigger_missing(&q.name)),
            Some((ti, _)) if ti != table_index => {
                return Err(not_supported("cannot replace table or schema for trigger"));
            }
            Some(_) => {}
        }
    }
    if let Some(catalog) = ctx.rt.catalog(ctx.prepared.as_ref(), &q.source)
        && let Some(table) = catalog.table(&q.table)
        && let Some(col) = q.listen_columns().find(|c| table.column(c).is_none())
    {
        return Err(not_exists(format!("column \"{col}\" does not exist")));
    }
    // V2 looks for the trigger's SQL functions in the source. Here the SQL always follows
    // metadata (leftovers are dropped when it is resolved), so metadata is what decides.
    if !q.replace && existing.is_some() {
        return Err(already_exists(format!(
            "Event trigger with name \"{}\" already exists",
            q.name
        )));
    }
    let entry = q.to_metadata();
    let triggers = &mut ctx
        .metadata
        .sources
        .iter_mut()
        .find(|s| s.name == q.source)
        .expect("source checked above")
        .tables[table_index]
        .event_triggers;
    match existing {
        Some((_, i)) => triggers[i] = entry,
        None => triggers.push(entry),
    }
    ctx.check(Check::object(
        "event_trigger",
        format!(
            "event_trigger {} in table {} in source {}",
            q.name,
            q.table.v2_text(),
            q.source
        ),
    ))
    .await?;
    Ok(success())
}

pub async fn delete_event_trigger<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let obj = object(args, "$.args")?;
    let name = required_text(obj, "name", "$.args")?;
    let source = source_name(obj, "$.args")?;
    let (ti, i) = find_trigger(ctx, &source, &name).ok_or_else(|| trigger_missing(&name))?;
    ctx.metadata
        .sources
        .iter_mut()
        .find(|s| s.name == source)
        .expect("found above")
        .tables[ti]
        .event_triggers
        .remove(i);
    // Resolving the metadata without it drops its SQL triggers. Its events are archived
    // here even when it had none (manual-only, or never installed), as V2 always does.
    ctx.changed().await?;
    let pool = source_pool(ctx, &source)?;
    admin::archive(pool.pool(), &name)
        .await
        .map_err(|e| ApiError::new(500, Code::Unexpected, e))?;
    Ok(success())
}

pub async fn invoke_event_trigger<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let obj = object(args, "$.args")?;
    let name = required_text(obj, "name", "$.args")?;
    let source = source_name(obj, "$.args")?;
    let payload = obj
        .get("payload")
        .ok_or_else(|| missing("$.args", "payload"))?;
    let (ti, i) = find_trigger(ctx, &source, &name).ok_or_else(|| trigger_missing(&name))?;
    let entry = &ctx.metadata.source(&source).expect("found above").tables[ti];
    let manual = entry.event_triggers[i]
        .pointer("/definition/enable_manual")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !manual {
        return Err(not_supported(
            "manual mode is not enabled for event trigger",
        ));
    }
    let table = entry.table.clone();
    let pool = source_pool(ctx, &source)?;
    let event_id = admin::invoke(
        pool.pool(),
        &table,
        &name,
        &payload.to_string(),
        &ctx.session.variables.to_sorted_json_string(),
        &hge_events::delivery::new_trace_context(),
    )
    .await
    .map_err(|e| ApiError::new(500, Code::Unexpected, e))?;
    Ok(json!({ "event_id": event_id }))
}

pub async fn redeliver_event<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let obj = object(args, "$.args")?;
    let event_id = required_text(obj, "event_id", "$.args")?;
    let source = source_name(obj, "$.args")?;
    let pool = source_pool(ctx, &source)?;
    match admin::redeliver(pool.pool(), &event_id)
        .await
        .map_err(|e| ApiError::new(500, Code::Unexpected, e))?
    {
        Redelivery::Queued => Ok(success()),
        Redelivery::NotFound => Err(not_exists("event not found")),
        Redelivery::Busy => Err(ApiError::bad(
            Code::Busy,
            "event is already being processed",
        )),
    }
}

/// A trigger's name, source and pool, after checking it exists.
fn existing_trigger<R: Runtime>(ctx: &Ctx<'_, R>, obj: &Obj) -> ApiResult<(String, Arc<PgSource>)> {
    let name = required_text(obj, "name", "$.args")?;
    let source = source_name(obj, "$.args")?;
    let pool = source_pool(ctx, &source)?;
    find_trigger(ctx, &source, &name).ok_or_else(|| trigger_missing(&name))?;
    Ok((name, pool))
}

pub async fn get_event_logs<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let p = "$.args";
    let obj = object(args, p)?;
    let limit = int(obj, "limit", p)?.unwrap_or(DEFAULT_LIMIT);
    let offset = int(obj, "offset", p)?.unwrap_or(0);
    let status = match field(obj, "status") {
        None => LogStatus::All,
        Some(Value::String(s)) if s == "pending" => LogStatus::Pending,
        Some(Value::String(s)) if s == "processed" => LogStatus::Processed,
        Some(_) => {
            return Err(parse_failed(
                "$.args.status",
                "event logs status can only be one of the following: processed or pending",
            ));
        }
    };
    let (name, pool) = existing_trigger(ctx, obj)?;
    admin::event_logs(pool.pool(), &name, status, limit, offset)
        .await
        .map(Value::Array)
        .map_err(|e| db_error("unexpected error while fetching event logs", e))
}

pub async fn get_event_invocation_logs<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let p = "$.args";
    let obj = object(args, p)?;
    let limit = int(obj, "limit", p)?.unwrap_or(DEFAULT_LIMIT);
    let offset = int(obj, "offset", p)?.unwrap_or(0);
    let (name, pool) = existing_trigger(ctx, obj)?;
    admin::invocation_logs(pool.pool(), InvocationsOf::Trigger(&name), limit, offset)
        .await
        .map(Value::Array)
        .map_err(|e| db_error("unexpected error while fetching invocation logs", e))
}

pub async fn get_event_by_id<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let p = "$.args";
    let obj = object(args, p)?;
    let source = source_name(obj, p)?;
    let event_id = required_text(obj, "event_id", p)?;
    let limit = int(obj, "invocation_log_limit", p)?.unwrap_or(DEFAULT_LIMIT);
    let offset = int(obj, "invocation_log_offset", p)?.unwrap_or(0);
    let pool = source_pool(ctx, &source)?;
    match admin::event_by_id(pool.pool(), &event_id, limit, offset)
        .await
        .map_err(|e| {
            db_error(
                &format!("unexpected error while fetching event with id {event_id}"),
                e,
            )
        })? {
        Some((event, invocations)) => Ok(json!({ "event": event, "invocations": invocations })),
        None => Err(not_exists(format!("event id {event_id} does not exist"))),
    }
}

pub fn cleanup_event_trigger_logs() -> ApiResult<Value> {
    Err(not_supported(
        "Event log cleanup feature is enterprise edition only",
    ))
}

/// `pause_event_trigger_cleanups` / `resume_event_trigger_cleanups`: set `paused` in the
/// `cleanup_config` of the selected triggers. With `{"sources": …}` triggers without a cleanup
/// config are skipped; naming such a trigger explicitly is an error.
pub async fn switch_event_trigger_cleanups<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
    paused: bool,
) -> ApiResult<Value> {
    let p = "$.args";
    let obj = object(args, p)?;
    let selection = obj
        .get("event_triggers")
        .ok_or_else(|| missing(p, "event_triggers"))?;
    let unknown_source = |s: &str| not_exists(format!("source with name {s} does not exists"));
    // (source, Some(trigger names) | None for all triggers with a cleanup config)
    let mut targets: Vec<(String, Option<Vec<String>>)> = vec![];
    match selection {
        Value::Object(sel) => {
            let path = "$.args.event_triggers";
            let sources = sel.get("sources").ok_or_else(|| missing(path, "sources"))?;
            let names: Vec<String> = match sources {
                Value::String(s) if s == "*" => ctx
                    .metadata
                    .sources
                    .iter()
                    .map(|s| s.name.clone())
                    .collect(),
                Value::Array(_) => typed(sources, &format!("{path}.sources"))?,
                _ => {
                    return Err(parse_failed(
                        &format!("{path}.sources"),
                        "only * or [] allowed",
                    ));
                }
            };
            targets.extend(names.into_iter().map(|s| (s, None)));
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let path = format!("$.args.event_triggers[{i}]");
                let item = object(item, &path)?;
                let source =
                    text(item, "source_name", &path)?.unwrap_or_else(|| "default".to_owned());
                let names: Vec<String> = typed(
                    item.get("event_triggers")
                        .ok_or_else(|| missing(&path, "event_triggers"))?,
                    &format!("{path}.event_triggers"),
                )?;
                targets.push((source, Some(names)));
            }
        }
        v => {
            return Err(parse_failed(
                "$.args.event_triggers",
                format!(
                    "parsing EventTriggerQualifier failed, expected Object or Array, but encountered {}",
                    json_kind(v)
                ),
            ));
        }
    }
    for (source, names) in targets {
        let src = ctx
            .metadata
            .sources
            .iter_mut()
            .find(|s| s.name == source)
            .ok_or_else(|| unknown_source(&source))?;
        let exists: Vec<String> = src
            .tables
            .iter()
            .flat_map(|t| &t.event_triggers)
            .filter_map(|t| Some(t.get("name")?.as_str()?.to_owned()))
            .collect();
        let mut configs: Vec<(String, &mut Value)> = src
            .tables
            .iter_mut()
            .flat_map(|t| t.event_triggers.iter_mut())
            .filter_map(|t| {
                let obj = t.as_object_mut()?;
                let name = obj.get("name")?.as_str()?.to_owned();
                let config = obj.get_mut("cleanup_config").filter(|c| c.is_object())?;
                Some((name, config))
            })
            .collect();
        match names {
            None => configs
                .iter_mut()
                .for_each(|(_, c)| c["paused"] = Value::Bool(paused)),
            Some(names) => {
                for name in names {
                    if !exists.contains(&name) {
                        return Err(trigger_missing(&name));
                    }
                    let (_, config) =
                        configs
                            .iter_mut()
                            .find(|(n, _)| *n == name)
                            .ok_or_else(|| {
                                not_exists(format!("cleanup config does not exist for {name}"))
                            })?;
                    config["paused"] = Value::Bool(paused);
                }
            }
        }
    }
    ctx.changed().await?;
    Ok(success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_err(args: Value) -> (String, String) {
        let e = CreateEventTrigger::parse(&args).unwrap_err();
        (e.error.path, e.error.message)
    }

    #[test]
    fn create_checks_in_v2_order() {
        let t = json!({"schema": "public", "name": "t"});
        assert_eq!(
            parse_err(json!({"name": "bad'name".to_owned() + &"a".repeat(43), "table": t})).1,
            "only alphanumeric and underscore and hyphens allowed for name"
        );
        assert_eq!(
            parse_err(json!({"name": "a".repeat(43), "table": t})).1,
            "event trigger name can be at most 42 characters"
        );
        assert_eq!(
            parse_err(json!({"name": "a", "table": t, "webhook": "x"})).1,
            "atleast one amongst insert/update/delete/enable_manual spec must be provided"
        );
        assert_eq!(
            parse_err(json!({"name": "a", "table": t, "insert": {"columns": []}})).1,
            "must provide webhook or webhook_from_env"
        );
        assert_eq!(
            parse_err(json!({"name": "a", "table": t, "insert": {"columns": "x"}})),
            (
                "$.args.insert.columns".to_owned(),
                "only * or [] allowed".to_owned()
            )
        );
        // The payload is only checked when the columns are `*` (as in V2).
        assert!(
            CreateEventTrigger::parse(
                &json!({"name": "a", "table": t, "webhook": "x", "update": {"columns": ["a"], "payload": []}})
            )
            .is_ok()
        );
        assert_eq!(
            parse_err(
                json!({"name": "a", "table": t, "webhook": "x", "update": {"columns": "*", "payload": []}})
            )
            .1,
            "found empty payload specification"
        );
    }

    #[test]
    fn stores_v2_shape() {
        let q = CreateEventTrigger::parse(&json!({
            "name": "t1", "table": "t", "insert": {"columns": "*"},
            "retry_conf": {"num_retries": 1, "interval_sec": 2}, "webhook": "http://x",
            "cleanup_config": {"schedule": "0 0 * * *", "clear_older_than": 1},
            "trigger_on_replication": false, "ignored": true
        }))
        .unwrap();
        assert_eq!(
            q.to_metadata(),
            json!({
                "name": "t1",
                "definition": {"enable_manual": false, "insert": {"columns": "*"}},
                "retry_conf": {"num_retries": 1, "interval_sec": 2},
                "webhook": "http://x",
                "cleanup_config": {"schedule": "0 0 * * *", "batch_size": 10000,
                    "clear_older_than": 1, "timeout": 60, "clean_invocation_logs": false,
                    "paused": false}
            })
        );
    }
}
