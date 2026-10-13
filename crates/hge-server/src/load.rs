//! Loading metadata at startup and resolving it against the databases (also on every
//! metadata change).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hge_actions::{ActionRegistry, HandlerConfig, ProcessEnv};
use hge_core::{Code, Error, InconsistentObject, QualifiedFunction, QualifiedTable, SourceCatalog};
use hge_metadata::store::MetadataStore;
use hge_metadata::{Metadata, PgConnectionInfo, Source, SourceConfiguration, UrlConfig, load_dir};
use hge_metadata_api::Reload;
use hge_postgres::{IsolationLevel, PgSource, PoolConfig};
use hge_schema::{ResolvedActions, SchemaOptions, Tables, resolve_actions, resolve_tables};
use serde_json::json;
use tracing::{info, warn};

use crate::config::ServerConfig;

/// Where the metadata came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataOrigin {
    Directory,
    Catalog,
    Default,
}

/// Load metadata following V2's precedence (see architecture §5) and make sure the store
/// holds it. Returns the metadata and its resource version.
pub async fn load_metadata(
    config: &ServerConfig,
    store: &dyn MetadataStore,
) -> Result<(Metadata, i64, MetadataOrigin), Error> {
    if let Some(dir) = &config.metadata_dir {
        let md =
            load_dir(dir).map_err(|e| Error::new(Code::InvalidConfiguration, e.to_string()))?;
        // Like the cli-migrations image's `hasura metadata apply`: the directory replaces
        // the stored metadata. Replicas booting together may race; retry on conflict.
        let doc = hge_metadata::export_json(&md);
        let mut attempts = 0;
        let version = loop {
            let current = store.resource_version().await?;
            match store.store(&doc, current).await {
                Ok(v) => break v,
                Err(e) if e.code == Code::Conflict && attempts < 5 => attempts += 1,
                Err(e) => return Err(e),
            }
        };
        return Ok((md, version, MetadataOrigin::Directory));
    }
    if let Some(stored) = store.fetch().await? {
        let md = hge_metadata::from_json(stored.metadata)?;
        return Ok((md, stored.resource_version, MetadataOrigin::Catalog));
    }
    let md = default_metadata(config);
    let version = match store.store(&hge_metadata::export_json(&md), 0).await {
        Ok(v) => v,
        // Another replica stored the initial metadata first: use theirs.
        Err(e) if e.code == Code::Conflict => {
            if let Some(stored) = store.fetch().await? {
                let md = hge_metadata::from_json(stored.metadata)?;
                return Ok((md, stored.resource_version, MetadataOrigin::Catalog));
            }
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    Ok((md, version, MetadataOrigin::Default))
}

/// Fresh-database metadata: a `default` source on `HASURA_GRAPHQL_DATABASE_URL`, like V2.
fn default_metadata(config: &ServerConfig) -> Metadata {
    let mut md = Metadata {
        version: 3,
        ..Metadata::default()
    };
    if config.database_url.is_some() {
        md.sources.push(
            serde_json::from_value(json!({
                "name": "default",
                "kind": "postgres",
                "tables": [],
                "configuration": {
                    "connection_info": {
                        "database_url": {"from_env": "HASURA_GRAPHQL_DATABASE_URL"},
                        "isolation_level": "read-committed",
                        "pool_settings": {"connection_lifetime": 600, "retries": 1, "idle_timeout": 180, "max_connections": 50},
                        "use_prepared_statements": true
                    }
                }
            }))
            .expect("valid default source"),
        );
    }
    md
}

fn resolve_url(url: &UrlConfig, config: &ServerConfig) -> Result<String, String> {
    match url {
        UrlConfig::Literal(s) => Ok(s.clone()),
        UrlConfig::FromEnv { from_env } => match std::env::var(from_env) {
            Ok(v) => Ok(v),
            Err(_) if from_env == "HASURA_GRAPHQL_DATABASE_URL" => config
                .database_url
                .clone()
                .ok_or_else(|| format!("environment variable \"{from_env}\" not set")),
            Err(_) => Err(format!("environment variable \"{from_env}\" not set")),
        },
        UrlConfig::Parameters {
            connection_parameters: p,
        } => {
            let enc = |s: &str| {
                s.bytes()
                    .map(|b| {
                        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                            (b as char).to_string()
                        } else {
                            format!("%{b:02X}")
                        }
                    })
                    .collect::<String>()
            };
            let auth = match &p.password {
                Some(pw) => format!("{}:{}", enc(&p.username), enc(pw)),
                None => enc(&p.username),
            };
            Ok(format!(
                "postgres://{auth}@{}:{}/{}",
                p.host,
                p.port,
                enc(&p.database)
            ))
        }
    }
}

fn pool_config(info: &PgConnectionInfo, config: &ServerConfig) -> PoolConfig {
    let ps = info.pool_settings.clone().unwrap_or_default();
    PoolConfig {
        max_connections: ps
            .max_connections
            .map(|n| n as usize)
            .unwrap_or(config.pg_connections),
        idle_timeout: ps.idle_timeout,
        pool_timeout: ps.pool_timeout,
        use_prepared_statements: info
            .use_prepared_statements
            .unwrap_or(config.use_prepared_statements),
        isolation_level: info
            .isolation_level
            .as_deref()
            .and_then(IsolationLevel::from_metadata)
            .unwrap_or_default(),
    }
}

/// The resolved connection URL of a Postgres source.
pub fn source_url(source: &Source, config: &ServerConfig) -> Result<String, String> {
    let info = connection_info(source).ok_or("missing connection_info")?;
    resolve_url(&info.database_url, config)
}

fn connection_info(source: &Source) -> Option<&PgConnectionInfo> {
    let SourceConfiguration {
        connection_info, ..
    } = &source.configuration;
    connection_info.as_ref()
}

/// A connected source, with what was introspected from it.
#[derive(Clone)]
pub struct PreparedSource {
    /// The source's configuration and resolved URL; a change means a new pool.
    key: String,
    pub pg: Arc<PgSource>,
    pub catalog: Arc<SourceCatalog>,
    /// The tables and functions `catalog` was introspected for.
    introspected: Introspected,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Introspected {
    tables: HashSet<QualifiedTable>,
    functions: HashSet<QualifiedFunction>,
}

impl Introspected {
    fn covers(&self, wanted: &Introspected) -> bool {
        wanted.tables.is_subset(&self.tables) && wanted.functions.is_subset(&self.functions)
    }
}

/// Metadata resolved against the databases: everything but the per-role schemas.
#[derive(Clone, Default)]
pub struct Prepared {
    pub sources: HashMap<String, PreparedSource>,
    pub tables: Arc<Tables>,
    /// Deliverable event triggers.
    pub event_triggers: Arc<hge_events::EventTriggers>,
    /// Resolved cron triggers.
    pub scheduled_triggers: Arc<hge_events::scheduled::ScheduledTriggers>,
    pub actions: Arc<ResolvedActions>,
    pub action_registry: Arc<ActionRegistry>,
    pub inconsistencies: Vec<InconsistentObject>,
}

fn source_inconsistency(source: &Source, reason: impl Into<String>) -> InconsistentObject {
    InconsistentObject::source(
        &source.name,
        reason,
        serde_json::to_value(source).unwrap_or_default(),
    )
}

/// Connect sources, introspect them, and resolve the schema inputs. Pools and catalogs of
/// `base` are reused for sources whose configuration did not change, unless `reload` says
/// otherwise; `catalogs` replaces introspection for the sources it names.
pub async fn prepare(
    metadata: &Metadata,
    config: &ServerConfig,
    base: Option<&Prepared>,
    reload: &Reload,
    mut catalogs: HashMap<String, SourceCatalog>,
) -> Prepared {
    let mut sources = HashMap::new();
    let mut inconsistencies = vec![];

    for source in &metadata.sources {
        if !matches!(
            source.kind.as_str(),
            "postgres" | "pg" | "citus" | "cockroach"
        ) {
            inconsistencies.push(source_inconsistency(
                source,
                format!("backend kind \"{}\" is not supported yet", source.kind),
            ));
            continue;
        }
        let Some(info) = connection_info(source) else {
            inconsistencies.push(source_inconsistency(source, "missing connection_info"));
            continue;
        };
        let url = match resolve_url(&info.database_url, config) {
            Ok(u) => u,
            Err(e) => {
                inconsistencies.push(source_inconsistency(source, e));
                continue;
            }
        };
        let key = format!(
            "{}\n{url}",
            serde_json::to_string(&source.configuration).unwrap_or_default()
        );
        let previous = base
            .and_then(|b| b.sources.get(&source.name))
            .filter(|p| p.key == key && !reload.reconnects(&source.name));
        let pg = match previous {
            Some(p) => p.pg.clone(),
            None => match PgSource::connect(&source.name, &url, &pool_config(info, config)) {
                Ok(pg) => Arc::new(pg),
                Err(e) => {
                    inconsistencies.push(source_inconsistency(source, e.message));
                    continue;
                }
            },
        };
        let tracked = Introspected {
            tables: source.tables.iter().map(|t| t.table.clone()).collect(),
            functions: hge_schema::functions::referenced_functions(source)
                .into_iter()
                .collect(),
        };
        let (catalog, introspected) = if let Some(c) = catalogs.remove(&source.name) {
            (Arc::new(c), tracked)
        } else if let Some(p) = previous
            .filter(|p| !reload.introspects(&source.name) && p.introspected.covers(&tracked))
        {
            (p.catalog.clone(), p.introspected.clone())
        } else {
            let tables: Vec<_> = tracked.tables.iter().cloned().collect();
            let functions: Vec<_> = tracked.functions.iter().cloned().collect();
            match pg.introspect(&tables, &functions).await {
                Ok(c) => (Arc::new(c), tracked),
                Err(e) => {
                    inconsistencies.push(source_inconsistency(
                        source,
                        format!("could not introspect database: {}", e.message),
                    ));
                    continue;
                }
            }
        };
        sources.insert(
            source.name.clone(),
            PreparedSource {
                key,
                pg,
                catalog,
                introspected,
            },
        );
    }

    let catalog_refs: HashMap<String, Arc<SourceCatalog>> = sources
        .iter()
        .map(|(name, s)| (name.clone(), s.catalog.clone()))
        .collect();
    let (tables, table_issues) = resolve_tables(metadata, &catalog_refs);
    inconsistencies.extend(table_issues);
    let pg_sources: HashMap<String, Arc<PgSource>> = sources
        .iter()
        .map(|(name, s)| (name.clone(), s.pg.clone()))
        .collect();
    let (event_triggers, event_issues) =
        crate::events::prepare(metadata, &tables, &pg_sources, config).await;
    inconsistencies.extend(event_issues);
    let (scheduled_triggers, cron_issues) = crate::scheduled::prepare(metadata);
    inconsistencies.extend(cron_issues);
    let (mut actions, action_issues) = resolve_actions(metadata, &tables);
    inconsistencies.extend(action_issues);
    let registry = action_registry(metadata, &mut actions, config, &mut inconsistencies);
    Prepared {
        sources,
        tables: Arc::new(tables),
        event_triggers: Arc::new(event_triggers),
        scheduled_triggers: Arc::new(scheduled_triggers),
        actions: Arc::new(actions),
        action_registry: Arc::new(registry),
        inconsistencies,
    }
}

/// Log a freshly prepared state.
pub fn log_prepared(prepared: &Prepared) {
    for issue in &prepared.inconsistencies {
        warn!(target: "metadata", "inconsistent metadata: {issue}");
    }
    info!(
        target: "startup",
        sources = prepared.sources.len(),
        tables = prepared.tables.ordered.len(),
        "metadata loaded"
    );
}

/// The schema options derived from the server configuration.
pub fn schema_options(config: &ServerConfig) -> SchemaOptions {
    SchemaOptions {
        stringify_numeric_types: config.stringify_numeric_types,
        dangerously_collapse_booleans: config.v1_boolean_null_collapse,
    }
}

/// Handler configuration for the resolved actions; actions whose handler cannot be resolved
/// are dropped from the schema as inconsistent.
fn action_registry(
    metadata: &Metadata,
    actions: &mut hge_schema::ResolvedActions,
    config: &ServerConfig,
    inconsistencies: &mut Vec<InconsistentObject>,
) -> ActionRegistry {
    let outputs = actions
        .actions
        .values()
        .map(|a| (a.name.clone(), a.output.clone()))
        .collect();
    let env: Arc<dyn hge_actions::Environment> = Arc::new(hge_actions::env::Overrides {
        values: config.env_overrides.clone(),
        fallback: ProcessEnv,
    });
    let handler_config = HandlerConfig {
        user_agent: format!("hasura-graphql-engine/{}", crate::routes::VERSION),
        configured_header_precedence: config.configured_header_precedence,
        ..HandlerConfig::default()
    };
    let built = ActionRegistry::build(metadata, &outputs, env, handler_config);
    for name in &built.failed {
        actions.actions.shift_remove(name);
    }
    inconsistencies.extend(built.inconsistencies);
    built.registry
}
