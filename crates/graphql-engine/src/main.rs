//! `graphql-engine`: a drop-in replacement for the Hasura V2 server binary.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use hge_server::ServerConfig;

#[derive(Parser)]
#[command(
    name = "graphql-engine",
    version,
    about = "Hasura V2 compatible GraphQL engine"
)]
struct Cli {
    /// Postgres database URL (also the default metadata database).
    #[arg(
        long,
        global = true,
        env = "HASURA_GRAPHQL_DATABASE_URL",
        hide_env_values = true
    )]
    database_url: Option<String>,

    /// Database holding the `hdb_catalog` metadata.
    #[arg(
        long,
        global = true,
        env = "HASURA_GRAPHQL_METADATA_DATABASE_URL",
        hide_env_values = true
    )]
    metadata_database_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the GraphQL server.
    Serve(Box<ServeArgs>),
    /// Print the version.
    Version,
    /// Assemble a Hasura CLI metadata directory into metadata JSON (stdout).
    ExportMetadata {
        #[arg(long)]
        dir: PathBuf,
    },
}

fn parse_bool(s: &str) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "1" | "yes" | "y" | "on" => Ok(true),
        "false" | "f" | "0" | "no" | "n" | "off" | "" => Ok(false),
        other => Err(format!("expected a boolean, found '{other}'")),
    }
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, env = "HASURA_GRAPHQL_SERVER_HOST", default_value = "*")]
    server_host: String,

    /// Port (falls back to `$PORT`, as set by Cloud Run, then 8080).
    #[arg(long, env = "HASURA_GRAPHQL_SERVER_PORT")]
    server_port: Option<u16>,

    /// Load metadata from a Hasura CLI metadata directory instead of the catalog.
    #[arg(long, env = "HASURA_GRAPHQL_METADATA_DIR")]
    metadata_dir: Option<PathBuf>,

    /// Apply a Hasura CLI migrations directory on boot (like the `cli-migrations` image).
    #[arg(long, env = "HASURA_GRAPHQL_MIGRATIONS_DIR")]
    migrations_dir: Option<PathBuf>,

    #[arg(long, env = "HASURA_GRAPHQL_ADMIN_SECRET", hide_env_values = true)]
    admin_secret: Option<String>,

    /// JSON list of admin secrets (`["a","b"]`).
    #[arg(long, env = "HASURA_GRAPHQL_ADMIN_SECRETS", hide_env_values = true)]
    admin_secrets: Option<String>,

    /// Deprecated alias of the admin secret, used only when that is unset.
    #[arg(long, env = "HASURA_GRAPHQL_ACCESS_KEY", hide_env_values = true)]
    access_key: Option<String>,

    #[arg(long, env = "HASURA_GRAPHQL_UNAUTHORIZED_ROLE")]
    unauthorized_role: Option<String>,

    #[arg(long, env = "HASURA_GRAPHQL_DISABLE_ADMIN_SECRET", value_parser = parse_bool, default_value = "false")]
    disable_admin_secret: bool,

    /// JWT config (a JSON object).
    #[arg(long, env = "HASURA_GRAPHQL_JWT_SECRET", hide_env_values = true)]
    jwt_secret: Option<String>,

    /// A JSON list of JWT configs.
    #[arg(long, env = "HASURA_GRAPHQL_JWT_SECRETS", hide_env_values = true)]
    jwt_secrets: Option<String>,

    /// Auth webhook URL.
    #[arg(long, env = "HASURA_GRAPHQL_AUTH_HOOK")]
    auth_hook: Option<String>,

    /// `GET` (default) or `POST`; `HASURA_GRAPHQL_AUTH_HOOK_TYPE` is read as a fallback.
    #[arg(long, env = "HASURA_GRAPHQL_AUTH_HOOK_MODE")]
    auth_hook_mode: Option<String>,

    #[arg(long, env = "HASURA_GRAPHQL_AUTH_HOOK_SEND_REQUEST_BODY", value_parser = parse_bool, default_value = "true")]
    auth_hook_send_request_body: bool,

    /// Reuse webhook answers that carry an expiry (not in V2 OSS; off by default).
    #[arg(long, env = "HASURA_GRAPHQL_AUTH_HOOK_CACHE", value_parser = parse_bool, default_value = "false")]
    auth_hook_cache: bool,

    #[arg(long, env = "HASURA_GRAPHQL_DEV_MODE", value_parser = parse_bool, default_value = "false")]
    dev_mode: bool,

    #[arg(long, env = "HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS", value_parser = parse_bool, default_value = "true")]
    admin_internal_errors: bool,

    #[arg(long, env = "HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES", value_parser = parse_bool, default_value = "false")]
    stringify_numeric_types: bool,

    #[arg(long, env = "HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE", value_parser = parse_bool, default_value = "false")]
    v1_boolean_null_collapse: bool,

    #[arg(long, env = "HASURA_GRAPHQL_CORS_DOMAIN", default_value = "*")]
    cors_domain: String,

    #[arg(long, env = "HASURA_GRAPHQL_DISABLE_CORS", value_parser = parse_bool, default_value = "false")]
    disable_cors: bool,

    #[arg(
        long,
        env = "HASURA_GRAPHQL_ENABLED_APIS",
        default_value = "graphql,metadata"
    )]
    enabled_apis: String,

    #[arg(long, env = "HASURA_GRAPHQL_PG_CONNECTIONS", default_value = "50")]
    pg_connections: usize,

    #[arg(long, env = "HASURA_GRAPHQL_USE_PREPARED_STATEMENTS", value_parser = parse_bool, default_value = "true")]
    use_prepared_statements: bool,

    #[arg(long, env = "HASURA_GRAPHQL_PRESERVE_401_ERRORS", value_parser = parse_bool, default_value = "false")]
    preserve_401_errors: bool,

    #[arg(long = "read-only-mode", env = "HASURA_GRAPHQL_ENABLE_READ_ONLY_MODE", value_parser = parse_bool, default_value = "false")]
    read_only_mode: bool,

    /// Milliseconds between checks for metadata changed by other instances (0 disables).
    #[arg(
        long,
        env = "HASURA_GRAPHQL_SCHEMA_SYNC_POLL_INTERVAL",
        default_value = "1000"
    )]
    schema_sync_poll_interval: f64,
    /// Configured action headers take precedence over forwarded client headers.
    #[arg(long, env = "HASURA_GRAPHQL_CONFIGURED_HEADER_PRECEDENCE", value_parser = parse_bool, default_value = "false")]
    configured_header_precedence: bool,

    /// Accepted for compatibility; the console is not served.
    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_CONSOLE", value_parser = parse_bool, default_value = "false")]
    enable_console: bool,

    /// Not implemented yet: refusing to start beats silently accepting every query.
    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_ALLOWLIST", value_parser = parse_bool, default_value = "false")]
    enable_allowlist: bool,

    #[arg(long, env = "HASURA_GRAPHQL_LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Accepted for compatibility.
    #[arg(long, env = "HASURA_GRAPHQL_ENABLED_LOG_TYPES")]
    enabled_log_types: Option<String>,

    /// Maximum concurrent event trigger deliveries.
    #[arg(long, env = "HASURA_GRAPHQL_EVENTS_HTTP_POOL_SIZE", default_value = "100", value_parser = clap::value_parser!(u32).range(1..))]
    events_http_pool_size: u32,

    /// Events fetched per source per poll; 0 disables delivery.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_EVENTS_FETCH_BATCH_SIZE",
        default_value = "100"
    )]
    events_fetch_batch_size: u32,

    /// Milliseconds to wait after a poll that found no events; 0 disables delivery.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL",
        default_value = "1000"
    )]
    events_fetch_interval: u64,

    /// Seconds to wait for in-flight deliveries on shutdown.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT",
        default_value = "60"
    )]
    graceful_shutdown_timeout: u64,

    /// No event trigger catalog set-up and no delivery workers.
    #[arg(long, env = "HASURA_GRAPHQL_DISABLE_EVENTING", value_parser = parse_bool, default_value = "false")]
    disable_eventing: bool,

    /// Keep SQL triggers up to date but deliver nothing from this instance.
    #[arg(long, env = "HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING", value_parser = parse_bool, default_value = "false")]
    disable_event_processing: bool,

    /// Log failed event deliveries at error level.
    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_TRIGGERS_ERROR_LOG_LEVEL", value_parser = parse_bool, default_value = "false")]
    enable_triggers_error_log_level: bool,
}

fn init_logging(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .init();
}

fn serve_config(cli: &Cli, args: &ServeArgs) -> anyhow::Result<ServerConfig> {
    if args.enable_allowlist {
        anyhow::bail!(
            "HASURA_GRAPHQL_ENABLE_ALLOWLIST is not supported yet; refusing to start without it"
        );
    }
    let mut admin_secrets = vec![];
    if let Some(s) = args.admin_secret.as_ref().or(args.access_key.as_ref()) {
        admin_secrets.push(s.clone());
    }
    if let Some(list) = &args.admin_secrets {
        let parsed: Vec<String> = serde_json::from_str(list)
            .context("HASURA_GRAPHQL_ADMIN_SECRETS must be a JSON list of strings")?;
        admin_secrets.extend(parsed);
    }
    if admin_secrets.iter().any(|s| s.trim().is_empty()) {
        anyhow::bail!("the admin secret must not be empty");
    }
    let auth_hook = match &args.auth_hook {
        None => None,
        Some(url) => {
            let mode = match args
                .auth_hook_mode
                .clone()
                .or_else(|| std::env::var("HASURA_GRAPHQL_AUTH_HOOK_TYPE").ok())
            {
                None => hge_auth::HookMode::Get,
                Some(m) => m
                    .parse()
                    .map_err(|e| anyhow::anyhow!("HASURA_GRAPHQL_AUTH_HOOK_MODE: {e}"))?,
            };
            Some(hge_auth::HookConfig {
                url: url.clone(),
                mode,
                send_request_body: args.auth_hook_send_request_body,
                cache: args.auth_hook_cache,
            })
        }
    };
    let port = match args.server_port {
        Some(p) => p,
        None => std::env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8080),
    };
    let host = if args.server_host == "*" {
        "0.0.0.0".to_owned()
    } else {
        args.server_host.clone()
    };
    Ok(ServerConfig {
        host,
        port,
        database_url: cli.database_url.clone(),
        metadata_database_url: cli.metadata_database_url.clone(),
        metadata_dir: args.metadata_dir.clone(),
        migrations_dir: args.migrations_dir.clone(),
        admin_secrets,
        unauthorized_role: args.unauthorized_role.clone(),
        disable_admin_secret: args.disable_admin_secret,
        jwt_secret: args.jwt_secret.clone(),
        jwt_secrets: args.jwt_secrets.clone(),
        auth_hook,
        dev_mode: args.dev_mode,
        admin_internal_errors: args.admin_internal_errors,
        stringify_numeric_types: args.stringify_numeric_types,
        v1_boolean_null_collapse: args.v1_boolean_null_collapse,
        cors_domain: args.cors_domain.clone(),
        disable_cors: args.disable_cors,
        enabled_apis: args
            .enabled_apis
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        pg_connections: args.pg_connections,
        use_prepared_statements: args.use_prepared_statements,
        preserve_401_errors: args.preserve_401_errors,
        events: hge_events_config(args),
        read_only: args.read_only_mode,
        schema_sync_poll_interval_ms: if args.schema_sync_poll_interval >= 0.0 {
            args.schema_sync_poll_interval as u64
        } else {
            anyhow::bail!(
                "HASURA_GRAPHQL_SCHEMA_SYNC_POLL_INTERVAL: Only expecting a non negative numeric"
            )
        },
        configured_header_precedence: args.configured_header_precedence,
        ..ServerConfig::default()
    })
}

fn hge_events_config(args: &ServeArgs) -> hge_server::config::EventingConfig {
    hge_server::config::EventingConfig {
        http_pool_size: args.events_http_pool_size as usize,
        fetch_batch_size: i64::from(args.events_fetch_batch_size),
        fetch_interval: std::time::Duration::from_millis(args.events_fetch_interval),
        graceful_shutdown_timeout: std::time::Duration::from_secs(args.graceful_shutdown_timeout),
        disable_eventing: args.disable_eventing,
        disable_event_processing: args.disable_event_processing,
        triggers_error_log_level: args.enable_triggers_error_log_level,
        ..ServerConfig::default().events
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Version => {
            println!("{}", hge_server::routes::VERSION);
            Ok(())
        }
        Command::ExportMetadata { dir } => {
            let json = hge_metadata::load_dir_json(dir)?;
            println!("{}", serde_json::to_string_pretty(&json)?);
            Ok(())
        }
        Command::Serve(args) => {
            init_logging(&args.log_level);
            let config = serve_config(&cli, args)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let engine =
                    hge_server::build_engine(config)
                        .await
                        .map_err(|e| match &e.internal {
                            Some(internal) => anyhow::anyhow!("{e} ({internal})"),
                            None => anyhow::anyhow!("{e}"),
                        })?;
                hge_server::serve(engine).await?;
                Ok(())
            })
        }
    }
}
