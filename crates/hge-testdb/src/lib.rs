//! A throwaway Postgres database for one test run.
//!
//! Tests that need Postgres read the server from `HASURA_GRAPHQL_DATABASE_URL`, but never
//! touch the database it names: [`TestDb::create`] makes a uniquely named database on that
//! server and drops it again when the [`TestDb`] is dropped, so test binaries and parallel
//! runs (CI jobs, several worktrees) never share state.
//!
//! The URL must be in URL form (`postgres://user:pass@host:port/db?…`) and its role needs
//! `CREATEDB`. Databases left behind by killed runs (named `hge_t_*`) are swept by later runs
//! once they are older than [`STALE_AFTER_SECS`].

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_postgres::NoTls;

/// The variable that names the Postgres server tests run against.
pub const URL_VAR: &str = "HASURA_GRAPHQL_DATABASE_URL";

/// Test databases older than this are assumed abandoned and dropped by the next run.
pub const STALE_AFTER_SECS: u64 = 6 * 60 * 60;

/// How long connecting to the server may take before the test fails.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

const PREFIX: &str = "hge_t_";

/// A database created for one test; dropped (with `FORCE`) on drop.
pub struct TestDb {
    name: String,
    url: String,
    admin_url: String,
}

impl TestDb {
    /// Create a fresh database on the server `HASURA_GRAPHQL_DATABASE_URL` points at, or
    /// `None` when the variable is not set (the caller skips its test). `label` goes into
    /// the database name to make leftovers easy to attribute.
    pub async fn create(label: &str) -> Option<TestDb> {
        let admin_url = std::env::var(URL_VAR).ok()?;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let label: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .take(12)
            .collect();
        // `hge_t_<unix secs>_<label>_<pid>_<nanos>_<seq>`, at most 58 bytes.
        let name = format!(
            "{PREFIX}{}_{label}_{}_{}_{}",
            now.as_secs(),
            std::process::id(),
            now.subsec_nanos(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let url = with_database(&admin_url, &name);
        sweep_stale(&admin_url, now.as_secs()).await;
        execute(&admin_url, &format!("CREATE DATABASE \"{name}\"")).await;
        Some(TestDb {
            name,
            url,
            admin_url,
        })
    }

    /// The connection URL of the test database.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The database name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let sql = format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name);
        let admin_url = self.admin_url.clone();
        // Drop runs inside the test's runtime, which cannot be blocked on; use a thread with
        // its own runtime.
        let dropped = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(execute(&admin_url, &sql));
        })
        .join();
        if dropped.is_err() {
            eprintln!("could not drop test database {}", self.name);
        }
    }
}

/// Drop test databases created more than [`STALE_AFTER_SECS`] ago (runs that were killed
/// before they could clean up). Best effort: errors are ignored.
async fn sweep_stale(admin_url: &str, now: u64) {
    let Ok(client) = connect(admin_url).await else {
        return;
    };
    let Ok(rows) = client
        .query(
            "SELECT datname FROM pg_database WHERE datname LIKE 'hge\\_t\\_%'",
            &[],
        )
        .await
    else {
        return;
    };
    for row in rows {
        let name: String = row.get(0);
        let created = name[PREFIX.len()..]
            .split('_')
            .next()
            .and_then(|s| s.parse::<u64>().ok());
        if created.is_some_and(|t| now.saturating_sub(t) > STALE_AFTER_SECS) {
            let _ = client
                .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                .await;
        }
    }
}

async fn connect(url: &str) -> Result<tokio_postgres::Client, String> {
    let mut config: tokio_postgres::Config = url.parse().map_err(|e| format!("{e}"))?;
    config.connect_timeout(CONNECT_TIMEOUT);
    let (client, conn) = config.connect(NoTls).await.map_err(|e| format!("{e}"))?;
    tokio::spawn(conn);
    Ok(client)
}

async fn execute(url: &str, sql: &str) {
    let mut config: tokio_postgres::Config = url
        .parse()
        .unwrap_or_else(|e| panic!("{URL_VAR} is not a valid URL: {e}"));
    config.connect_timeout(CONNECT_TIMEOUT);
    let (client, conn) = config
        .connect(NoTls)
        .await
        .unwrap_or_else(|e| panic!("connect to the server in {URL_VAR}: {e}"));
    let conn = tokio::spawn(conn);
    client
        .batch_execute(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    drop(client);
    let _ = conn.await;
}

/// `url` with its database name replaced by `name` (URL form, `postgres://…/db?…`).
pub fn with_database(url: &str, name: &str) -> String {
    let (base, query) = match url.find('?') {
        Some(i) => url.split_at(i),
        None => (url, ""),
    };
    let authority_start = base.find("://").map_or(0, |i| i + 3);
    let base = match base[authority_start..].find('/') {
        Some(i) => &base[..authority_start + i],
        None => base,
    };
    format!("{base}/{name}{query}")
}

#[cfg(test)]
mod tests {
    use super::with_database;

    #[test]
    fn replaces_the_database() {
        assert_eq!(
            with_database("postgres://u:p@h:5432/hge_test", "x"),
            "postgres://u:p@h:5432/x"
        );
        assert_eq!(
            with_database("postgres://u@h/db?sslmode=disable", "x"),
            "postgres://u@h/x?sslmode=disable"
        );
        assert_eq!(
            with_database("postgres://h:5432", "x"),
            "postgres://h:5432/x"
        );
    }
}
