//! Source catalog set-up and the per-trigger SQL objects (DDL against a source database).
//!
//! All DDL runs under a transaction-scoped advisory lock, so several engine replicas booting
//! at once serialise instead of failing with "tuple concurrently updated". Trigger functions
//! are only replaced when their body differs from what is installed, so a restart issues no
//! DDL at all when nothing changed.

use std::collections::{HashMap, HashSet};

use deadpool_postgres::{Pool, Transaction};
use hge_core::QualifiedTable;
use tracing::info;

use crate::ddl::{self, SOURCE_CATALOG_VERSION, TriggerColumn};
use crate::model::{Op, TriggerOpsDef};

/// `hashtext('hasura-rs:event-catalog')`-style constant; any fixed key works.
const DDL_LOCK_KEY: i64 = 0x4855_5241_4556_5431;

/// Errors are plain messages: callers turn them into inconsistencies or API errors.
pub type DdlError = String;

pub(crate) fn pg_err(e: tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db.message().to_owned(),
        None => e.to_string(),
    }
}

/// What [`init_source_catalog`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogInit {
    /// The catalog was already at the current version.
    UpToDate,
    /// The catalog was created from scratch.
    Created,
    /// An older catalog was migrated; trigger functions should be recreated.
    Migrated,
}

async fn lock(tx: &Transaction<'_>) -> Result<(), DdlError> {
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&DDL_LOCK_KEY])
        .await
        .map_err(pg_err)?;
    Ok(())
}

async fn exists(tx: &Transaction<'_>, regclass: &str) -> Result<bool, DdlError> {
    let row = tx
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&regclass])
        .await
        .map_err(pg_err)?;
    Ok(row.get(0))
}

/// Create `pgcrypto` if it is missing (V2 does this for `gen_random_uuid()`). On Postgres 13+
/// `gen_random_uuid()` is built in, so a failure (no privilege) is not fatal there.
async fn ensure_pgcrypto(tx: &Transaction<'_>, extensions_schema: &str) -> Result<(), DdlError> {
    let present: bool = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pgcrypto')",
            &[],
        )
        .await
        .map_err(pg_err)?
        .get(0);
    if present {
        return Ok(());
    }
    let sql = format!(
        "CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA {}",
        ddl::quote_ident(extensions_schema)
    );
    tx.batch_execute("SAVEPOINT hge_pgcrypto")
        .await
        .map_err(pg_err)?;
    match tx.batch_execute(&sql).await {
        Ok(()) => Ok(()),
        Err(e) => {
            tx.batch_execute("ROLLBACK TO SAVEPOINT hge_pgcrypto")
                .await
                .map_err(pg_err)?;
            let version: i32 = tx
                .query_one("SELECT current_setting('server_version_num')::int", &[])
                .await
                .map_err(pg_err)?
                .get(0);
            if version >= 130000 {
                Ok(())
            } else {
                Err(pg_err(e))
            }
        }
    }
}

async fn set_version(tx: &Transaction<'_>) -> Result<(), DdlError> {
    tx.execute(
        "INSERT INTO hdb_catalog.hdb_source_catalog_version(version, upgraded_on) VALUES ($1, NOW()) \
         ON CONFLICT ((version IS NOT NULL)) DO UPDATE SET version = $1, upgraded_on = NOW()",
        &[&SOURCE_CATALOG_VERSION.to_string()],
    )
    .await
    .map_err(pg_err)?;
    Ok(())
}

/// V2's `prepareCatalog` for a source that has event triggers: create or migrate
/// `hdb_catalog.event_log` and friends.
pub async fn init_source_catalog(
    pool: &Pool,
    extensions_schema: &str,
) -> Result<CatalogInit, DdlError> {
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(pg_err)?;
    lock(&tx).await?;
    let schema: bool = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'hdb_catalog')",
            &[],
        )
        .await
        .map_err(pg_err)?
        .get(0);
    let has_version = schema && exists(&tx, "hdb_catalog.hdb_source_catalog_version").await?;
    let has_event_log = schema && exists(&tx, "hdb_catalog.event_log").await?;
    let result = match (schema, has_version, has_event_log) {
        (false, _, _) | (true, false, false) => {
            if !schema {
                tx.batch_execute("CREATE SCHEMA hdb_catalog")
                    .await
                    .map_err(pg_err)?;
            }
            ensure_pgcrypto(&tx, extensions_schema).await?;
            tx.batch_execute(ddl::INIT_PG_SOURCE)
                .await
                .map_err(pg_err)?;
            set_version(&tx).await?;
            CatalogInit::Created
        }
        (true, false, true) => {
            return Err(
                "hdb_catalog.event_log predates Hasura v2.0 (no hdb_source_catalog_version); \
                 upgrade it with Hasura v2 first"
                    .to_owned(),
            );
        }
        (true, true, _) => {
            let text: String = tx
                .query_one(
                    "SELECT version FROM hdb_catalog.hdb_source_catalog_version",
                    &[],
                )
                .await
                .map_err(pg_err)?
                .get(0);
            let version: i32 = text
                .trim()
                .parse()
                .map_err(|_| format!("invalid source catalog version \"{text}\""))?;
            if version > SOURCE_CATALOG_VERSION {
                return Err(format!(
                    "Expected source catalog version <= {SOURCE_CATALOG_VERSION}, but the current version is {version}"
                ));
            }
            if version == SOURCE_CATALOG_VERSION {
                CatalogInit::UpToDate
            } else {
                for sql in &ddl::SOURCE_MIGRATIONS[version.max(0) as usize..] {
                    tx.batch_execute(sql).await.map_err(pg_err)?;
                }
                set_version(&tx).await?;
                CatalogInit::Migrated
            }
        }
    };
    tx.commit().await.map_err(pg_err)?;
    if result != CatalogInit::UpToDate {
        info!(target: "startup", ?result, "event trigger source catalog prepared");
    }
    Ok(result)
}

/// Everything needed to (re)create one trigger's SQL objects.
#[derive(Debug, Clone)]
pub struct TriggerDdl {
    pub name: String,
    pub table: QualifiedTable,
    pub definition: TriggerOpsDef,
    pub columns: Vec<TriggerColumn>,
    pub stringify_numeric: bool,
    pub trigger_on_replication: bool,
}

/// What [`sync_triggers`] changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub functions_written: usize,
    pub triggers_created: usize,
    pub functions_dropped: usize,
    /// SQL triggers dropped because their event trigger moved to another table.
    pub triggers_dropped: usize,
    /// Event triggers no longer in metadata whose objects were dropped and events archived.
    pub removed: Vec<String>,
    /// Triggers whose DDL failed, with the database's message.
    pub errors: Vec<(String, DdlError)>,
}

/// Installed `hdb_catalog.notify_hasura_*` functions, by event trigger name.
const INSTALLED_FUNCTIONS_SQL: &str = "SELECT p.proname::text FROM pg_catalog.pg_proc p \
     JOIN pg_catalog.pg_namespace n ON p.pronamespace = n.oid \
     WHERE n.nspname = 'hdb_catalog' AND p.proname LIKE 'notify\\_hasura\\_%'";

/// SQL triggers calling an `hdb_catalog.notify_hasura_*` function: (name, schema, table).
const INSTALLED_TRIGGERS_SQL: &str = "SELECT t.tgname::text, tn.nspname::text, c.relname::text \
     FROM pg_catalog.pg_trigger t \
     JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
     JOIN pg_catalog.pg_namespace tn ON tn.oid = c.relnamespace \
     JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
     JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace \
     WHERE NOT t.tgisinternal AND pn.nspname = 'hdb_catalog' \
     AND t.tgname LIKE 'notify\\_hasura\\_%'";

/// Identifies the database a pool connects to, so sources that share one can share the set
/// of triggers [`sync_triggers`] keeps: the cluster's system identifier and the database oid,
/// which do not depend on how the source connects (socket or TCP, host name or address).
const DATABASE_ID_SQL: &str = "SELECT (SELECT system_identifier FROM pg_control_system())::text \
     || '/' || (SELECT oid FROM pg_database WHERE datname = current_database())::text";

/// For roles that may not call `pg_control_system()`: the server address and database name.
/// The same database reached over different addresses does not match.
const DATABASE_ADDR_SQL: &str = "SELECT coalesce(host(inet_server_addr()), '') || ':' || \
     coalesce(inet_server_port()::text, '') || '/' || current_database()::text";

/// The database `pool` connects to, as a comparable key. Two sources with the same key manage
/// SQL triggers in the same `hdb_catalog`.
pub async fn database_id(pool: &Pool) -> Result<String, DdlError> {
    let client = pool.get().await.map_err(|e| e.to_string())?;
    let row = match client.query_one(DATABASE_ID_SQL, &[]).await {
        Ok(row) => row,
        Err(_) => client
            .query_one(DATABASE_ADDR_SQL, &[])
            .await
            .map_err(pg_err)?,
    };
    Ok(row.get(0))
}

/// The event trigger a `notify_hasura_<name>_<OP>` object belongs to.
fn trigger_of(object: &str) -> Option<&str> {
    let rest = object.strip_prefix("notify_hasura_")?;
    Op::ALL
        .iter()
        .find_map(|op| rest.strip_suffix(op.as_str())?.strip_suffix('_'))
}

/// Make the installed SQL match the definitions: write functions whose body differs, create
/// missing SQL triggers, and drop functions of ops a trigger no longer listens to. One
/// trigger's failure does not affect the others.
///
/// `keep` names every event trigger of the source in metadata, including inconsistent ones
/// (their objects are left alone, as V2 does). Objects of any other trigger are dropped and
/// its pending events archived, and SQL triggers left on a table a trigger moved away from
/// are dropped, so a metadata change applied by a restart has the effect it has in V2.
pub async fn sync_triggers(
    pool: &Pool,
    triggers: &[TriggerDdl],
    keep: &HashSet<String>,
) -> Result<SyncReport, DdlError> {
    let mut report = SyncReport::default();
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    if triggers.is_empty() {
        // Nothing to install: only take the lock when there is something to clean up.
        let installed = client
            .query(INSTALLED_FUNCTIONS_SQL, &[])
            .await
            .map_err(pg_err)?;
        if !installed
            .iter()
            .any(|r| trigger_of(r.get(0)).is_none_or(|t| !keep.contains(t)))
        {
            return Ok(report);
        }
    }
    let tx = client.transaction().await.map_err(pg_err)?;
    lock(&tx).await?;
    prune(&tx, triggers, keep, &mut report).await?;
    for t in triggers {
        tx.batch_execute("SAVEPOINT hge_trigger")
            .await
            .map_err(pg_err)?;
        let mut local = SyncReport::default();
        match sync_one(&tx, t, &mut local).await {
            Ok(()) => {
                tx.batch_execute("RELEASE SAVEPOINT hge_trigger")
                    .await
                    .map_err(pg_err)?;
                report.functions_written += local.functions_written;
                report.triggers_created += local.triggers_created;
                report.functions_dropped += local.functions_dropped;
            }
            Err(e) => {
                tx.batch_execute("ROLLBACK TO SAVEPOINT hge_trigger")
                    .await
                    .map_err(pg_err)?;
                report.errors.push((t.name.clone(), e));
            }
        }
    }
    tx.commit().await.map_err(pg_err)?;
    Ok(report)
}

async fn prune(
    tx: &Transaction<'_>,
    triggers: &[TriggerDdl],
    keep: &HashSet<String>,
    report: &mut SyncReport,
) -> Result<(), DdlError> {
    let tables: HashMap<&str, &QualifiedTable> = triggers
        .iter()
        .map(|t| (t.name.as_str(), &t.table))
        .collect();
    for row in tx
        .query(INSTALLED_TRIGGERS_SQL, &[])
        .await
        .map_err(pg_err)?
    {
        let (tgname, schema, table): (String, String, String) =
            (row.get(0), row.get(1), row.get(2));
        let Some(desired) = trigger_of(&tgname).and_then(|t| tables.get(t)) else {
            continue;
        };
        if desired.schema != schema || desired.name != table {
            tx.batch_execute(&format!(
                "DROP TRIGGER IF EXISTS {} ON {}.{}",
                ddl::quote_ident(&tgname),
                ddl::quote_ident(&schema),
                ddl::quote_ident(&table)
            ))
            .await
            .map_err(pg_err)?;
            report.triggers_dropped += 1;
        }
    }
    let mut removed: Vec<String> = vec![];
    for row in tx
        .query(INSTALLED_FUNCTIONS_SQL, &[])
        .await
        .map_err(pg_err)?
    {
        let function: String = row.get(0);
        let Some(name) = trigger_of(&function) else {
            continue;
        };
        if keep.contains(name) || tables.contains_key(name) {
            continue;
        }
        tx.batch_execute(&format!(
            "DROP FUNCTION IF EXISTS hdb_catalog.{}() CASCADE",
            ddl::quote_ident(&function)
        ))
        .await
        .map_err(pg_err)?;
        report.functions_dropped += 1;
        if !removed.iter().any(|r| r == name) {
            removed.push(name.to_owned());
        }
    }
    if !removed.is_empty() && exists(tx, "hdb_catalog.event_log").await? {
        tx.execute(
            "UPDATE hdb_catalog.event_log SET archived = 't' \
             WHERE trigger_name = ANY($1)",
            &[&removed],
        )
        .await
        .map_err(pg_err)?;
    }
    report.removed = removed;
    Ok(())
}

async fn installed_body(tx: &Transaction<'_>, function: &str) -> Result<Option<String>, DdlError> {
    let row = tx
        .query_opt(
            "SELECT p.prosrc FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON p.pronamespace = n.oid \
             WHERE n.nspname = 'hdb_catalog' AND p.proname = $1",
            &[&function],
        )
        .await
        .map_err(pg_err)?;
    Ok(row.map(|r| r.get(0)))
}

async fn sync_one(
    tx: &Transaction<'_>,
    t: &TriggerDdl,
    report: &mut SyncReport,
) -> Result<(), DdlError> {
    for op in Op::ALL {
        let function = ddl::trigger_object_name(&t.name, op);
        let Some(spec) = t.definition.op(op) else {
            if installed_body(tx, &function).await?.is_some() {
                tx.batch_execute(&ddl::drop_trigger_function_sql(&t.name, op))
                    .await
                    .map_err(pg_err)?;
                report.functions_dropped += 1;
            }
            continue;
        };
        let body = ddl::trigger_function_body(
            &t.name,
            &t.table,
            op,
            spec,
            &t.columns,
            t.stringify_numeric,
        );
        if installed_body(tx, &function).await?.as_deref() != Some(body.as_str()) {
            tx.batch_execute(&ddl::create_trigger_function_sql(&t.name, op, &body))
                .await
                .map_err(pg_err)?;
            report.functions_written += 1;
        }
        let present: bool = tx
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_trigger WHERE NOT tgisinternal AND tgname = $1 \
                 AND tgrelid = (quote_ident($2) || '.' || quote_ident($3))::regclass)",
                &[&function, &t.table.schema, &t.table.name],
            )
            .await
            .map_err(pg_err)?
            .get(0);
        if !present {
            tx.batch_execute(&ddl::create_trigger_sql(&t.name, op, &t.table))
                .await
                .map_err(pg_err)?;
            if t.trigger_on_replication {
                tx.batch_execute(&ddl::enable_always_sql(&t.name, op, &t.table))
                    .await
                    .map_err(pg_err)?;
            }
            report.triggers_created += 1;
        }
    }
    Ok(())
}

/// Drop a trigger's SQL objects (`pg_delete_event_trigger`, untrack, `replace_metadata`) and,
/// when `archive` is set, archive its pending events.
pub async fn drop_trigger(pool: &Pool, name: &str, archive: bool) -> Result<(), DdlError> {
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(pg_err)?;
    lock(&tx).await?;
    for op in Op::ALL {
        tx.batch_execute(&ddl::drop_trigger_function_sql(name, op))
            .await
            .map_err(pg_err)?;
    }
    if archive && exists(&tx, "hdb_catalog.event_log").await? {
        tx.execute(
            "UPDATE hdb_catalog.event_log SET archived = 't' WHERE trigger_name = $1",
            &[&name],
        )
        .await
        .map_err(pg_err)?;
    }
    tx.commit().await.map_err(pg_err)?;
    Ok(())
}

/// The source tables V2 drops from a database that also holds metadata
/// (`src-rsr/drop_pg_source.sql`).
const DROP_PG_SOURCE_SQL: &str = "\
DROP TABLE IF EXISTS hdb_catalog.hdb_source_catalog_version;
DROP FUNCTION IF EXISTS hdb_catalog.insert_event_log(text, text, text, text, json);
DROP TABLE IF EXISTS hdb_catalog.event_invocation_logs;
DROP TABLE IF EXISTS hdb_catalog.event_log;
DROP TABLE IF EXISTS hdb_catalog.hdb_event_log_cleanups;";

/// After a source is dropped from metadata, clean the engine's traces out of its database
/// (V2's `postDropSourceHook`). A database that also holds metadata
/// (`hdb_catalog.hdb_metadata`) keeps its `hdb_catalog` schema: only the SQL of `triggers`
/// and the event tables go. Any other database loses the whole `hdb_catalog` schema, and
/// with it every `notify_hasura_*` function and the SQL triggers calling them.
///
/// When another source still uses the same database, its event log and triggers go too:
/// V2 does exactly this, and the parity is deliberate.
pub async fn drop_source_catalog(pool: &Pool, triggers: &[String]) -> Result<(), DdlError> {
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(pg_err)?;
    lock(&tx).await?;
    if exists(&tx, "hdb_catalog.hdb_metadata").await? {
        for name in triggers {
            for op in Op::ALL {
                tx.batch_execute(&ddl::drop_trigger_function_sql(name, op))
                    .await
                    .map_err(pg_err)?;
            }
        }
        tx.batch_execute(DROP_PG_SOURCE_SQL).await.map_err(pg_err)?;
    } else {
        tx.batch_execute("DROP SCHEMA IF EXISTS hdb_catalog CASCADE")
            .await
            .map_err(pg_err)?;
    }
    tx.commit().await.map_err(pg_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::trigger_of;

    #[test]
    fn object_names_map_back_to_triggers() {
        assert_eq!(trigger_of("notify_hasura_items_INSERT"), Some("items"));
        assert_eq!(
            trigger_of("notify_hasura_a_INSERT_UPDATE"),
            Some("a_INSERT")
        );
        assert_eq!(trigger_of("notify_hasura_x-y_DELETE"), Some("x-y"));
        assert_eq!(trigger_of("notify_hasura_items_MANUAL"), None);
        assert_eq!(trigger_of("other_items_INSERT"), None);
    }
}
