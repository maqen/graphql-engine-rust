//! Wiring for cron triggers and one-off scheduled events (metadata database).

use std::sync::Arc;

use hge_core::InconsistentObject;
use hge_events::model::ProcessEnv;
use hge_events::scheduled::{CronTriggerConf, CronTriggerInfo, ScheduledTriggers};
use hge_metadata::Metadata;
use hge_postgres::{PoolConfig, connect_pool};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::state::Engine;

/// Resolve the cron triggers of a metadata version. Inconsistent ones are left out, as in V2.
pub fn prepare(metadata: &Metadata) -> (ScheduledTriggers, Vec<InconsistentObject>) {
    let mut out = ScheduledTriggers::default();
    let mut issues = vec![];
    for raw in &metadata.cron_triggers {
        let name = raw
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        out.in_metadata.insert(name.clone());
        let resolved = serde_json::from_value::<CronTriggerConf>(raw.clone())
            .map_err(|e| e.to_string())
            .and_then(|conf| {
                if conf.request_transform.is_some() || conf.response_transform.is_some() {
                    return Err("request/response transforms are not supported yet".to_owned());
                }
                CronTriggerInfo::resolve(conf, &ProcessEnv)
            });
        match resolved {
            Ok(info) => {
                out.cron.insert(name, Arc::new(info));
            }
            Err(e) => issues.push(InconsistentObject::new(
                "cron_trigger",
                format!("cron_trigger {name}"),
                format!("in cron trigger {name}: {e}"),
                hge_events::admin::sorted(raw.clone()),
            )),
        }
    }
    (out, issues)
}

/// Start the cron generator and scheduled-event delivery if this instance processes events
/// and there is a metadata database.
pub fn spawn_worker(engine: &Arc<Engine>, shutdown: CancellationToken) -> Option<JoinHandle<()>> {
    let config = engine.config.events.clone();
    if config.disable_eventing || config.disable_event_processing {
        return None;
    }
    let url = engine.config.catalog_url()?;
    let pool = match connect_pool(
        url,
        &PoolConfig {
            max_connections: 5,
            ..PoolConfig::default()
        },
    ) {
        Ok((pool, _)) => pool,
        Err(e) => {
            warn!(target: "startup", "scheduled events disabled: {}", e.message);
            return None;
        }
    };
    let client = match hge_events::webhook::HttpClient::new() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            warn!(target: "startup", "scheduled events disabled: cannot create HTTP client: {e}");
            return None;
        }
    };
    info!(target: "startup", "starting scheduled event processing");
    let engine = engine.clone();
    let provider = Arc::new(move || engine.state.load().scheduled_triggers.clone());
    Some(hge_events::scheduled::spawn(
        config,
        pool,
        client,
        provider,
        Arc::new(ProcessEnv),
        shutdown,
    ))
}
