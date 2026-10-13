//! Apply a Hasura CLI migrations directory on boot, like V2's `cli-migrations` image.
//!
//! Layout (config v3): `<dir>/<source name>/<version>_<name>/up.sql`. Each pending
//! migration runs in its own transaction and is recorded in the source database's
//! `hdb_catalog.schema_migrations`. Versions that the CLI recorded as applied in the metadata
//! database's `hdb_catalog.hdb_version.cli_state` are skipped too. A session advisory lock
//! keeps concurrently booting replicas from applying the same migration twice.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use hge_core::{Code, Error};
use hge_metadata::Metadata;
use hge_postgres::{PgSource, PoolConfig};
use serde_json::Value;
use tracing::{info, warn};

use crate::config::ServerConfig;
use crate::load::source_url;

/// Arbitrary constant shared by every replica (`pg_advisory_lock` key).
const LOCK_KEY: i64 = 0x0068_6765_5f6d_6967; // "hge_mig"

struct Migration {
    version: u64,
    name: String,
    up_sql: PathBuf,
}

/// Apply pending migrations for every source that has a directory under `dir`.
pub async fn apply(dir: &Path, metadata: &Metadata, config: &ServerConfig) -> Result<(), Error> {
    let entries = std::fs::read_dir(dir).map_err(|e| config_error(dir, e))?;
    let mut source_dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    source_dirs.sort();
    let cli_state = read_cli_state(config).await?;
    for source_dir in source_dirs {
        let name = source_dir.file_name().unwrap_or_default().to_string_lossy();
        let Some(source) = metadata.sources.iter().find(|s| s.name == name) else {
            warn!(target: "migrations", source = %name, "no source with this name in metadata; skipping");
            continue;
        };
        let url = source_url(source, config).map_err(|e| {
            Error::new(
                Code::InvalidConfiguration,
                format!("source \"{name}\": {e}"),
            )
        })?;
        // Versions the Hasura CLI recorded as applied (and not dirty) for this source.
        let cli_applied: HashSet<u64> = cli_state
            .get("migrations")
            .and_then(|m| m.get(name.as_ref()))
            .and_then(|m| m.as_object())
            .map(|versions| {
                versions
                    .iter()
                    .filter(|(_, dirty)| **dirty == Value::Bool(false))
                    .filter_map(|(v, _)| v.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        let migrations: Vec<Migration> = read_migrations(&source_dir)?
            .into_iter()
            .filter(|m| !cli_applied.contains(&m.version))
            .collect();
        if !migrations.is_empty() {
            apply_source(&name, &url, &migrations).await?;
        }
    }
    Ok(())
}

/// `hdb_catalog.hdb_version.cli_state` from the metadata database (where V2's CLI records
/// applied migrations per source), or `null` when there is none.
async fn read_cli_state(config: &ServerConfig) -> Result<Value, Error> {
    let Some(url) = config.catalog_url() else {
        return Ok(Value::Null);
    };
    let catalog = PgSource::connect(
        "hdb_catalog",
        url,
        &PoolConfig {
            max_connections: 1,
            use_prepared_statements: false,
            ..PoolConfig::default()
        },
    )?;
    // A missing table fails at parse time, so check for it first.
    let exists = catalog
        .query_text(
            "SELECT (to_regclass('hdb_catalog.hdb_version') IS NOT NULL)::text",
            &[],
        )
        .await?;
    if exists.as_deref() != Some("true") {
        return Ok(Value::Null);
    }
    let text = catalog
        .query_text(
            "SELECT cli_state::text FROM hdb_catalog.hdb_version LIMIT 1",
            &[],
        )
        .await?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null))
}

fn config_error(path: &Path, e: std::io::Error) -> Error {
    Error::new(
        Code::InvalidConfiguration,
        format!("cannot read migrations directory {}: {e}", path.display()),
    )
}

fn read_migrations(dir: &Path) -> Result<Vec<Migration>, Error> {
    let mut out = vec![];
    for entry in std::fs::read_dir(dir).map_err(|e| config_error(dir, e))? {
        let path = entry.map_err(|e| config_error(dir, e))?.path();
        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let Some(version) = file_name
            .split('_')
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        let up_sql = path.join("up.sql");
        if path.is_dir() && up_sql.is_file() {
            out.push(Migration {
                version,
                name: file_name,
                up_sql,
            });
        } else if path.is_dir() {
            warn!(target: "migrations", migration = %file_name, "no up.sql; skipping");
        }
    }
    out.sort_by_key(|m| m.version);
    Ok(out)
}

async fn apply_source(name: &str, url: &str, migrations: &[Migration]) -> Result<(), Error> {
    // One connection, so the session lock and every statement share it.
    let pg = PgSource::connect(
        name,
        url,
        &PoolConfig {
            max_connections: 1,
            use_prepared_statements: false,
            ..PoolConfig::default()
        },
    )?;
    pg.batch_execute(&format!(
        "SELECT pg_advisory_lock({LOCK_KEY}); \
         CREATE SCHEMA IF NOT EXISTS hdb_catalog; \
         CREATE TABLE IF NOT EXISTS hdb_catalog.schema_migrations \
           (version bigint NOT NULL PRIMARY KEY, dirty boolean NOT NULL);"
    ))
    .await?;
    let result = apply_locked(&pg, name, migrations).await;
    if result.is_err() {
        let _ = pg.batch_execute("ROLLBACK").await;
    }
    let _ = pg
        .batch_execute(&format!("SELECT pg_advisory_unlock({LOCK_KEY})"))
        .await;
    result
}

async fn apply_locked(pg: &PgSource, source: &str, migrations: &[Migration]) -> Result<(), Error> {
    let mut applied = 0;
    for m in migrations {
        let version = m.version.to_string();
        let done = pg
            .query_text(
                "SELECT 'yes' FROM hdb_catalog.schema_migrations \
                 WHERE version = ($1)::bigint AND NOT dirty",
                &[Some(&version)],
            )
            .await?;
        if done.as_deref() == Some("yes") {
            continue;
        }
        let sql = std::fs::read_to_string(&m.up_sql).map_err(|e| config_error(&m.up_sql, e))?;
        info!(target: "migrations", source, migration = %m.name, "applying migration");
        pg.batch_execute(&format!(
            "BEGIN;\n{sql}\n;\nINSERT INTO hdb_catalog.schema_migrations (version, dirty) \
             VALUES ({version}, false) ON CONFLICT (version) DO UPDATE SET dirty = false;\nCOMMIT;"
        ))
        .await
        .map_err(|e| {
            let mut e = e;
            e.message = format!("migration {} failed: {}", m.name, e.message);
            e
        })?;
        applied += 1;
    }
    info!(target: "migrations", source, applied, "migrations up to date");
    Ok(())
}
