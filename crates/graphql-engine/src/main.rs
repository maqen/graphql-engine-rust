//! `graphql-engine`: a drop-in replacement for the Hasura V2 server binary.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use hge_server::ServerConfig;
use hge_server::telemetry::{Level, LogConfig, LogTypes};

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

/// Milliseconds in Haskell `Double` syntax (`500`, `500.5`, `5e2`), as V2 reads its
/// interval settings; must be non-negative.
fn parse_millis(s: &str) -> Result<Duration, String> {
    let (mantissa, exponent) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (s, None),
    };
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa
        .strip_prefix('-')
        .unwrap_or(mantissa)
        .split_once('.')
    {
        Some((i, f)) => digits(i) && digits(f),
        None => digits(mantissa.strip_prefix('-').unwrap_or(mantissa)),
    };
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)));
    let value: f64 = match (mantissa_ok && exponent_ok)
        .then(|| s.parse().ok())
        .flatten()
    {
        Some(v) => v,
        None => return Err("Prelude.read: no parse".to_owned()),
    };
    if value < 0.0 {
        return Err("Only expecting a non negative numeric".to_owned());
    }
    Duration::try_from_secs_f64(value / 1000.0).map_err(|_| "Prelude.read: no parse".to_owned())
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

    #[arg(long, env = "HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS", value_parser = parse_bool, default_value = "true")]
    infer_function_permissions: bool,

    #[arg(long, env = "HASURA_GRAPHQL_CORS_DOMAIN", default_value = "*")]
    cors_domain: String,

    #[arg(long, env = "HASURA_GRAPHQL_DISABLE_CORS", value_parser = parse_bool, default_value = "false")]
    disable_cors: bool,

    #[arg(
        long,
        env = "HASURA_GRAPHQL_ENABLED_APIS",
        default_value = "metadata,graphql,pgdump,config"
    )]
    enabled_apis: String,

    #[arg(long, env = "HASURA_GRAPHQL_PG_CONNECTIONS", default_value = "50")]
    pg_connections: usize,

    #[arg(long, env = "HASURA_GRAPHQL_USE_PREPARED_STATEMENTS", value_parser = parse_bool, default_value = "true")]
    use_prepared_statements: bool,

    #[arg(long, env = "HASURA_GRAPHQL_PRESERVE_401_ERRORS", value_parser = parse_bool, default_value = "false")]
    preserve_401_errors: bool,

    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_RELAY", value_parser = parse_bool, default_value = "true")]
    enable_relay: bool,

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

    /// Async action poll interval in milliseconds (0 disables the processor).
    #[arg(
        long,
        env = "HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL",
        default_value = "1000",
        value_parser = parse_millis
    )]
    async_actions_fetch_interval: Duration,

    /// Schema of the metadata database in which `pgcrypto` is created.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_METADATA_DATABASE_EXTENSIONS_SCHEMA",
        default_value = "public"
    )]
    metadata_database_extensions_schema: String,

    /// Async actions claimed (and processed concurrently) per poll.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_BATCH_SIZE",
        default_value = "10"
    )]
    async_actions_fetch_batch_size: u32,

    /// Use a remote schema's `data` even when it also returns `errors`.
    #[arg(long, env = "HASURA_GRAPHQL_REMOTE_SCHEMA_PRIORITIZE_DATA", value_parser = parse_bool, default_value = "false")]
    remote_schema_prioritize_data: bool,

    /// Accepted for compatibility; the console is not served.
    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_CONSOLE", value_parser = parse_bool, default_value = "false")]
    enable_console: bool,

    /// Only allowlisted queries for non-admin roles (CE: global collections only).
    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_ALLOWLIST", value_parser = parse_bool, default_value = "false")]
    enable_allowlist: bool,

    /// Reported by `/v1alpha1/config`; remote schema permissions are always enforced.
    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_REMOTE_SCHEMA_PERMISSIONS", value_parser = parse_bool, default_value = "false")]
    enable_remote_schema_permissions: bool,

    /// Comma-separated experimental features (reported by `/v1alpha1/config`).
    #[arg(long, env = "HASURA_GRAPHQL_EXPERIMENTAL_FEATURES", default_value = "")]
    experimental_features: String,

    /// `hasura-default` or `graphql-default`.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_DEFAULT_NAMING_CONVENTION",
        default_value = "hasura-default"
    )]
    default_naming_convention: String,

    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_APOLLO_FEDERATION", value_parser = parse_bool, default_value = "false")]
    enable_apollo_federation: bool,

    #[arg(long, env = "HASURA_GRAPHQL_LOG_LEVEL", default_value = "info")]
    log_level: String,

    #[arg(long, env = "HASURA_GRAPHQL_ENABLED_LOG_TYPES")]
    enabled_log_types: Option<String>,

    /// Log `/v1/metadata` and `/v1/query` request bodies in `http-log` (with `query-log`).
    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_METADATA_QUERY_LOGGING", value_parser = parse_bool, default_value = "false")]
    enable_metadata_query_logging: bool,

    /// Log request bodies in `http-log` only for failed requests.
    #[arg(long, env = "HASURA_GRAPHQL_HTTP_LOG_QUERY_ONLY_ON_ERROR", value_parser = parse_bool, default_value = "false")]
    http_log_query_only_on_error: bool,

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

    /// Seconds to wait for in-flight event deliveries and async actions on shutdown.
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

    /// Seconds between WebSocket keep-alive messages.
    #[arg(long, env = "HASURA_GRAPHQL_WEBSOCKET_KEEPALIVE", default_value = "5")]
    websocket_keepalive: u64,

    /// Seconds a WebSocket client has to send a successful `connection_init`.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_WEBSOCKET_CONNECTION_INIT_TIMEOUT",
        default_value = "3"
    )]
    websocket_connection_init_timeout: u64,

    /// Results queued per WebSocket connection before the operation producing them stops.
    #[arg(long, env = "HASURA_GRAPHQL_WEBSOCKET_QUEUE_SIZE", default_value = "300", value_parser = clap::value_parser!(u64).range(1..))]
    websocket_queue_size: u64,

    /// Accepted for compatibility; WebSocket compression is not supported.
    #[arg(long = "websocket-compression", env = "HASURA_GRAPHQL_CONNECTION_COMPRESSION", value_parser = parse_bool, default_value = "false")]
    connection_compression: bool,

    /// Keep the `Cookie` header of WebSocket upgrades when CORS is disabled.
    #[arg(long, env = "HASURA_GRAPHQL_WS_READ_COOKIE", value_parser = parse_bool, default_value = "false")]
    ws_read_cookie: bool,

    /// Close all WebSockets with 1012 after a metadata change through this instance.
    #[arg(long = "close-websockets-on-metadata-change", env = "HASURA_GRAPHQL_CLOSE_WEBSOCKETS_ON_METADATA_CHANGE", value_parser = parse_bool, default_value = "true")]
    close_websockets_on_metadata_change: bool,

    /// Milliseconds between live query polls.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_LIVE_QUERIES_MULTIPLEXED_REFETCH_INTERVAL",
        default_value = "1000"
    )]
    live_queries_multiplexed_refetch_interval: u64,

    /// Live query cohorts per database round trip.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_LIVE_QUERIES_MULTIPLEXED_BATCH_SIZE",
        default_value = "100"
    )]
    live_queries_multiplexed_batch_size: usize,

    /// Milliseconds between streaming subscription polls.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_STREAMING_QUERIES_MULTIPLEXED_REFETCH_INTERVAL",
        default_value = "1000"
    )]
    streaming_queries_multiplexed_refetch_interval: u64,

    /// Streaming subscription cohorts per database round trip.
    #[arg(
        long,
        env = "HASURA_GRAPHQL_STREAMING_QUERIES_MULTIPLEXED_BATCH_SIZE",
        default_value = "100"
    )]
    streaming_queries_multiplexed_batch_size: usize,

    /// Log failed event deliveries at error level.
    #[arg(long, env = "HASURA_GRAPHQL_ENABLE_TRIGGERS_ERROR_LOG_LEVEL", value_parser = parse_bool, default_value = "false")]
    enable_triggers_error_log_level: bool,
}

/// The log filter from `HASURA_GRAPHQL_LOG_LEVEL` / `HASURA_GRAPHQL_ENABLED_LOG_TYPES`, with
/// V2's error messages, plus the EE-only log types that were ignored.
fn log_config(args: &ServeArgs) -> anyhow::Result<(LogConfig, Vec<String>)> {
    let level = Level::parse(&args.log_level)
        .map_err(|e| anyhow::anyhow!("Environment variable HASURA_GRAPHQL_LOG_LEVEL: {e}"))?;
    let (types, ignored) = match &args.enabled_log_types {
        None => (LogTypes::DEFAULT, vec![]),
        Some(s) => LogTypes::parse(s).map_err(|e| {
            anyhow::anyhow!("Environment variable HASURA_GRAPHQL_ENABLED_LOG_TYPES: {e}")
        })?,
    };
    Ok((LogConfig { level, types }, ignored))
}

/// The `server_configuration` startup log (spec 11 §5.4), keys in V2's order.
fn server_configuration(
    args: &ServeArgs,
    config: &ServerConfig,
    log: &LogConfig,
) -> serde_json::Value {
    use serde_json::{Value, json};
    let jwt_secret: Vec<Value> = config
        .jwt_secret
        .iter()
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .chain(
            config
                .jwt_secrets
                .iter()
                .filter_map(|s| serde_json::from_str::<Vec<Value>>(s).ok())
                .flatten(),
        )
        .map(|mut v| {
            if let Some(o) = v.as_object_mut() {
                if o.contains_key("type") {
                    o.insert("type".into(), "<TYPE REDACTED>".into());
                }
                if o.contains_key("key") {
                    o.insert("key".into(), "<JWK REDACTED>".into());
                }
            }
            v
        })
        .collect();
    let allowed_origins: Value = if config.disable_cors {
        Value::Null
    } else if config.cors_domain.trim() == "*" {
        "*".into()
    } else {
        config
            .cors_domain
            .split(',')
            .map(|d| Value::from(d.trim()))
            .collect()
    };
    let secs = |d: std::time::Duration| d.as_secs_f64();
    json!({
        "port": config.port,
        "server_host": if args.server_host == "*" { "HostAny".to_owned() } else { format!("Host {:?}", args.server_host) },
        "transaction_isolation": "ReadCommitted",
        "admin_secret_set": !config.admin_secrets.is_empty(),
        "disable_admin_secret": config.disable_admin_secret,
        "auth_hook": config.auth_hook.as_ref().map(|h| h.url.clone()),
        "auth_hook_mode": config.auth_hook.as_ref().map(|h| match h.mode {
            hge_auth::HookMode::Get => "GET",
            hge_auth::HookMode::Post => "POST",
        }),
        "jwt_secret": jwt_secret,
        "unauth_role": config.unauthorized_role,
        "cors_config": {
            "disabled": config.disable_cors,
            "ws_read_cookie": if config.disable_cors { Value::from(args.ws_read_cookie) } else { Value::Null },
            "allowed_origins": allowed_origins,
        },
        "enable_console": args.enable_console,
        "console_assets_dir": null,
        "console_sentry_dsn": null,
        "enable_telemetry": false,
        "use_prepared_statements": config.use_prepared_statements,
        "stringify_numeric_types": config.stringify_numeric_types,
        "v1-boolean-null-collapse": config.v1_boolean_null_collapse,
        "enabled_apis": config.enabled_apis,
        "live_query_options": {
            "batch_size": config.websocket.live_queries.batch_size,
            "refetch_delay": secs(config.websocket.live_queries.refetch_interval),
        },
        "enable_allowlist": args.enable_allowlist,
        "enabled_log_types": log.types.names(),
        "log_level": log.level.as_str(),
        "remote_schema_permissions": false,
        "websocket_compression_options": "NoCompression",
        "websocket_keep_alive": format!("KeepAliveDelay {{unKeepAliveDelay = Seconds {{seconds = {}s}}}}", config.websocket.keep_alive.as_secs()),
        "infer_function_permissions": config.infer_function_permissions,
        "enable_maintenance_mode": false,
        "experimental_features": [],
        "events_fetch_batch_size": config.events.fetch_batch_size,
        "graceful_shutdown_timeout": config.events.graceful_shutdown_timeout.as_secs(),
        "websocket_connection_init_timeout": format!("WSConnectionInitTimeout {{unWSConnectionInitTimeout = Seconds {{seconds = {}s}}}}", config.websocket.init_timeout.as_secs()),
        "websocket_queue_size": config.websocket.queue_size,
        "enable_metadata_query_logging": config.enable_metadata_query_logging,
        "http_log_query_only_on_error": config.http_log_query_only_on_error,
    })
}

/// The startup logs V2 writes before connecting: the configuration, the metadata database
/// connection and the admin secret warning.
fn log_startup(args: &ServeArgs, config: &ServerConfig, log: &LogConfig, ignored: &[String]) {
    use hge_server::telemetry::{self, LogType};
    let startup = |level: Level, kind: &str, info: serde_json::Value| {
        telemetry::emit(
            LogType::Startup,
            level,
            &serde_json::json!({"kind": kind, "info": info}),
        );
    };
    startup(
        Level::Info,
        "server_configuration",
        server_configuration(args, config, log),
    );
    if let Some(url) = config.catalog_url() {
        let info = match telemetry::startup::mask_database_url(url) {
            Some(url) => serde_json::json!({"retries": 1, "database_url": url}),
            None => serde_json::json!({"error": "parsing database url failed"}),
        };
        startup(Level::Info, "postgres_connection", info);
    }
    if config.admin_secrets.is_empty() && !config.disable_admin_secret {
        startup(
            Level::Warn,
            "no_admin_secret",
            "WARNING: No admin secret provided".into(),
        );
    }
    if !ignored.is_empty() {
        startup(
            Level::Warn,
            "server",
            format!("ignoring enterprise-only log types: {}", ignored.join(", ")).into(),
        );
    }
}

fn serve_config(cli: &Cli, args: &ServeArgs) -> anyhow::Result<ServerConfig> {
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
        infer_function_permissions: args.infer_function_permissions,
        cors_domain: args.cors_domain.clone(),
        disable_cors: args.disable_cors,
        enabled_apis: enabled_apis(&args.enabled_apis)?,
        pg_connections: args.pg_connections,
        use_prepared_statements: args.use_prepared_statements,
        preserve_401_errors: args.preserve_401_errors,
        enable_relay: args.enable_relay,
        events: hge_events_config(args),
        enable_allowlist: args.enable_allowlist,
        websocket: websocket_config(args),
        read_only: args.read_only_mode,
        schema_sync_poll_interval_ms: if args.schema_sync_poll_interval >= 0.0 {
            args.schema_sync_poll_interval as u64
        } else {
            anyhow::bail!(
                "HASURA_GRAPHQL_SCHEMA_SYNC_POLL_INTERVAL: Only expecting a non negative numeric"
            )
        },
        configured_header_precedence: args.configured_header_precedence,
        async_actions_fetch_interval: args.async_actions_fetch_interval,
        async_actions_fetch_batch_size: args.async_actions_fetch_batch_size,
        metadata_database_extensions_schema: args.metadata_database_extensions_schema.clone(),
        enable_metadata_query_logging: args.enable_metadata_query_logging,
        http_log_query_only_on_error: args.http_log_query_only_on_error,
        remote_schema_prioritize_data: args.remote_schema_prioritize_data,
        remote_schema_permissions: args.enable_remote_schema_permissions,
        experimental_features: {
            let mut features: Vec<String> = args
                .experimental_features
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            features.sort();
            features.dedup();
            features
        },
        default_naming_convention: args.default_naming_convention.clone(),
        apollo_federation: args.enable_apollo_federation,
        ..ServerConfig::default()
    })
}

/// `HASURA_GRAPHQL_ENABLED_APIS`: comma-separated, case-insensitive (spec 10 §11).
fn enabled_apis(s: &str) -> anyhow::Result<Vec<String>> {
    const APIS: [&str; 6] = [
        "metadata",
        "graphql",
        "pgdump",
        "developer",
        "config",
        "metrics",
    ];
    let mut out: Vec<String> = vec![];
    for item in s.split(',') {
        let api = item.trim().to_ascii_lowercase();
        if !APIS.contains(&api.as_str()) {
            anyhow::bail!(
                "Environment variable HASURA_GRAPHQL_ENABLED_APIS: Only expecting list of comma separated API types metadata,graphql,pgdump,developer,config,metrics"
            );
        }
        if !out.contains(&api) {
            out.push(api);
        }
    }
    Ok(out)
}

fn websocket_config(args: &ServeArgs) -> hge_server::config::WebSocketConfig {
    use std::time::Duration;
    let poller = |interval_ms: u64, batch_size: usize| hge_server::ws::PollerOptions {
        refetch_interval: Duration::from_millis(interval_ms),
        batch_size: batch_size.max(1),
    };
    if args.connection_compression {
        tracing::warn!(target: "startup", "WebSocket compression is not supported; ignoring HASURA_GRAPHQL_CONNECTION_COMPRESSION");
    }
    hge_server::config::WebSocketConfig {
        keep_alive: Duration::from_secs(args.websocket_keepalive),
        init_timeout: Duration::from_secs(args.websocket_connection_init_timeout),
        queue_size: args.websocket_queue_size as usize,
        close_on_metadata_change: args.close_websockets_on_metadata_change,
        read_cookie: args.ws_read_cookie,
        live_queries: poller(
            args.live_queries_multiplexed_refetch_interval,
            args.live_queries_multiplexed_batch_size,
        ),
        streaming: poller(
            args.streaming_queries_multiplexed_refetch_interval,
            args.streaming_queries_multiplexed_batch_size,
        ),
    }
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
            println!(
                "{} (graphql-engine-rust {})",
                hge_server::routes::VERSION,
                hge_server::routes::ENGINE_VERSION
            );
            Ok(())
        }
        Command::ExportMetadata { dir } => {
            let json = hge_metadata::load_dir_json(dir)?;
            println!("{}", serde_json::to_string_pretty(&json)?);
            Ok(())
        }
        Command::Serve(args) => {
            let (log, ignored) = log_config(args)?;
            let _flush = hge_server::telemetry::init(log);
            let config = serve_config(&cli, args)?;
            log_startup(args, &config, &log, &ignored);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let engine = hge_server::build_engine(config).await.map_err(|e| {
                    // V2 logs a failed start as a `catalog_migrate` error with the QErr.
                    hge_server::telemetry::emit(
                        hge_server::telemetry::LogType::Startup,
                        Level::Error,
                        &serde_json::json!({"kind": "catalog_migrate", "info": e.to_api_json(true)}),
                    );
                    match &e.internal {
                        Some(internal) => anyhow::anyhow!("{e} ({internal})"),
                        None => anyhow::anyhow!("{e}"),
                    }
                })?;
                hge_server::serve(engine).await?;
                Ok(())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millis_use_double_syntax() {
        assert_eq!(parse_millis("1000"), Ok(Duration::from_secs(1)));
        assert_eq!(parse_millis("500.5"), Ok(Duration::from_micros(500_500)));
        assert_eq!(parse_millis("5e2"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_millis("0"), Ok(Duration::ZERO));
        for bad in [".5", "5.", "", "abc", "1_000", "inf", "NaN", "1e"] {
            assert_eq!(
                parse_millis(bad),
                Err("Prelude.read: no parse".to_owned()),
                "{bad}"
            );
        }
        assert_eq!(
            parse_millis("-1"),
            Err("Only expecting a non negative numeric".to_owned())
        );
    }

    fn serve_args(env: &[&str]) -> (Cli, ServeArgs) {
        let argv = ["graphql-engine", "serve"].iter().chain(env).copied();
        let cli = Cli::parse_from(argv);
        let Command::Serve(args) = cli.command else {
            unreachable!()
        };
        let cli = Cli {
            command: Command::Version,
            ..cli
        };
        (cli, *args)
    }

    #[test]
    fn log_settings() {
        let (_, args) = serve_args(&[
            "--log-level",
            "WARN",
            "--enabled-log-types",
            "startup,query-log",
        ]);
        let (log, ignored) = log_config(&args).unwrap();
        assert_eq!(log.level, Level::Warn);
        assert_eq!(log.types.names(), ["startup", "query-log"]);
        assert!(ignored.is_empty());
        let (_, args) = serve_args(&["--log-level", "verbose"]);
        assert_eq!(
            log_config(&args).unwrap_err().to_string(),
            "Environment variable HASURA_GRAPHQL_LOG_LEVEL: Valid log levels: debug, info, warn or error"
        );
    }

    #[test]
    fn server_configuration_keys() {
        let (cli, args) = serve_args(&[
            "--jwt-secret",
            r#"{"type":"HS256","key":"0123456789abcdef0123456789abcdef"}"#,
            "--admin-secret",
            "s",
        ]);
        let config = serve_config(&cli, &args).unwrap();
        let info = server_configuration(&args, &config, &LogConfig::default());
        let keys: Vec<&str> = info
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "port",
                "server_host",
                "transaction_isolation",
                "admin_secret_set",
                "disable_admin_secret",
                "auth_hook",
                "auth_hook_mode",
                "jwt_secret",
                "unauth_role",
                "cors_config",
                "enable_console",
                "console_assets_dir",
                "console_sentry_dsn",
                "enable_telemetry",
                "use_prepared_statements",
                "stringify_numeric_types",
                "v1-boolean-null-collapse",
                "enabled_apis",
                "live_query_options",
                "enable_allowlist",
                "enabled_log_types",
                "log_level",
                "remote_schema_permissions",
                "websocket_compression_options",
                "websocket_keep_alive",
                "infer_function_permissions",
                "enable_maintenance_mode",
                "experimental_features",
                "events_fetch_batch_size",
                "graceful_shutdown_timeout",
                "websocket_connection_init_timeout",
                "websocket_queue_size",
                "enable_metadata_query_logging",
                "http_log_query_only_on_error",
            ]
        );
        assert_eq!(
            info["jwt_secret"],
            serde_json::json!([{"type": "<TYPE REDACTED>", "key": "<JWK REDACTED>"}])
        );
        assert_eq!(info["admin_secret_set"], true);
        assert_eq!(
            info["enabled_apis"],
            serde_json::json!(["metadata", "graphql", "pgdump", "config"])
        );
        assert_eq!(
            enabled_apis(" GraphQL ,developer").unwrap(),
            ["graphql", "developer"]
        );
        assert!(enabled_apis("graphql,console").is_err());
        assert_eq!(info["live_query_options"]["refetch_delay"], 1.0);
    }
}
