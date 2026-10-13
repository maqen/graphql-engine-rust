//! Building [`AppState`] at startup (and, later, on metadata reload).

use std::collections::HashMap;
use std::sync::Arc;

use hge_core::{Code, Error};
use hge_metadata::{Metadata, PgConnectionInfo, Source, SourceConfiguration, UrlConfig, load_dir};
use hge_postgres::{PgSource, PoolConfig};
use hge_schema::{SchemaOptions, resolve_tables};
use serde_json::json;
use tracing::{info, warn};

use crate::config::ServerConfig;
use crate::state::AppState;

/// Where the metadata came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataOrigin {
    Directory,
    Catalog,
    Default,
}

/// Load metadata following V2's precedence (see architecture §5).
pub async fn load_metadata(config: &ServerConfig) -> Result<(Metadata, MetadataOrigin), Error> {
    if let Some(dir) = &config.metadata_dir {
        let md =
            load_dir(dir).map_err(|e| Error::new(Code::InvalidConfiguration, e.to_string()))?;
        return Ok((md, MetadataOrigin::Directory));
    }
    if let Some(url) = config.catalog_url() {
        let catalog = PgSource::connect(
            "hdb_catalog",
            url,
            &PoolConfig {
                max_connections: 1,
                ..PoolConfig::default()
            },
        )?;
        // A missing table fails at parse time, so check for it in its own statement.
        let exists = catalog
            .query_text(
                "SELECT (to_regclass('hdb_catalog.hdb_metadata') IS NOT NULL)::text",
                &[],
            )
            .await?;
        let stored = if exists.as_deref() == Some("true") {
            catalog
                .query_text(
                    "SELECT metadata::text FROM hdb_catalog.hdb_metadata WHERE id = 1",
                    &[],
                )
                .await?
        } else {
            None
        };
        if let Some(text) = stored {
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| Error::unexpected(format!("invalid stored metadata: {e}")))?;
            return Ok((hge_metadata::from_json(value)?, MetadataOrigin::Catalog));
        }
    }
    Ok((default_metadata(config), MetadataOrigin::Default))
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

/// Connect sources, introspect them, and resolve the schema inputs.
pub async fn build_state(metadata: Metadata, config: &ServerConfig) -> AppState {
    let mut sources = HashMap::new();
    let mut catalogs = HashMap::new();
    let mut inconsistencies = vec![];

    for source in &metadata.sources {
        if !matches!(
            source.kind.as_str(),
            "postgres" | "pg" | "citus" | "cockroach"
        ) {
            inconsistencies.push(format!(
                "source \"{}\": backend kind \"{}\" is not supported yet",
                source.name, source.kind
            ));
            continue;
        }
        let Some(info) = connection_info(source) else {
            inconsistencies.push(format!(
                "source \"{}\": missing connection_info",
                source.name
            ));
            continue;
        };
        let url = match resolve_url(&info.database_url, config) {
            Ok(u) => u,
            Err(e) => {
                inconsistencies.push(format!("source \"{}\": {e}", source.name));
                continue;
            }
        };
        let pg = match PgSource::connect(&source.name, &url, &pool_config(info, config)) {
            Ok(pg) => Arc::new(pg),
            Err(e) => {
                inconsistencies.push(format!("source \"{}\": {}", source.name, e.message));
                continue;
            }
        };
        let tracked: Vec<_> = source.tables.iter().map(|t| t.table.clone()).collect();
        match pg.introspect(&tracked).await {
            Ok(catalog) => {
                catalogs.insert(source.name.clone(), catalog);
                sources.insert(source.name.clone(), pg);
            }
            Err(e) => inconsistencies.push(format!(
                "source \"{}\": could not introspect database: {}",
                source.name, e.message
            )),
        }
    }

    let (tables, table_issues) = resolve_tables(&metadata, &catalogs);
    inconsistencies.extend(table_issues);
    for issue in &inconsistencies {
        warn!(target: "metadata", "inconsistent metadata: {issue}");
    }
    info!(
        target: "startup",
        sources = sources.len(),
        tables = tables.ordered.len(),
        "metadata loaded"
    );
    let options = SchemaOptions {
        stringify_numeric_types: config.stringify_numeric_types,
        dangerously_collapse_booleans: config.v1_boolean_null_collapse,
    };
    AppState::new(metadata, tables, sources, inconsistencies, options)
}
