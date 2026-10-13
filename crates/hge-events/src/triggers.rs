//! Event triggers resolved for one metadata version (V2's `EventTriggerInfo` in the schema
//! cache): what the delivery worker reads, swapped atomically with the rest of the state.

use std::collections::HashMap;
use std::sync::Arc;

use deadpool_postgres::Pool;
use hge_core::QualifiedTable;

use crate::catalog::TriggerDdl;
use crate::ddl::TriggerColumn;
use crate::model::{
    DEFAULT_TIMEOUT_SECONDS, Env, EventTriggerConf, ResolvedHeader, ResolvedWebhook, RetryConf,
    resolve_headers, resolve_webhook,
};

/// One deliverable event trigger.
#[derive(Debug, Clone)]
pub struct EventTriggerInfo {
    pub name: String,
    pub table: QualifiedTable,
    pub webhook: ResolvedWebhook,
    pub headers: Vec<ResolvedHeader>,
    pub retry_conf: RetryConf,
    pub conf: EventTriggerConf,
}

impl EventTriggerInfo {
    /// Resolve a trigger's webhook and headers from the environment. The error is V2's
    /// inconsistency message (without the table/trigger context prefix).
    pub fn resolve(
        table: &QualifiedTable,
        conf: EventTriggerConf,
        env: &dyn Env,
    ) -> Result<Self, String> {
        let webhook = resolve_webhook(
            conf.webhook.as_deref(),
            conf.webhook_from_env.as_deref(),
            env,
        )?;
        let headers = resolve_headers(&conf.headers, env)?;
        Ok(EventTriggerInfo {
            name: conf.name.clone(),
            table: table.clone(),
            webhook,
            headers,
            retry_conf: conf.retry_conf,
            conf,
        })
    }

    /// Bound on processing one event: `min(30 min, timeout_sec)`, 30 min when unset.
    pub fn processing_timeout(&self) -> std::time::Duration {
        match self.retry_conf.timeout_sec {
            Some(t) => std::time::Duration::from_secs(t.max(0) as u64)
                .min(crate::delivery::MAX_PROCESSING_TIMEOUT),
            None => crate::delivery::MAX_PROCESSING_TIMEOUT,
        }
    }

    pub fn timeout_seconds(&self) -> u64 {
        self.retry_conf
            .timeout_sec
            .map(|t| t.max(0) as u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
    }

    /// The DDL input for this trigger, given the table's current columns.
    pub fn ddl(&self, columns: Vec<TriggerColumn>, stringify_numeric: bool) -> TriggerDdl {
        TriggerDdl {
            name: self.name.clone(),
            table: self.table.clone(),
            definition: self.conf.definition.clone(),
            columns,
            stringify_numeric,
            trigger_on_replication: self.conf.trigger_on_replication,
        }
    }
}

/// The event triggers of one source.
pub struct SourceTriggers {
    pub source: String,
    pub pool: Pool,
    /// By trigger name (names are unique per source).
    pub triggers: HashMap<String, Arc<EventTriggerInfo>>,
}

impl SourceTriggers {
    pub fn trigger_names(&self) -> Vec<String> {
        self.triggers.keys().cloned().collect()
    }
}

/// All event triggers of a metadata version.
#[derive(Default)]
pub struct EventTriggers {
    pub sources: Vec<Arc<SourceTriggers>>,
}

impl EventTriggers {
    pub fn source(&self, name: &str) -> Option<&Arc<SourceTriggers>> {
        self.sources.iter().find(|s| s.source == name)
    }

    pub fn is_empty(&self) -> bool {
        self.sources.iter().all(|s| s.triggers.is_empty())
    }
}
