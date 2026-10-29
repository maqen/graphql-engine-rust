//! The execution context shared by commands (`cli.ExecutionContext`): global flags, the
//! logger, the project directory, configuration and the server client.

use std::path::{Path, PathBuf};

use crate::client::Client;
use crate::config::{self, Config, ServerFlags};
use crate::error::{Context as _, Error, Result};
use crate::logger::{Level, Logger};

/// The version this CLI reports. It matches the engine's `/v1/version` so tools that
/// compare the two (and the console) keep working.
pub const CLI_VERSION: &str = "v2.51.0-ce";

#[derive(Clone, Debug, Default)]
pub struct GlobalFlags {
    pub log_level: String,
    pub project: Option<String>,
    pub no_color: bool,
    pub envfile: String,
    pub skip_update_check: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataMode {
    Directory,
    Json,
    Yaml,
}

pub struct Ctx {
    pub flags: GlobalFlags,
    pub logger: Logger,
    pub dir: PathBuf,
    pub config: Config,
    pub client: Client,
    pub migrations_dir: PathBuf,
    pub seeds_dir: PathBuf,
    pub metadata_dir: PathBuf,
    pub metadata_file: Option<PathBuf>,
    pub metadata_mode: MetadataMode,
    pub server_version: String,
}

/// The logger as `ec.Prepare` sets it up. An unknown `--log-level` is logged and ignored.
pub fn make_logger(flags: &GlobalFlags) -> Logger {
    match Level::parse(&flags.log_level) {
        Some(level) => Logger::new(level, flags.no_color),
        None => {
            let l = Logger::new(Level::Info, flags.no_color);
            l.log(
                Level::Error,
                "error parsing log-level flag",
                &[(
                    "error",
                    &format!(
                        "not a valid logrus Level: {}",
                        crate::gofmt::quote(&flags.log_level)
                    ),
                )],
            );
            l
        }
    }
}

/// Find the project, load `.env` and `config.yaml` (the first half of `ec.Validate`).
pub fn load_project(
    flags: &GlobalFlags,
    logger: &Logger,
    server: &ServerFlags,
) -> Result<(PathBuf, Config)> {
    let dir = config::find_project_dir(flags.project.as_deref())?;
    load_envfile(&dir, &flags.envfile, logger)?;
    let config = config::read_config(&dir, server).map_err(|e| {
        if e.0.starts_with("cannot read config") {
            e
        } else {
            Error(format!("cannot read config: {e}"))
        }
    })?;
    Ok((dir, config))
}

fn load_envfile(dir: &Path, envfile: &str, logger: &Logger) -> Result<()> {
    let name = envfile.trim();
    if name.is_empty() {
        logger.debug("env file path is empty. Skip loading");
        return Ok(());
    }
    let path = if Path::new(name).is_absolute() {
        PathBuf::from(name)
    } else {
        dir.join(name)
    };
    match config::load_envfile(&path) {
        Ok(()) => {
            logger.debug(&format!("ENV vars read from: {}", path.display()));
            Ok(())
        }
        Err(e) if envfile != ".env" => Err(Error(format!(
            "loading .env file failed: open {}: {}",
            path.display(),
            io_message(&e)
        ))),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            logger.warn(&format!("open {}: {}", path.display(), io_message(&e)));
            Ok(())
        }
        Err(_) => Ok(()),
    }
}

/// An I/O error message in Go's wording.
pub fn io_message(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        std::io::ErrorKind::AlreadyExists => "file exists".into(),
        _ => {
            let s = e.to_string();
            match s.find(" (os error") {
                Some(i) => s[..i].to_lowercase(),
                None => s,
            }
        }
    }
}

impl Ctx {
    /// `ec.Prepare()` + `ec.Validate()`: everything a command talking to the server needs.
    pub fn validate(flags: &GlobalFlags, logger: Logger, server: &ServerFlags) -> Result<Ctx> {
        let (dir, config) = load_project(flags, &logger, server).map_err(|e| {
            if e.0.starts_with("validating current directory")
                || e.0.starts_with("loading .env")
                || e.0.starts_with("cannot read config")
            {
                e
            } else {
                Error(format!("cannot read config: {e}"))
            }
        })?;
        let mut client = Client::new(&config)?;
        let version_url = config.version_endpoint();
        let status = client.get(&version_url).and_then(|r| {
            if r.ok() {
                Ok(r)
            } else {
                Err(Error(format!(
                    "request failed: url: {version_url} status code: {} status: {} {} \n{}",
                    r.status,
                    r.status,
                    status_text(r.status),
                    r.body
                )))
            }
        });
        let version_body = match status {
            Ok(r) => r.body,
            Err(e) => {
                logger.error("connecting to graphql-engine server failed");
                logger.info("possible reasons:");
                logger.info("1) Provided root endpoint of graphql-engine server is wrong. Verify endpoint key in config.yaml or/and value of --endpoint flag");
                logger.info("2) Endpoint should NOT be your GraphQL API, ie endpoint is NOT https://hasura-cloud-app.io/v1/graphql it should be: https://hasura-cloud-app.io");
                logger
                    .info("3) Server might be unhealthy and is not running/accepting API requests");
                logger.info("4) Admin secret is not correct/set");
                logger.info("");
                let msg = if e.0.starts_with("request failed") {
                    e.0
                } else {
                    format!("making http request failed: {e}")
                };
                return Err(Error(msg));
            }
        };
        let server_version = serde_json::from_str::<serde_json::Value>(&version_body)
            .ok()
            .and_then(|v| {
                v.get("version")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        client.set_admin_secret(config.get_admin_secret());

        if config.version < 3 {
            return Err(Error(
                "this CLI supports config v3 projects only. Upgrade the project with the hasura CLI v2: hasura scripts update-project-v3".into(),
            ));
        }
        let migrations_dir = dir.join(&config.migrations_directory);
        if !migrations_dir.exists() {
            std::fs::create_dir_all(&migrations_dir).ctx("cannot create migrations directory")?;
        }
        let seeds_dir = dir.join(&config.seeds_directory);
        if !seeds_dir.exists() {
            std::fs::create_dir_all(&seeds_dir).ctx("cannot create seeds directory")?;
        }
        let metadata_dir = dir.join(&config.metadata_directory);
        let mut metadata_file = None;
        let mut metadata_mode = MetadataMode::Directory;
        if config.version >= 2 && !config.metadata_directory.is_empty() {
            if !config.metadata_file.is_empty() {
                let f = dir.join(&config.metadata_file);
                if !f.exists() {
                    std::fs::write(&f, b"")?;
                }
                metadata_mode = match f.extension().and_then(|e| e.to_str()) {
                    Some("json") => MetadataMode::Json,
                    Some("yaml") => MetadataMode::Yaml,
                    _ => {
                        return Err(Error(
                            "unrecogonized file extension. only .json/.yaml files are allowed for value of metadata_file".into(),
                        ));
                    }
                };
                metadata_file = Some(f);
            }
            if !metadata_dir.exists() && metadata_file.is_none() {
                std::fs::create_dir_all(&metadata_dir).ctx("cannot create metadata directory")?;
            }
        }
        logger.debug(&format!("graphql engine endpoint: {}", config.endpoint));
        Ok(Ctx {
            flags: flags.clone(),
            logger,
            dir,
            config,
            client,
            migrations_dir,
            seeds_dir,
            metadata_dir,
            metadata_file,
            metadata_mode,
            server_version,
        })
    }
}

fn status_text(code: u16) -> &'static str {
    match code {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}
