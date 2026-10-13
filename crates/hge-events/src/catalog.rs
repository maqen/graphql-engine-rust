//! Source catalog set-up and the per-trigger SQL objects (DDL against a source database).
//!
//! All DDL runs under a transaction-scoped advisory lock, so several engine replicas booting
//! at once serialise instead of failing with "tuple concurrently updated". Trigger functions
//! are only replaced when their body differs from what is installed, so a restart issues no
//! DDL at all when nothing changed.

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
    /// Triggers whose DDL failed, with the database's message.
    pub errors: Vec<(String, DdlError)>,
}

/// Make the installed SQL match the definitions: write functions whose body differs, create
/// missing SQL triggers, and drop functions of ops a trigger no longer listens to. One
/// trigger's failure does not affect the others.
pub async fn sync_triggers(pool: &Pool, triggers: &[TriggerDdl]) -> Result<SyncReport, DdlError> {
    let mut report = SyncReport::default();
    if triggers.is_empty() {
        return Ok(report);
    }
    let mut client = pool.get().await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(pg_err)?;
    lock(&tx).await?;
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
