//! Postgres helpers: one throwaway database per worker slot.

use anyhow::{Context, Result};
use tokio_postgres::{Client, NoTls};

pub async fn connect(url: &str) -> Result<Client> {
    let (client, conn) = tokio_postgres::connect(url, NoTls)
        .await
        .with_context(|| format!("connecting to {}", redact(url)))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(client)
}

/// `url` with its path (database name) replaced.
pub fn with_database(url: &str, db: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    // scheme://authority/path
    let after_scheme = base.find("://").map_or(0, |i| i + 3);
    let path_start = base[after_scheme..]
        .find('/')
        .map_or(base.len(), |i| after_scheme + i);
    let mut out = format!("{}/{db}", &base[..path_start]);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(s), Some(at)) if at > s => format!("{}://***{}", &url[..s], &url[at..]),
        _ => url.to_owned(),
    }
}

/// Drop and create `db` (terminating anything still connected to it).
pub async fn recreate(admin: &Client, db: &str) -> Result<()> {
    // Separate statements: a multi-statement batch is one transaction, which DROP DATABASE refuses.
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .await
        .with_context(|| format!("dropping database {db}"))?;
    admin
        .batch_execute(&format!("CREATE DATABASE \"{db}\""))
        .await
        .with_context(|| format!("creating database {db}"))
}

/// Run a SQL script on a fresh connection (scripts may change session settings).
pub async fn run_script(url: &str, sql: &str) -> Result<()> {
    let client = connect(url).await?;
    client.batch_execute(sql).await.map_err(|e| {
        let detail = e
            .as_db_error()
            .map(|d| d.message().to_owned())
            .unwrap_or_else(|| e.to_string());
        anyhow::anyhow!("{detail}")
    })
}

/// Empty every user table, so `data.sql` can be re-applied.
pub const TRUNCATE_ALL: &str = r#"
DO $$ DECLARE r record; BEGIN
  FOR r IN SELECT format('%I.%I', n.nspname, c.relname) AS t FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE c.relkind IN ('r', 'p') AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'hdb_catalog')
             AND n.nspname NOT LIKE 'pg_toast%'
             AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = c.oid AND d.deptype = 'e')
  LOOP EXECUTE 'TRUNCATE ' || r.t || ' RESTART IDENTITY CASCADE'; END LOOP; END $$;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_database() {
        assert_eq!(
            with_database("postgres://u:p@h:5432/postgres", "x"),
            "postgres://u:p@h:5432/x"
        );
        assert_eq!(
            with_database("postgres://u:p@h:5432/postgres?sslmode=disable", "x"),
            "postgres://u:p@h:5432/x?sslmode=disable"
        );
        assert_eq!(with_database("postgres://h", "x"), "postgres://h/x");
    }

    #[test]
    fn redacts_password() {
        assert_eq!(redact("postgres://u:p@h/db"), "postgres://***@h/db");
    }
}
