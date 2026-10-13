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

    #[arg(long, env = "HASURA_GRAPHQL_UNAUTHORIZED_ROLE")]
    unauthorized_role: Option<String>,

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
    if let Some(s) = &args.admin_secret {
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
    })
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
