//! Apply a Hasura CLI migrations directory on boot, like V2's `cli-migrations-v3` image.
//!
//! Layout (config v3): `<dir>/<source name>/<version>_<name>/up.sql`. The image runs
//! `hasura migrate apply --all-databases`, so state is kept exactly where the CLI keeps it:
//! `cli_state` in the metadata database's `hdb_catalog.hdb_version`
//! (`{"migrations": {"<source>": {"<version>": <dirty>}}, "settings": …,
//! "isStateCopyCompleted": true}`). A later `hasura migrate status` therefore agrees with what
//! booting applied, and booting skips what the CLI applied.
//!
//! Each pending migration runs in its own transaction on its source and is recorded right
//! after it commits, as the CLI does (it never writes a dirty flag first, so neither do we:
//! `hasura migrate status` must read the same state either way). Projects that predate the catalog state API kept their
//! state in each source's `hdb_catalog.schema_migrations`; like the CLI, the first run copies
//! it into `cli_state`. A session advisory lock on the metadata database keeps concurrently
//! booting replicas from applying the same migration twice.

use std::path::{Path, PathBuf};

use hge_core::{Code, Error};
use hge_metadata::Metadata;
use hge_metadata::store::{CatalogStateKind, MetadataStore};
use hge_postgres::{PgSource, PoolConfig};
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::config::ServerConfig;
use crate::load::source_url;

/// Arbitrary constant shared by every replica (`pg_advisory_lock` key).
const LOCK_KEY: i64 = 0x0068_6765_5f6d_6967; // "hge_mig"

#[derive(Debug)]
struct Migration {
    version: u64,
    name: String,
    up_sql: PathBuf,
}

/// Apply pending migrations for every source that has a directory under `dir`.
pub async fn apply(
    dir: &Path,
    metadata: &Metadata,
    config: &ServerConfig,
    store: &dyn MetadataStore,
) -> Result<(), Error> {
    let entries = std::fs::read_dir(dir).map_err(|e| config_error(dir, e))?;
    let mut source_dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    source_dirs.sort();
    // State lives in the metadata database; without one every boot would re-apply.
    let Some(url) = config.catalog_url() else {
        return Err(Error::new(
            Code::InvalidConfiguration,
            "HASURA_GRAPHQL_MIGRATIONS_DIR needs a metadata database \
             (HASURA_GRAPHQL_METADATA_DATABASE_URL or HASURA_GRAPHQL_DATABASE_URL)",
        ));
    };
    // Held (on its own connection) until every source is done.
    let lock = lock(url).await?;
    let result = apply_locked(&source_dirs, metadata, config, store).await;
    let _ = lock
        .batch_execute(&format!("SELECT pg_advisory_unlock({LOCK_KEY})"))
        .await;
    result
}

async fn apply_locked(
    source_dirs: &[PathBuf],
    metadata: &Metadata,
    config: &ServerConfig,
    store: &dyn MetadataStore,
) -> Result<(), Error> {
    let mut state = CliState::new(store.catalog_state().await?.cli_state);
    let mut sources = vec![];
    for source_dir in source_dirs {
        let name = source_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
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
        sources.push((name, url, source_dir));
    }
    if !state.copy_completed() {
        for (name, url, _) in &sources {
            let legacy = legacy_versions(name, url).await?;
            state.import_legacy(name, &legacy);
        }
        state.mark_copy_completed();
        store
            .set_catalog_state(CatalogStateKind::Cli, &state.value)
            .await?;
    }
    for (name, url, source_dir) in &sources {
        if let Some(v) = state.dirty(name) {
            return Err(Error::new(
                Code::InvalidConfiguration,
                format!(
                    "migrations of source \"{name}\": Dirty database version {v}. \
                     Fix and force version."
                ),
            ));
        }
        let pending: Vec<Migration> = read_migrations(source_dir)?
            .into_iter()
            .filter(|m| !state.applied(name, m.version))
            .collect();
        if pending.is_empty() {
            continue;
        }
        let pg = connect(name, url)?;
        for m in &pending {
            let sql = std::fs::read_to_string(&m.up_sql).map_err(|e| config_error(&m.up_sql, e))?;
            info!(target: "migrations", source = %name, migration = %m.name, "applying migration");
            // The CLI skips an empty up.sql but still records it as applied.
            if !sql.trim().is_empty()
                && let Err(mut e) = pg
                    .batch_execute(&format!("BEGIN;\n{sql}\n;\nCOMMIT;"))
                    .await
            {
                let _ = pg.batch_execute("ROLLBACK").await;
                e.message = format!("migration {} failed: {}", m.name, e.message);
                return Err(e);
            }
            state.record(name, m.version);
            store
                .set_catalog_state(CatalogStateKind::Cli, &state.value)
                .await?;
        }
        info!(target: "migrations", source = %name, applied = pending.len(), "migrations up to date");
    }
    Ok(())
}

/// The CLI's `cli_state` document.
struct CliState {
    value: Value,
}

impl CliState {
    fn new(value: Value) -> Self {
        let value = match value {
            Value::Object(_) => value,
            _ => json!({}),
        };
        CliState { value }
    }

    fn versions(&self, source: &str) -> Option<&Map<String, Value>> {
        self.value.get("migrations")?.get(source)?.as_object()
    }

    fn applied(&self, source: &str, version: u64) -> bool {
        self.versions(source)
            .is_some_and(|v| v.get(&version.to_string()) == Some(&Value::Bool(false)))
    }

    /// A version a failed migration left dirty.
    fn dirty(&self, source: &str) -> Option<String> {
        self.versions(source)?
            .iter()
            .find(|(_, dirty)| **dirty == Value::Bool(true))
            .map(|(v, _)| v.clone())
    }

    fn record(&mut self, source: &str, version: u64) {
        let obj = self.value.as_object_mut().expect("cli_state is an object");
        let migrations = obj.entry("migrations").or_insert_with(|| json!({}));
        if !migrations.is_object() {
            *migrations = json!({});
        }
        let versions = migrations
            .as_object_mut()
            .expect("an object")
            .entry(source)
            .or_insert_with(|| json!({}));
        if !versions.is_object() {
            *versions = json!({});
        }
        if let Some(versions) = versions.as_object_mut() {
            versions.insert(version.to_string(), Value::Bool(false));
        }
    }

    fn copy_completed(&self) -> bool {
        self.value.get("isStateCopyCompleted") == Some(&Value::Bool(true))
    }

    fn import_legacy(&mut self, source: &str, versions: &[(u64, bool)]) {
        for (version, dirty) in versions {
            self.record(source, *version);
            if *dirty
                && let Some(v) = self
                    .value
                    .pointer_mut(&format!("/migrations/{}/{version}", escape(source)))
            {
                *v = Value::Bool(true);
            }
        }
    }

    /// What the CLI writes on its first run against a server.
    fn mark_copy_completed(&mut self) {
        let obj = self.value.as_object_mut().expect("cli_state is an object");
        obj.entry("settings")
            .or_insert_with(|| json!({"migration_mode": "true"}));
        obj.insert("isStateCopyCompleted".into(), Value::Bool(true));
    }
}

/// JSON pointer escaping of one path segment.
fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// `hdb_catalog.schema_migrations` of a source (pre-catalog-state CLI versions, and this
/// engine before it kept `cli_state`).
async fn legacy_versions(name: &str, url: &str) -> Result<Vec<(u64, bool)>, Error> {
    let pg = connect(name, url)?;
    let exists = pg
        .query_text(
            "SELECT (to_regclass('hdb_catalog.schema_migrations') IS NOT NULL)::text",
            &[],
        )
        .await?;
    if exists.as_deref() != Some("true") {
        return Ok(vec![]);
    }
    let rows = pg
        .query_rows(
            "SELECT version::text, dirty::text FROM hdb_catalog.schema_migrations ORDER BY version",
            &[],
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let version = r.first()?.as_deref()?.parse().ok()?;
            Some((version, r.get(1)?.as_deref() == Some("true")))
        })
        .collect())
}

fn connect(name: &str, url: &str) -> Result<PgSource, Error> {
    // One connection, so a transaction and its statements share it.
    PgSource::connect(
        name,
        url,
        &PoolConfig {
            max_connections: 1,
            use_prepared_statements: false,
            ..PoolConfig::default()
        },
    )
}

async fn lock(catalog_url: &str) -> Result<PgSource, Error> {
    let pg = connect("hdb_catalog", catalog_url)?;
    pg.batch_execute(&format!("SELECT pg_advisory_lock({LOCK_KEY})"))
        .await?;
    Ok(pg)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_like_the_cli() {
        let mut s = CliState::new(Value::Null);
        assert!(!s.copy_completed());
        s.import_legacy("default", &[(1, false), (2, true)]);
        s.mark_copy_completed();
        assert_eq!(s.dirty("default").as_deref(), Some("2"));
        s.record("default", 2);
        s.record("other/db", 3);
        assert_eq!(
            s.value,
            json!({"migrations": {"default": {"1": false, "2": false}, "other/db": {"3": false}},
                   "settings": {"migration_mode": "true"}, "isStateCopyCompleted": true})
        );
        assert!(s.applied("default", 1) && s.applied("other/db", 3) && !s.applied("default", 4));
        assert_eq!(s.dirty("default"), None);
    }

    #[test]
    fn keeps_existing_settings() {
        let mut s = CliState::new(json!({"settings": {"migration_mode": "false"}}));
        s.mark_copy_completed();
        assert_eq!(s.value["settings"], json!({"migration_mode": "false"}));
    }
}
