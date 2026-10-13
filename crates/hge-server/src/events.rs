//! Wiring for event triggers: resolve them for a metadata version, keep the source catalogs
//! and SQL triggers in sync, and run the delivery worker.

use std::collections::HashMap;
use std::sync::Arc;

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

fn context(table: &hge_core::QualifiedTable, trigger: &str, msg: &str) -> String {
    format!("in table \"{table}\": in event trigger \"{trigger}\": {msg}")
}

/// Resolve every event trigger in `metadata` and, unless eventing is disabled, prepare the
/// source catalogs and make the installed SQL triggers match. Returns the deliverable
/// triggers and inconsistencies (a trigger with an issue is left out, as in V2).
pub async fn prepare(
    metadata: &Metadata,
    tables: &Tables,
    sources: &HashMap<String, Arc<PgSource>>,
    config: &ServerConfig,
) -> (EventTriggers, Vec<String>) {
    let mut inconsistencies = vec![];
    let mut out = EventTriggers::default();
    for source in &metadata.sources {
        let Some(pg) = sources.get(&source.name) else {
            continue;
        };
        let mut triggers = HashMap::new();
        let mut ddl = vec![];
        for entry in &source.tables {
            for raw in &entry.event_triggers {
                let name = raw.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let conf: EventTriggerConf = match serde_json::from_value(raw.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        inconsistencies.push(context(&entry.table, name, &e.to_string()));
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
                } else if conf.request_transform.is_some() || conf.response_transform.is_some() {
                    // Delivering untransformed bodies would break the webhook; leave the
                    // events queued until transforms are supported.
                    Some("request/response transforms are not supported yet".to_owned())
                } else {
                    None
                };
                if let Some(p) = problem {
                    inconsistencies.push(context(&entry.table, &conf.name, &p));
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
                    inconsistencies.push(context(
                        &entry.table,
                        &conf.name,
                        &format!("column \"{missing}\" does not exist"),
                    ));
                    continue;
                }
                match EventTriggerInfo::resolve(&entry.table, conf, &ProcessEnv) {
                    Ok(info) => {
                        ddl.push(info.ddl(columns, config.stringify_numeric_types));
                        triggers.insert(info.name.clone(), Arc::new(info));
                    }
                    Err(e) => inconsistencies.push(context(&entry.table, name, &e)),
                }
            }
        }
        if triggers.is_empty() {
            continue;
        }
        if !config.events.disable_eventing {
            let schema = source
                .configuration
                .extra
                .get("extensions_schema")
                .and_then(|v| v.as_str())
                .unwrap_or("public");
            if let Err(e) = catalog::init_source_catalog(pg.pool(), schema).await {
                inconsistencies.push(format!(
                    "source \"{}\": could not prepare the event trigger catalog: {e}",
                    source.name
                ));
                continue;
            }
            match catalog::sync_triggers(pg.pool(), &ddl).await {
                Ok(report) => {
                    for (name, e) in &report.errors {
                        triggers.remove(name);
                        inconsistencies.push(format!(
                            "source \"{}\": in event trigger \"{name}\": {e}",
                            source.name
                        ));
                    }
                    if report.functions_written + report.triggers_created + report.functions_dropped
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
                    inconsistencies.push(format!(
                        "source \"{}\": could not create event triggers: {e}",
                        source.name
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

fn listen_columns(conf: &EventTriggerConf) -> Vec<&str> {
    use hge_events::model::SubscribeColumns;
    [
        &conf.definition.insert,
        &conf.definition.update,
        &conf.definition.delete,
    ]
    .into_iter()
    .flatten()
    .flat_map(|spec| match &spec.columns {
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
