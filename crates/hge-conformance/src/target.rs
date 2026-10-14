//! The engines a suite can run against, and how a suite is put in place on each.
//!
//! * [`Target::Rust`]: our `graphql-engine` binary, started per suite with the suite's
//!   environment. Metadata is written to `hdb_catalog.hdb_metadata` of a fresh metadata
//!   database before start-up (until `/v1/metadata replace_metadata` lands).
//! * [`Target::Hasura`]: a `hasura/graphql-engine` container (host network) per suite
//!   environment, reused across suites; metadata is applied with `replace_metadata`.
//!
//! Each worker slot owns two databases, `hgeconf_<pid>_<slot>` (the suite's source) and
//! `hgeconf_<pid>_<slot>_md` (metadata), so slots, and concurrent runs, do not collide.
//! Sources on `{"from_env": "HASURA_GRAPHQL_DATABASE_URL"}` reach the source database
//! through that variable, as in a real deployment.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio_postgres::Client;

use crate::db;
use crate::suite::{Isolation, Suite};

pub const ADMIN_SECRET: &str = "conformance-admin-secret";

/// Every Hasura container the runner starts carries this label.
const CONTAINER_LABEL: &str = "hge-conformance=1";

/// Remove stopped containers left by earlier runs that were killed before cleaning up
/// (running ones may belong to another run, so they are left alone).
pub async fn remove_exited_containers() {
    let filter = format!("label={CONTAINER_LABEL}");
    let Ok(out) = tokio::process::Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &filter,
            "--filter",
            "status=exited",
        ])
        .args(["--filter", "status=created", "--filter", "status=dead"])
        .output()
        .await
    else {
        return;
    };
    let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if !ids.is_empty() {
        let _ = tokio::process::Command::new("docker")
            .arg("rm")
            .args(&ids)
            .output()
            .await;
    }
}

#[derive(Debug, Clone)]
pub enum Target {
    Rust { bin: PathBuf },
    Hasura { image: String },
}

enum Handle {
    Process(tokio::process::Child),
    Container(String),
}

struct Running {
    key: String,
    base_url: String,
    handle: Handle,
}

pub struct Slot {
    index: usize,
    pg_url: String,
    admin: Client,
    target: Target,
    http: reqwest::Client,
    engine: Option<Running>,
    admin_secret: Option<String>,
}

impl Slot {
    pub async fn new(index: usize, pg_url: &str, target: Target) -> Result<Self> {
        Ok(Slot {
            index,
            pg_url: pg_url.to_owned(),
            admin: db::connect(pg_url).await?,
            target,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?,
            engine: None,
            admin_secret: None,
        })
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn base_url(&self) -> &str {
        self.engine.as_ref().map_or("", |e| e.base_url.as_str())
    }

    pub fn admin_secret(&self) -> Option<&str> {
        self.admin_secret.as_deref()
    }

    fn name(&self) -> String {
        format!("{}_{}", std::process::id(), self.index)
    }

    fn src_db(&self) -> String {
        format!("hgeconf_{}", self.name())
    }

    fn md_db(&self) -> String {
        format!("hgeconf_{}_md", self.name())
    }

    fn src_url(&self) -> String {
        db::with_database(&self.pg_url, &self.src_db())
    }

    fn log_path(&self) -> PathBuf {
        std::env::temp_dir().join(format!("hge-conformance-{}.log", self.name()))
    }

    /// Put `suite` in place: fresh database, metadata, and an engine serving it.
    pub async fn setup_suite(&mut self, suite: &Suite) -> Result<()> {
        self.admin_secret = suite.config.admin_secret.then(|| ADMIN_SECRET.to_owned());
        match self.target.clone() {
            Target::Rust { bin } => {
                self.stop().await;
                self.create_source(suite).await?;
                db::recreate(&self.admin, &self.md_db()).await?;
                let md_url = db::with_database(&self.pg_url, &self.md_db());
                let catalog = db::connect(&md_url).await?;
                catalog
                    .batch_execute(
                        "CREATE SCHEMA hdb_catalog;
                         CREATE TABLE hdb_catalog.hdb_metadata (
                           id integer PRIMARY KEY, metadata json NOT NULL,
                           resource_version integer NOT NULL DEFAULT 1);",
                    )
                    .await?;
                let md = suite.metadata.to_string();
                catalog
                    .execute(
                        "INSERT INTO hdb_catalog.hdb_metadata (id, metadata) VALUES (1, $1::text::json)",
                        &[&md],
                    )
                    .await?;
                drop(catalog);
                self.start_rust(&bin, suite, &md_url).await
            }
            Target::Hasura { image } => {
                let key = suite.engine_key();
                if self.engine.as_ref().map(|e| &e.key) != Some(&key) {
                    self.stop().await;
                    self.start_hasura(&image, suite).await?;
                }
                self.metadata_api(
                    json!({"type": "replace_metadata", "args": {"version": 3, "sources": []}}),
                )
                .await?;
                self.create_source(suite).await?;
                self.metadata_api(json!({"type": "replace_metadata", "args": suite.metadata}))
                    .await
            }
        }
    }

    async fn create_source(&self, suite: &Suite) -> Result<()> {
        db::recreate(&self.admin, &self.src_db()).await?;
        db::run_script(&self.src_url(), &suite.setup_sql)
            .await
            .context("setup.sql")
    }

    /// Reset state before a case, as the suite's isolation requires.
    pub async fn before_case(&mut self, suite: &Suite, first: bool) -> Result<()> {
        match suite.config.isolation {
            Isolation::None => Ok(()),
            Isolation::Data => {
                let sql = format!(
                    "{}{}",
                    db::TRUNCATE_ALL,
                    suite.data_sql.as_deref().unwrap_or_default()
                );
                db::run_script(&self.src_url(), &sql)
                    .await
                    .context("resetting data")
            }
            Isolation::Database if first => Ok(()),
            Isolation::Database => self.setup_suite(suite).await,
        }
    }

    async fn metadata_api(&self, body: Value) -> Result<()> {
        let mut req = self
            .http
            .post(format!("{}/v1/metadata", self.base_url()))
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(s) = &self.admin_secret {
            req = req.header("x-hasura-admin-secret", s);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("{} -> {status}: {text}", body["type"]);
        }
        Ok(())
    }

    async fn start_rust(&mut self, bin: &Path, suite: &Suite, md_url: &str) -> Result<()> {
        let port = free_port()?;
        let log = std::fs::File::create(self.log_path())?;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.arg("serve")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HASURA_GRAPHQL_METADATA_DATABASE_URL", md_url)
            .env("HASURA_GRAPHQL_DATABASE_URL", self.src_url())
            .env("HASURA_GRAPHQL_SERVER_PORT", port.to_string())
            .env("HASURA_GRAPHQL_SERVER_HOST", "127.0.0.1")
            .env("HASURA_GRAPHQL_ENABLE_TELEMETRY", "false")
            .envs(&suite.config.env)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true);
        if let Some(s) = &self.admin_secret {
            cmd.env("HASURA_GRAPHQL_ADMIN_SECRET", s);
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        self.engine = Some(Running {
            key: suite.engine_key(),
            base_url: format!("http://127.0.0.1:{port}"),
            handle: Handle::Process(child),
        });
        self.wait_healthy(Duration::from_secs(30)).await
    }

    async fn start_hasura(&mut self, image: &str, suite: &Suite) -> Result<()> {
        db::recreate(&self.admin, &self.md_db()).await?;
        let port = free_port()?;
        let name = format!("hge-conformance-{}", self.name());
        let _ = tokio::process::Command::new("docker")
            .args(["rm", "-f", &name])
            .output()
            .await;
        let mut args: Vec<String> = [
            "run",
            "-d",
            "--name",
            &name,
            "--label",
            CONTAINER_LABEL,
            "--network",
            "host",
        ]
        .map(String::from)
        .to_vec();
        let mut env = vec![
            (
                "HASURA_GRAPHQL_METADATA_DATABASE_URL".to_owned(),
                db::with_database(&self.pg_url, &self.md_db()),
            ),
            ("HASURA_GRAPHQL_DATABASE_URL".to_owned(), self.src_url()),
            ("HASURA_GRAPHQL_SERVER_PORT".to_owned(), port.to_string()),
            (
                "HASURA_GRAPHQL_ENABLE_TELEMETRY".to_owned(),
                "false".to_owned(),
            ),
        ];
        if let Some(s) = &self.admin_secret {
            env.push(("HASURA_GRAPHQL_ADMIN_SECRET".to_owned(), s.clone()));
        }
        env.extend(suite.config.env.clone());
        for (k, v) in env {
            args.push("-e".to_owned());
            args.push(format!("{k}={v}"));
        }
        args.push(image.to_owned());
        let out = tokio::process::Command::new("docker")
            .args(&args)
            .output()
            .await
            .context("running docker")?;
        if !out.status.success() {
            // The container may exist even though it did not start.
            let _ = tokio::process::Command::new("docker")
                .args(["rm", "-f", &name])
                .output()
                .await;
            bail!(
                "docker run failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        self.engine = Some(Running {
            key: suite.engine_key(),
            base_url: format!("http://127.0.0.1:{port}"),
            handle: Handle::Container(name),
        });
        self.wait_healthy(Duration::from_secs(120)).await
    }

    async fn wait_healthy(&mut self, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        let url = format!("{}/healthz", self.base_url());
        loop {
            if let Ok(r) = self.http.get(&url).send().await
                && r.status().is_success()
            {
                return Ok(());
            }
            if let Some(Running {
                handle: Handle::Process(child),
                ..
            }) = &mut self.engine
                && let Some(status) = child.try_wait()?
            {
                let log = self.engine_log().await;
                self.engine = None;
                return Err(anyhow!("engine exited ({status}) during start-up:\n{log}"));
            }
            if let Some(Running {
                handle: Handle::Container(name),
                ..
            }) = &self.engine
                && !container_running(name).await
            {
                // Not started with --rm, so the log of a container that died is still there.
                let log = self.engine_log().await;
                self.stop().await;
                bail!("container exited during start-up:\n{log}");
            }
            if started.elapsed() > timeout {
                let log = self.engine_log().await;
                self.stop().await;
                bail!("engine not healthy after {timeout:?}:\n{log}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The tail of the engine's output, for start-up failures.
    pub async fn engine_log(&self) -> String {
        let text = match &self.engine {
            Some(Running {
                handle: Handle::Container(name),
                ..
            }) => tokio::process::Command::new("docker")
                .args(["logs", name])
                .output()
                .await
                .map(|o| {
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )
                })
                .unwrap_or_default(),
            _ => std::fs::read_to_string(self.log_path()).unwrap_or_default(),
        };
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(20)..].join("\n")
    }

    /// Stop the engine and drop this slot's databases.
    pub async fn finish(&mut self) {
        self.stop().await;
        for db in [self.src_db(), self.md_db()] {
            let _ = self
                .admin
                .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
                .await;
        }
    }

    pub async fn stop(&mut self) {
        match self.engine.take().map(|e| e.handle) {
            Some(Handle::Process(mut child)) => {
                let _ = child.kill().await;
            }
            Some(Handle::Container(name)) => {
                let _ = tokio::process::Command::new("docker")
                    .args(["rm", "-f", &name])
                    .output()
                    .await;
            }
            None => {}
        }
    }
}

async fn container_running(name: &str) -> bool {
    tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .output()
        .await
        .is_ok_and(|o| o.status.success() && o.stdout.starts_with(b"true"))
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}
