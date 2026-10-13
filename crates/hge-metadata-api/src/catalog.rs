//! V2's metadata catalog (`hdb_catalog`, catalog version 48) in Postgres.
//!
//! [`PgCatalog::init`] creates or upgrades the catalog exactly like V2 (`Migrate.hs`), so a
//! database created by V2 boots here with its metadata, and a database created here can be
//! handed back to V2. Metadata writes use V2's optimistic `resource_version` check, and every
//! write is announced in `hdb_schema_notifications` so other instances (ours or V2's) reload.

use deadpool_postgres::GenericClient;
use deadpool_postgres::{Object, Pool};
use hge_core::{Code, Error};
use hge_metadata::store::{
    BoxFuture, CatalogState, CatalogStateKind, Invalidations, MetadataStore, StoredMetadata,
    conflict,
};
use hge_postgres::source::pg_error;
use hge_postgres::{PoolConfig, connect_pool};
use serde_json::Value;
use tracing::info;

/// The catalog version this engine writes (V2 v2.51.0).
pub const CATALOG_VERSION: u32 = 48;

/// Oldest catalog version we upgrade from (the metadata-separation release).
const OLDEST_SUPPORTED: u32 = 43;

/// `pg_advisory_xact_lock` key serialising concurrent catalog initialisation.
const INIT_LOCK_KEY: i64 = 0x0068_6765_5f63_6174; // "hge_cat"

/// `server/src-rsr/initialise.sql` (catalog version 48), made idempotent.
const INITIALISE_SQL: &str = r#"
CREATE OR REPLACE FUNCTION hdb_catalog.gen_hasura_uuid() RETURNS uuid AS
  'select gen_random_uuid()' LANGUAGE SQL;

CREATE TABLE IF NOT EXISTS hdb_catalog.hdb_version (
  hasura_uuid UUID PRIMARY KEY DEFAULT hdb_catalog.gen_hasura_uuid(),
  version TEXT NOT NULL,
  upgraded_on TIMESTAMPTZ NOT NULL,
  cli_state JSONB NOT NULL DEFAULT '{}'::jsonb,
  console_state JSONB NOT NULL DEFAULT '{}'::jsonb,
  ee_client_id TEXT,
  ee_client_secret TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS hdb_version_one_row
  ON hdb_catalog.hdb_version((version IS NOT NULL));

CREATE TABLE IF NOT EXISTS hdb_catalog.hdb_metadata (
  id INTEGER PRIMARY KEY,
  metadata JSON NOT NULL,
  resource_version INTEGER NOT NULL DEFAULT 1 UNIQUE
);

CREATE TABLE IF NOT EXISTS hdb_catalog.hdb_action_log (
  id UUID PRIMARY KEY DEFAULT hdb_catalog.gen_hasura_uuid(),
  action_name TEXT,
  input_payload JSONB NOT NULL,
  request_headers JSONB NOT NULL,
  session_variables JSONB NOT NULL,
  response_payload JSONB NULL,
  errors JSONB NULL,
  created_at timestamptz NOT NULL default now(),
  response_received_at timestamptz NULL,
  status text NOT NULL,
  CHECK (status IN ('created', 'processing', 'completed', 'error'))
);

CREATE TABLE IF NOT EXISTS hdb_catalog.hdb_cron_events (
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  trigger_name TEXT NOT NULL,
  scheduled_time TIMESTAMPTZ NOT NULL,
  status TEXT NOT NULL DEFAULT 'scheduled',
  tries INTEGER NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  next_retry_at TIMESTAMPTZ,
  CONSTRAINT valid_status CHECK (status IN ('scheduled','locked','delivered','error','dead'))
);
CREATE INDEX IF NOT EXISTS hdb_cron_event_status ON hdb_catalog.hdb_cron_events (status);
CREATE UNIQUE INDEX IF NOT EXISTS hdb_cron_events_unique_scheduled ON
  hdb_catalog.hdb_cron_events (trigger_name, scheduled_time) WHERE status = 'scheduled';

CREATE TABLE IF NOT EXISTS hdb_catalog.hdb_cron_event_invocation_logs (
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  event_id TEXT,
  status INTEGER,
  request JSON,
  response JSON,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  FOREIGN KEY (event_id) REFERENCES hdb_catalog.hdb_cron_events (id)
    ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS hdb_cron_event_invocation_event_id
  ON hdb_catalog.hdb_cron_event_invocation_logs (event_id);

CREATE TABLE IF NOT EXISTS hdb_catalog.hdb_scheduled_events (
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  webhook_conf JSON NOT NULL,
  scheduled_time TIMESTAMPTZ NOT NULL,
  retry_conf JSON,
  payload JSON,
  header_conf JSON,
  status TEXT NOT NULL DEFAULT 'scheduled',
  tries INTEGER NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  next_retry_at TIMESTAMPTZ,
  comment TEXT,
  CONSTRAINT valid_status CHECK (status IN ('scheduled','locked','delivered','error','dead'))
);
CREATE INDEX IF NOT EXISTS hdb_scheduled_event_status
  ON hdb_catalog.hdb_scheduled_events (status);

CREATE TABLE IF NOT EXISTS hdb_catalog.hdb_scheduled_event_invocation_logs (
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  event_id TEXT,
  status INTEGER,
  request JSON,
  response JSON,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  FOREIGN KEY (event_id) REFERENCES hdb_catalog.hdb_scheduled_events (id)
    ON DELETE CASCADE ON UPDATE CASCADE
);

CREATE TABLE IF NOT EXISTS hdb_catalog.hdb_schema_notifications (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  notification JSON NOT NULL,
  resource_version INTEGER NOT NULL DEFAULT 1,
  instance_id UUID NOT NULL,
  updated_at TIMESTAMPTZ DEFAULT NOW()
);
"#;

/// Upgrade steps `n -> n+1` (V2's `server/src-rsr/migrations/<n>_to_<n+1>.sql`).
const UPGRADES: &[(u32, &str)] = &[
    (
        43,
        "ALTER TABLE hdb_catalog.hdb_metadata \
           ADD COLUMN IF NOT EXISTS resource_version INTEGER NOT NULL DEFAULT 1 UNIQUE;",
    ),
    (
        44,
        "CREATE TABLE IF NOT EXISTS hdb_catalog.hdb_schema_notifications (
           id INTEGER PRIMARY KEY CHECK (id = 1),
           notification JSON NOT NULL,
           resource_version INTEGER NOT NULL DEFAULT 1,
           instance_id UUID NOT NULL,
           updated_at TIMESTAMPTZ DEFAULT NOW());",
    ),
    (
        45,
        "CREATE UNIQUE INDEX IF NOT EXISTS hdb_cron_events_unique_scheduled ON \
           hdb_catalog.hdb_cron_events (trigger_name, scheduled_time) WHERE status = 'scheduled';",
    ),
    (
        46,
        "CREATE INDEX IF NOT EXISTS hdb_cron_event_invocation_event_id \
           ON hdb_catalog.hdb_cron_event_invocation_logs (event_id);",
    ),
    (
        47,
        "ALTER TABLE hdb_catalog.hdb_version \
           ADD COLUMN IF NOT EXISTS ee_client_id TEXT, \
           ADD COLUMN IF NOT EXISTS ee_client_secret TEXT;",
    ),
];

/// The metadata catalog of one database.
pub struct PgCatalog {
    pool: Pool,
    /// `HASURA_GRAPHQL_METADATA_DATABASE_EXTENSIONS_SCHEMA`: where `pgcrypto` is created.
    extensions_schema: String,
}

/// What [`PgCatalog::init`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogInit {
    /// The catalog was created.
    Created,
    /// The catalog was upgraded from an older version.
    Upgraded { from: u32 },
    /// The catalog was already current.
    Current,
}

impl PgCatalog {
    /// Connect to the metadata database (connections open lazily).
    pub fn connect(url: &str) -> Result<Self, Error> {
        let (pool, _) = connect_pool(
            url,
            &PoolConfig {
                max_connections: 4,
                use_prepared_statements: false,
                ..PoolConfig::default()
            },
        )?;
        Ok(PgCatalog {
            pool,
            extensions_schema: "public".to_owned(),
        })
    }

    /// Create `pgcrypto` in this schema instead of `public`.
    pub fn with_extensions_schema(mut self, schema: &str) -> Self {
        self.extensions_schema = schema.to_owned();
        self
    }

    /// The metadata database (also home of cron and one-off scheduled events).
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    async fn client(&self) -> Result<Object, Error> {
        self.pool.get().await.map_err(|e| {
            Error::new(
                Code::PostgresError,
                format!("connection error on the metadata database: {e}"),
            )
        })
    }

    /// Create the catalog, or upgrade it to [`CATALOG_VERSION`]. Concurrent callers (several
    /// replicas booting at once) are serialised by an advisory lock in one transaction.
    pub async fn init(&self) -> Result<CatalogInit, Error> {
        let mut client = self.client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| pg_error(e, "BEGIN"))?;
        exec(
            &tx,
            &format!("SELECT pg_advisory_xact_lock({INIT_LOCK_KEY})"),
        )
        .await?;
        let row = tx
            .query_one(
                "SELECT to_regnamespace('hdb_catalog') IS NOT NULL, \
                        to_regclass('hdb_catalog.hdb_version') IS NOT NULL",
                &[],
            )
            .await
            .map_err(|e| pg_error(e, "catalog check"))?;
        let (has_schema, has_version): (bool, bool) = (row.get(0), row.get(1));
        let outcome = if !has_version {
            if !has_schema {
                exec(&tx, "CREATE SCHEMA hdb_catalog").await?;
            }
            ensure_gen_random_uuid(&tx, &self.extensions_schema).await?;
            tx.batch_execute(INITIALISE_SQL)
                .await
                .map_err(|e| pg_error(e, INITIALISE_SQL))?;
            set_version(&tx).await?;
            CatalogInit::Created
        } else {
            let text: Option<String> = tx
                .query_opt("SELECT version FROM hdb_catalog.hdb_version", &[])
                .await
                .map_err(|e| pg_error(e, "SELECT version FROM hdb_catalog.hdb_version"))?
                .map(|r| r.get(0));
            let version = text
                .as_deref()
                .and_then(|t| t.trim().parse::<f64>().ok())
                .map(|v| v as u32);
            match version {
                Some(v) if v == CATALOG_VERSION => CatalogInit::Current,
                Some(v) if v > CATALOG_VERSION => {
                    return Err(Error::new(
                        Code::NotSupported,
                        format!(
                            "Cannot use database previously used with a newer version of \
                             graphql-engine (expected a catalog version <={CATALOG_VERSION}, \
                             but the current version is {v})."
                        ),
                    ));
                }
                Some(v) if v >= OLDEST_SUPPORTED => {
                    for (from, sql) in UPGRADES.iter().filter(|(from, _)| *from >= v) {
                        info!(target: "startup", from, "upgrading the metadata catalog");
                        tx.batch_execute(sql).await.map_err(|e| pg_error(e, sql))?;
                    }
                    set_version(&tx).await?;
                    CatalogInit::Upgraded { from: v }
                }
                _ => {
                    return Err(Error::new(
                        Code::NotSupported,
                        format!(
                            "the metadata catalog version {} is too old; upgrade the database \
                             with Hasura GraphQL Engine v2 first (catalog version >= \
                             {OLDEST_SUPPORTED})",
                            text.unwrap_or_default()
                        ),
                    ));
                }
            }
        };
        tx.commit().await.map_err(|e| pg_error(e, "COMMIT"))?;
        Ok(outcome)
    }
}

async fn exec(client: &impl GenericClient, sql: &str) -> Result<(), Error> {
    client
        .batch_execute(sql)
        .await
        .map_err(|e| pg_error(e, sql))
}

/// V2 installs pgcrypto for `gen_random_uuid()`. Postgres 13+ has it built in, so a failure
/// to create the extension there (e.g. no permission) is not fatal.
async fn ensure_gen_random_uuid(
    tx: &impl GenericClient,
    extensions_schema: &str,
) -> Result<(), Error> {
    let row = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pgcrypto'), \
                    EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pgcrypto'), \
                    current_setting('server_version_num')::int >= 130000",
            &[],
        )
        .await
        .map_err(|e| pg_error(e, "pgcrypto check"))?;
    let (installed, available, builtin): (bool, bool, bool) = (row.get(0), row.get(1), row.get(2));
    if installed || builtin {
        return Ok(());
    }
    if !available {
        return Err(Error::unexpected(
            "pgcrypto extension is required, but could not find the extension in the \
             PostgreSQL server. Please make sure this extension is available.",
        ));
    }
    let schema = format!("\"{}\"", extensions_schema.replace('"', "\"\""));
    exec(
        tx,
        &format!("CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA {schema}"),
    )
    .await
}

async fn set_version(tx: &impl GenericClient) -> Result<(), Error> {
    let sql = format!(
        "INSERT INTO hdb_catalog.hdb_version (version, upgraded_on) VALUES ('{CATALOG_VERSION}', now()) \
         ON CONFLICT ((version IS NOT NULL)) DO UPDATE SET version = '{CATALOG_VERSION}', upgraded_on = now()"
    );
    exec(tx, &sql).await
}

fn parse_json(text: &str, what: &str) -> Result<Value, Error> {
    serde_json::from_str(text).map_err(|e| Error::unexpected(format!("invalid {what}: {e}")))
}

impl MetadataStore for PgCatalog {
    fn fetch(&self) -> BoxFuture<'_, Result<Option<StoredMetadata>, Error>> {
        Box::pin(async move {
            const SQL: &str =
                "SELECT metadata::text, resource_version::int8 FROM hdb_catalog.hdb_metadata";
            let client = self.client().await?;
            let rows = client.query(SQL, &[]).await.map_err(|e| pg_error(e, SQL))?;
            match rows.as_slice() {
                [] => Ok(None),
                [row] => Ok(Some(StoredMetadata {
                    metadata: parse_json(row.get(0), "stored metadata")?,
                    resource_version: row.get(1),
                })),
                _ => Err(Error::unexpected("multiple rows in hdb_metadata table")),
            }
        })
    }

    fn store<'a>(
        &'a self,
        metadata: &'a Value,
        expected: i64,
    ) -> BoxFuture<'a, Result<i64, Error>> {
        Box::pin(async move {
            const SQL: &str = "INSERT INTO hdb_catalog.hdb_metadata (id, metadata) VALUES (1, $1::text::json) \
                 ON CONFLICT (id) DO UPDATE SET metadata = $1::text::json, \
                   resource_version = hdb_catalog.hdb_metadata.resource_version + 1 \
                 WHERE hdb_catalog.hdb_metadata.resource_version = $2::text::int8 \
                 RETURNING resource_version::int8";
            let client = self.client().await?;
            let text = metadata.to_string();
            let expected_text = expected.to_string();
            let row = client
                .query_opt(SQL, &[&text, &expected_text])
                .await
                .map_err(|e| pg_error(e, SQL))?;
            match row {
                Some(r) => Ok(r.get(0)),
                None => Err(conflict(expected)),
            }
        })
    }

    fn bump_version(&self, expected: i64) -> BoxFuture<'_, Result<i64, Error>> {
        Box::pin(async move {
            const SQL: &str = "UPDATE hdb_catalog.hdb_metadata SET resource_version = resource_version + 1 \
                 WHERE resource_version = $1::text::int8 RETURNING resource_version::int8";
            let client = self.client().await?;
            let row = client
                .query_opt(SQL, &[&expected.to_string()])
                .await
                .map_err(|e| pg_error(e, SQL))?;
            row.map(|r| r.get(0)).ok_or_else(|| conflict(expected))
        })
    }

    fn resource_version(&self) -> BoxFuture<'_, Result<i64, Error>> {
        Box::pin(async move {
            const SQL: &str = "SELECT resource_version::int8 FROM hdb_catalog.hdb_metadata";
            let client = self.client().await?;
            let row = client
                .query_opt(SQL, &[])
                .await
                .map_err(|e| pg_error(e, SQL))?;
            Ok(row.map_or(0, |r| r.get(0)))
        })
    }

    fn notify<'a>(
        &'a self,
        resource_version: i64,
        invalidations: &'a Invalidations,
        instance_id: &'a str,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            const SQL: &str = "INSERT INTO hdb_catalog.hdb_schema_notifications \
                   (id, notification, resource_version, instance_id) \
                 VALUES (1, $1::text::json, $2::text::int, $3::text::uuid) \
                 ON CONFLICT (id) DO UPDATE SET notification = $1::text::json, \
                   resource_version = $2::text::int, instance_id = $3::text::uuid, updated_at = now()";
            let client = self.client().await?;
            let notification = invalidations.to_json().to_string();
            let version = resource_version.to_string();
            client
                .execute(SQL, &[&notification, &version, &instance_id])
                .await
                .map_err(|e| pg_error(e, SQL))?;
            Ok(())
        })
    }

    fn notification_since<'a>(
        &'a self,
        after: i64,
        instance_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<(i64, Invalidations)>, Error>> {
        Box::pin(async move {
            const SQL: &str = "SELECT resource_version::int8, notification::text \
                 FROM hdb_catalog.hdb_schema_notifications \
                 WHERE resource_version > $1::text::int8 AND instance_id <> $2::text::uuid";
            let client = self.client().await?;
            let after = after.to_string();
            let row = client
                .query_opt(SQL, &[&after, &instance_id])
                .await
                .map_err(|e| pg_error(e, SQL))?;
            row.map(|r| {
                let v: i64 = r.get(0);
                let n = parse_json(r.get(1), "schema notification")?;
                Ok((v, Invalidations::from_json(&n)))
            })
            .transpose()
        })
    }

    fn catalog_state(&self) -> BoxFuture<'_, Result<CatalogState, Error>> {
        Box::pin(async move {
            const SQL: &str = "SELECT hasura_uuid::text, cli_state::text, console_state::text \
                 FROM hdb_catalog.hdb_version";
            let client = self.client().await?;
            let row = client
                .query_opt(SQL, &[])
                .await
                .map_err(|e| pg_error(e, SQL))?
                .ok_or_else(|| Error::unexpected("hdb_catalog.hdb_version is empty"))?;
            Ok(CatalogState {
                id: row.get(0),
                cli_state: parse_json(row.get(1), "cli_state")?,
                console_state: parse_json(row.get(2), "console_state")?,
            })
        })
    }

    fn set_catalog_state<'a>(
        &'a self,
        kind: CatalogStateKind,
        state: &'a Value,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let sql = match kind {
                CatalogStateKind::Cli => {
                    "UPDATE hdb_catalog.hdb_version SET cli_state = $1::text::jsonb"
                }
                CatalogStateKind::Console => {
                    "UPDATE hdb_catalog.hdb_version SET console_state = $1::text::jsonb"
                }
            };
            let client = self.client().await?;
            let text = state.to_string();
            client
                .execute(sql, &[&text])
                .await
                .map_err(|e| pg_error(e, sql))?;
            Ok(())
        })
    }
}
