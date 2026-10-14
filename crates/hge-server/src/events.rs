//! Wiring for event triggers: resolve them for a metadata version, keep the source catalogs
//! and SQL triggers in sync, and run the delivery worker.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hge_core::InconsistentObject;
use hge_events::catalog;
use hge_events::ddl::TriggerColumn;
use hge_events::model::{ProcessEnv, is_legal_trigger_name};
use hge_events::{EventTriggerConf, EventTriggerInfo, EventTriggers, SourceTriggers};
use hge_metadata::Metadata;
use hge_postgres::PgSource;
use hge_schema::Tables;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::ServerConfig;
use crate::state::Engine;

/// An inconsistent event trigger, as V2 reports it. `reason` is usually [`in_trigger`].
fn trigger_issue(
    source: &str,
    table: &hge_core::QualifiedTable,
    trigger: &str,
    definition: &serde_json::Value,
    reason: String,
) -> InconsistentObject {
    InconsistentObject::new(
        "event_trigger",
        format!(
            "event_trigger {trigger} in table {} in source {source}",
            table.v2_text()
        ),
        reason,
        serde_json::json!({ "configuration": definition, "table": table }),
    )
}

/// A problem found while resolving a trigger, with V2's context prefix.
fn in_trigger(table: &hge_core::QualifiedTable, trigger: &str, msg: &str) -> String {
    format!(
        "in table \"{}\": in event trigger \"{trigger}\": {msg}",
        table.v2_text()
    )
}

/// A source-wide eventing problem.
fn source_issue(source: &hge_metadata::Source, msg: String) -> InconsistentObject {
    InconsistentObject::source(
        &source.name,
        msg,
        serde_json::to_value(source).unwrap_or_default(),
    )
}

/// Resolve every event trigger in `metadata` and, unless eventing is disabled, prepare the
/// source catalogs and make the installed SQL triggers match. Returns the deliverable
/// triggers and inconsistencies (a trigger with an issue is left out, as in V2).
pub async fn prepare(
    metadata: &Metadata,
    tables: &Tables,
    sources: &HashMap<String, Arc<PgSource>>,
    config: &ServerConfig,
) -> (EventTriggers, Vec<InconsistentObject>) {
    let mut inconsistencies = vec![];
    let mut out = EventTriggers::default();
    for source in &metadata.sources {
        let Some(pg) = sources.get(&source.name) else {
            continue;
        };
        let mut triggers = HashMap::new();
        let mut ddl = vec![];
        // Every trigger name in metadata, consistent or not: their SQL objects are kept.
        let mut keep = HashSet::new();
        for entry in &source.tables {
            for raw in &entry.event_triggers {
                let name = raw.get("name").and_then(|v| v.as_str()).unwrap_or("");
                keep.insert(name.to_owned());
                let conf: EventTriggerConf = match serde_json::from_value(raw.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        inconsistencies.push(trigger_issue(
                            &source.name,
                            &entry.table,
                            name,
                            raw,
                            in_trigger(&entry.table, name, &e.to_string()),
                        ));
                        continue;
                    }
                };
                let problem = if !is_legal_trigger_name(&conf.name) {
                    Some("only alphanumeric and underscore and hyphens allowed for name".to_owned())
                } else if conf.name.len() > hge_events::model::MAX_TRIGGER_NAME_LENGTH {
                    Some("event trigger name can be at most 42 characters".to_owned())
                } else if triggers.contains_key(&conf.name) {
                    Some(format!(
                        "Event trigger with duplicate names not allowed: \"{}\"",
                        conf.name
                    ))
                } else {
                    None
                };
                if let Some(p) = problem {
                    inconsistencies.push(trigger_issue(
                        &source.name,
                        &entry.table,
                        &conf.name,
                        raw,
                        in_trigger(&entry.table, &conf.name, &p),
                    ));
                    continue;
                }
                let Some(table) = tables.get(&source.name, &entry.table) else {
                    // The table itself is inconsistent; that is reported already.
                    continue;
                };
                let columns: Vec<TriggerColumn> = table
                    .columns
                    .values()
                    .map(|c| TriggerColumn {
                        name: c.name.clone(),
                        type_name: if c.ty.is_array() {
                            format!("_{}", c.ty.name)
                        } else {
                            c.ty.name.clone()
                        },
                    })
                    .collect();
                if let Some(missing) = listen_columns(&conf)
                    .into_iter()
                    .find(|c| !table.columns.contains_key(*c))
                {
                    inconsistencies.push(trigger_issue(
                        &source.name,
                        &entry.table,
                        &conf.name,
                        raw,
                        // A dependency error in V2, without the trigger context.
                        format!(
                            "table \"{}\" has no field named \"{missing}\"",
                            entry.table.v2_text()
                        ),
                    ));
                    continue;
                }
                match EventTriggerInfo::resolve(&entry.table, conf, &ProcessEnv) {
                    Ok(info) => {
                        ddl.push(info.ddl(columns, config.stringify_numeric_types));
                        triggers.insert(info.name.clone(), Arc::new(info));
                    }
                    Err(e) => inconsistencies.push(trigger_issue(
                        &source.name,
                        &entry.table,
                        name,
                        raw,
                        in_trigger(&entry.table, name, &e),
                    )),
                }
            }
        }
        if !config.events.disable_eventing && !triggers.is_empty() {
            let schema = source
                .configuration
                .extra
                .get("extensions_schema")
                .and_then(|v| v.as_str())
                .unwrap_or("public");
            if let Err(e) = catalog::init_source_catalog(pg.pool(), schema).await {
                inconsistencies.push(source_issue(
                    source,
                    format!("could not prepare the event trigger catalog: {e}"),
                ));
                continue;
            }
        }
        if !config.events.disable_eventing {
            match catalog::sync_triggers(pg.pool(), &ddl, &keep).await {
                Ok(report) => {
                    for (name, e) in &report.errors {
                        triggers.remove(name);
                        inconsistencies.push(source_issue(
                            source,
                            format!("in event trigger \"{name}\": {e}"),
                        ));
                    }
                    if !report.removed.is_empty() {
                        info!(
                            target: "startup",
                            source = %source.name,
                            removed = ?report.removed,
                            "dropped event triggers no longer in metadata and archived their events"
                        );
                    }
                    if report.functions_written
                        + report.triggers_created
                        + report.functions_dropped
                        + report.triggers_dropped
                        > 0
                    {
                        info!(
                            target: "startup",
                            source = %source.name,
                            functions_written = report.functions_written,
                            triggers_created = report.triggers_created,
                            functions_dropped = report.functions_dropped,
                            "event triggers installed"
                        );
                    }
                }
                Err(e) => {
                    inconsistencies.push(source_issue(
                        source,
                        format!("could not create event triggers: {e}"),
                    ));
                    continue;
                }
            }
        }
        out.sources.push(Arc::new(SourceTriggers {
            source: source.name.clone(),
            pool: pg.pool().clone(),
            triggers,
        }));
    }
    (out, inconsistencies)
}

/// Columns a trigger listens to or sends in its payload; each must exist.
fn listen_columns(conf: &EventTriggerConf) -> Vec<&str> {
    use hge_events::model::SubscribeColumns;
    [
        &conf.definition.insert,
        &conf.definition.update,
        &conf.definition.delete,
    ]
    .into_iter()
    .flatten()
    .flat_map(|spec| [Some(&spec.columns), spec.payload.as_ref()])
    .flatten()
    .flat_map(|cols| match cols {
        SubscribeColumns::Star => vec![],
        SubscribeColumns::Columns(c) => c.iter().map(String::as_str).collect(),
    })
    .collect()
}

/// Start the delivery worker if this instance processes events.
pub fn spawn_worker(engine: &Arc<Engine>, shutdown: CancellationToken) -> Option<JoinHandle<()>> {
    let config = engine.config.events.clone();
    if !config.processing_enabled() {
        info!(target: "startup", "event trigger processing is disabled on this instance");
        return None;
    }
    let client = match hge_events::webhook::HttpClient::new() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            warn!(target: "startup", "event triggers disabled: cannot create HTTP client: {e}");
            return None;
        }
    };
    let engine = engine.clone();
    let provider = Arc::new(move || engine.state.load().event_triggers.clone());
    Some(hge_events::processor::spawn(
        config, client, provider, shutdown,
    ))
}
