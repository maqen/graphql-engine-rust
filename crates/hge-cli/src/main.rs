//! The `hasura` command line: the official CLI's commands, aliases and flags.

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Args, Parser, Subcommand};

use hge_cli::cmd::{self, Database, DatabaseArgs, ServerArgs};
use hge_cli::config::ServerFlags;
use hge_cli::context::GlobalFlags;
use hge_cli::error::{Error, Result};
use hge_cli::logger::fatal_line;

#[derive(Parser)]
#[command(
    name = "hasura",
    about = "Hasura GraphQL Engine command line tool",
    disable_version_flag = true
)]
#[command(arg_required_else_help = true)]
struct Cli {
    /// log level (DEBUG, INFO, WARN, ERROR, FATAL)
    #[arg(long, global = true, default_value = "INFO")]
    log_level: String,
    /// directory where commands are executed (default: current dir)
    #[arg(long, global = true)]
    project: Option<String>,
    /// do not colorize output (default: false)
    #[arg(long, global = true)]
    no_color: bool,
    /// .env filename to load ENV vars from
    #[arg(long, global = true, default_value = ".env")]
    envfile: String,
    /// skip automatic update check on command execution
    #[arg(long, global = true)]
    skip_update_check: bool,
    #[arg(long, global = true, hide = true)]
    cli_ext_path: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone, Default)]
struct Server {
    /// http(s) endpoint for Hasura GraphQL Engine
    #[arg(long, global = true)]
    endpoint: Option<String>,
    /// admin secret for Hasura GraphQL Engine
    #[arg(long, global = true)]
    admin_secret: Option<String>,
    #[arg(long, global = true, hide = true)]
    access_key: Option<String>,
    /// disable TLS verification for the endpoint
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    insecure_skip_tls_verify: Option<bool>,
    /// path to a cert file for the certificate authority
    #[arg(long, global = true)]
    certificate_authority: Option<String>,
}

#[derive(Args, Clone, Default)]
struct Interactive {
    /// disables interactive prompts (default: false)
    #[arg(long, global = true)]
    disable_interactive: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Print the CLI version
    Version,
    /// Initialize a new Hasura GraphQL Engine project
    Init(InitArgs),
    /// Manage Hasura Metadata
    #[command(alias = "md", arg_required_else_help = true)]
    Metadata {
        #[command(flatten)]
        server: Server,
        #[command(subcommand)]
        command: MetadataCommand,
    },
    /// Manage migrations on the database
    #[command(arg_required_else_help = true)]
    Migrate {
        #[command(flatten)]
        server: Server,
        #[command(flatten)]
        interactive: Interactive,
        /// database on which operation should be applied
        #[arg(long, global = true)]
        database_name: Option<String>,
        #[arg(long, global = true, hide = true)]
        disable_auto_state_migration: bool,
        #[command(subcommand)]
        command: MigrateCommand,
    },
    /// Manage seed data
    #[command(aliases = ["sd", "seeds"], arg_required_else_help = true)]
    Seed {
        #[command(flatten)]
        server: Server,
        #[command(flatten)]
        interactive: Interactive,
        /// database on which operation should be applied
        #[arg(long, global = true)]
        database_name: Option<String>,
        #[command(subcommand)]
        command: SeedCommand,
    },
    /// Utility command to apply metadata & database migrations to graphql-engine
    Deploy(DeployArgs),
}

#[derive(Args)]
struct InitArgs {
    directory_name: Option<String>,
    /// config version to be used
    #[arg(long, default_value_t = 3)]
    version: i64,
    #[arg(long, hide = true)]
    directory: Option<String>,
    /// http(s) endpoint for Hasura GraphQL Engine
    #[arg(long, default_value = "")]
    endpoint: String,
    /// admin secret for Hasura GraphQL Engine
    #[arg(long)]
    admin_secret: Option<String>,
    #[arg(long, hide = true)]
    access_key: Option<String>,
    #[arg(long, hide = true)]
    install_manifest: Option<String>,
    #[arg(long, hide = true)]
    fetch: bool,
}

#[derive(Subcommand)]
enum MetadataCommand {
    /// Apply Hasura Metadata on a database
    Apply {
        #[arg(long, hide = true)]
        from_file: bool,
        /// show metadata generated from project directory without applying to server
        #[arg(long)]
        dry_run: bool,
        /// specify an output format to show applied metadata. Allowed values: json, yaml
        #[arg(short, long, default_value = "")]
        output: String,
        /// disallow inconsistent metadata to be applied. Defaults to false
        #[arg(long)]
        disallow_inconsistent_metadata: bool,
    },
    /// Export Hasura metadata to a file
    Export {
        /// write metadata to standard output in given format. Allowed values: json, yaml
        #[arg(short, long, default_value = "")]
        output: String,
    },
    /// Clear Hasura Metadata on a database
    #[command(alias = "reset")]
    Clear,
    /// Reload Hasura GraphQL Engine schema to pick up changes in any underlying data sources
    Reload,
    /// (PREVIEW) Show a highlighted diff of the Hasura Metadata
    Diff {
        #[arg(num_args = 0..=2)]
        files: Vec<String>,
        /// specify a type of diff [allowed values: unified-json,unified-yaml, yaml, json]
        #[arg(long = "type", default_value = "")]
        diff_type: String,
    },
    /// Manage inconsistent objects in Hasura Metadata
    #[command(aliases = ["inconsistencies", "ic"], arg_required_else_help = true)]
    Inconsistency {
        #[command(subcommand)]
        command: InconsistencyCommand,
    },
}

#[derive(Subcommand)]
enum InconsistencyCommand {
    /// List all inconsistent objects from the metadata
    #[command(alias = "ls")]
    List {
        /// select output format for inconsistent metadata objects(Allowed values: json)
        #[arg(short, long, default_value = "")]
        output: String,
    },
    /// Check if the metadata is inconsistent or not
    Status,
    /// Drop inconsistent objects from the metadata
    Drop,
}

#[derive(Args, Default)]
struct Transactions {
    /// disable transaction for migration
    #[arg(long)]
    no_transaction: bool,
    /// enable per-migration transaction control
    #[arg(long)]
    per_migration_transaction: bool,
}

#[derive(Subcommand)]
enum MigrateCommand {
    /// Apply migrations on the database
    Apply {
        /// apply all or N up migration steps
        #[arg(long, default_value = "")]
        up: String,
        /// apply all or N down migration steps
        #[arg(long, default_value = "")]
        down: String,
        /// apply migration chain up to the version specified
        #[arg(long, default_value = "")]
        goto: String,
        /// only apply this particular migration
        #[arg(long, default_value = "")]
        version: String,
        /// skip executing the migration action, but mark them as applied
        #[arg(long)]
        skip_execution: bool,
        /// type of migration (up, down) to be used with version flag
        #[arg(long = "type", default_value = "up")]
        migration_type: String,
        /// print the names of migrations which are going to be applied
        #[arg(long)]
        dry_run: bool,
        /// set this flag to attempt to apply migrations on all databases present on server
        #[arg(long)]
        all_databases: bool,
        #[arg(long, hide = true)]
        progressbar_logs: bool,
        #[command(flatten)]
        transactions: Transactions,
    },
    /// Display current status of migrations on a database
    Status,
    /// Create files required for a migration
    Create {
        migration_name: String,
        /// take pg_dump of schema (default: public) and Hasura metadata from the server
        #[arg(long)]
        from_server: bool,
        /// path to an SQL file which contains the SQL statements
        #[arg(long, default_value = "")]
        sql_from_file: String,
        #[arg(long, hide = true)]
        sql_from_server: bool,
        /// name of Postgres schema to export as a migration
        #[arg(long = "schema", value_delimiter = ',', default_value = "public")]
        schemas: Vec<String>,
        #[arg(long, hide = true)]
        metadata_from_file: Option<String>,
        #[arg(long, hide = true)]
        metadata_from_server: bool,
        /// sql string/query that is to be used to create an up migration
        #[arg(long)]
        up_sql: Option<String>,
        /// sql string/query that is to be used to create a down migration
        #[arg(long)]
        down_sql: Option<String>,
    },
    /// (PREVIEW) Delete migrations from the migration directory and server
    Delete {
        /// deletes the specified version in migrations
        #[arg(long)]
        version: Option<u64>,
        /// clears all migrations for selected database
        #[arg(long)]
        all: bool,
        /// when set executes operation without any confirmation
        #[arg(long)]
        force: bool,
        /// to reset migrations only on server
        #[arg(long = "server")]
        only_server: bool,
    },
    /// (PREVIEW) Squash multiple migrations into a single one
    Squash {
        /// start squashing from this version
        #[arg(long, required = true)]
        from: Option<u64>,
        /// squash up to this version
        #[arg(long, default_value_t = -1, allow_negative_numbers = true)]
        to: i64,
        /// name for the new squashed migration
        #[arg(long, default_value = "squashed")]
        name: String,
        /// delete the source files after squashing without any confirmation
        #[arg(long)]
        delete_source: bool,
    },
}

#[derive(Subcommand)]
enum SeedCommand {
    /// Create a new seed file
    Create {
        seed_name: String,
        /// name of table from which seed file has to be initialized
        #[arg(long = "from-table")]
        from_tables: Vec<String>,
    },
    /// Apply seed data
    Apply {
        /// seed file to apply
        #[arg(short = 'f', long = "file")]
        files: Vec<String>,
        /// set this flag to attempt to apply seeds on all databases present on server
        #[arg(long)]
        all_databases: bool,
    },
}

#[derive(Args)]
struct DeployArgs {
    #[command(flatten)]
    server: Server,
    #[command(flatten)]
    interactive: Interactive,
    /// apply available seeds data to databases
    #[arg(long)]
    with_seeds: bool,
    #[arg(long, hide = true)]
    disable_auto_state_migration: bool,
    #[command(flatten)]
    transactions: Transactions,
}

impl Server {
    fn args(&self, interactive: Option<&Interactive>) -> ServerArgs {
        if self.access_key.is_some() {
            eprintln!("Flag --access-key has been deprecated, use --admin-secret instead");
        }
        ServerArgs {
            server: ServerFlags {
                endpoint: self.endpoint.clone(),
                admin_secret: self.admin_secret.clone(),
                access_key: self.access_key.clone(),
                insecure_skip_tls_verify: self.insecure_skip_tls_verify,
                certificate_authority: self.certificate_authority.clone(),
            },
            disable_interactive: interactive.is_some_and(|i| i.disable_interactive),
        }
    }
}

/// The message cobra returns for a command line it cannot parse.
fn cobra_message(e: &clap::Error) -> String {
    let get = |k: ContextKind| match e.get(k) {
        Some(ContextValue::String(s)) => s.clone(),
        Some(ContextValue::Strings(v)) => v.join(" "),
        _ => String::new(),
    };
    let flag_name = |s: &str| s.split(['=', ' ']).next().unwrap_or(s).to_string();
    match e.kind() {
        ErrorKind::UnknownArgument => {
            let arg = get(ContextKind::InvalidArg);
            if arg.starts_with("--") {
                format!("unknown flag: {}", flag_name(&arg))
            } else if let Some(short) = arg.strip_prefix('-') {
                format!(
                    "unknown shorthand flag: '{}' in {arg}",
                    short.chars().next().unwrap_or('-')
                )
            } else {
                format!(
                    "unknown command {} for \"hasura\"",
                    hge_cli::gofmt::quote(&arg)
                )
            }
        }
        ErrorKind::InvalidSubcommand => {
            format!(
                "unknown command {} for \"hasura\"",
                hge_cli::gofmt::quote(&get(ContextKind::InvalidSubcommand))
            )
        }
        ErrorKind::MissingRequiredArgument => {
            let missing = get(ContextKind::InvalidArg);
            if missing.starts_with("--") {
                let names: Vec<String> = missing
                    .split(' ')
                    .filter(|s| s.starts_with("--"))
                    .map(|s| format!("\"{}\"", flag_name(s.trim_start_matches('-'))))
                    .collect();
                format!("required flag(s) {} not set", names.join(", "))
            } else {
                "accepts 1 arg(s), received 0".into()
            }
        }
        ErrorKind::TooManyValues | ErrorKind::WrongNumberOfValues => {
            format!(
                "accepts at most 2 arg(s), received {}",
                get(ContextKind::ActualNumValues)
            )
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            let arg = get(ContextKind::InvalidArg);
            let value = get(ContextKind::InvalidValue);
            format!(
                "invalid argument {} for \"{}\" flag",
                hge_cli::gofmt::quote(&value),
                flag_name(&arg)
            )
        }
        _ => e
            .to_string()
            .lines()
            .next()
            .unwrap_or_default()
            .trim_start_matches("error: ")
            .to_string(),
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let cli = match Cli::try_parse_from(&raw) {
        Ok(c) => c,
        Err(e) => match e.kind() {
            ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                let _ = e.print();
                std::process::exit(0);
            }
            _ => fatal(&cobra_message(&e)),
        },
    };
    if let Err(e) = run(cli, &raw) {
        fatal(&e.0);
    }
}

fn fatal(msg: &str) -> ! {
    eprint!("{}", fatal_line(msg));
    std::process::exit(1);
}

fn run(cli: Cli, raw: &[String]) -> Result<()> {
    let flags = GlobalFlags {
        log_level: cli.log_level,
        project: cli.project,
        no_color: cli.no_color,
        envfile: cli.envfile,
        skip_update_check: cli.skip_update_check,
    };
    match cli.command {
        Command::Version => cmd::version::run(&flags),
        Command::Init(a) => {
            if a.access_key.is_some() {
                eprintln!("Flag --access-key has been deprecated, use --admin-secret instead");
            }
            if a.directory.is_some() {
                eprintln!(
                    "Flag --directory has been deprecated, use directory-name argument instead"
                );
            }
            let opts = cmd::init::InitOptions {
                version: a.version,
                dir: a.directory_name.or(a.directory),
                endpoint: a.endpoint,
                admin_secret: a.admin_secret.or(a.access_key).unwrap_or_default(),
                fetch: a.fetch,
            };
            cmd::init::run(&flags, &opts)
        }
        Command::Metadata { server, command } => {
            let ctx = cmd::validate(&flags, &server.args(None))?;
            metadata(&ctx, command, raw)
        }
        Command::Migrate {
            server,
            interactive,
            database_name,
            disable_auto_state_migration,
            command,
        } => {
            let ctx = cmd::validate(&flags, &server.args(Some(&interactive)))?;
            migrate(
                &ctx,
                command,
                database_name.as_deref(),
                disable_auto_state_migration,
            )
        }
        Command::Seed {
            server,
            interactive,
            database_name,
            command,
        } => {
            let ctx = cmd::validate(&flags, &server.args(Some(&interactive)))?;
            seed(&ctx, command, database_name.as_deref())
        }
        Command::Deploy(a) => {
            let mut ctx = cmd::validate(&flags, &a.server.args(Some(&a.interactive)))?;
            let opts = cmd::deploy::DeployOptions {
                with_seeds: a.with_seeds,
                no_transaction: a.transactions.no_transaction,
                per_migration_transaction: a.transactions.per_migration_transaction,
                disable_state_copy: a.disable_auto_state_migration,
            };
            cmd::deploy::run(&mut ctx, &opts)
        }
    }
}

fn metadata(ctx: &hge_cli::context::Ctx, command: MetadataCommand, raw: &[String]) -> Result<()> {
    use cmd::metadata as md;
    match command {
        MetadataCommand::Apply {
            from_file,
            dry_run,
            output,
            disallow_inconsistent_metadata,
        } => {
            if from_file {
                eprintln!(
                    "Flag --from-file has been deprecated, deprecation is a side effect of config v1 deprecation from v2.0.0"
                );
            }
            md::apply_cmd(
                ctx,
                &md::ApplyOptions {
                    dry_run,
                    output,
                    disallow_inconsistent: disallow_inconsistent_metadata,
                },
            )
        }
        MetadataCommand::Export { output } => md::export_cmd(ctx, &output),
        MetadataCommand::Clear => md::clear_cmd(ctx, raw.iter().any(|a| a == "reset")),
        MetadataCommand::Reload => md::reload_cmd(ctx),
        MetadataCommand::Diff { files, diff_type } => md::diff_cmd(ctx, &files, &diff_type),
        MetadataCommand::Inconsistency { command } => match command {
            InconsistencyCommand::List { output } => md::ic_list_cmd(ctx, &output),
            InconsistencyCommand::Status => md::ic_status_cmd(ctx),
            InconsistencyCommand::Drop => md::ic_drop_cmd(ctx),
        },
    }
}

fn migrate(
    ctx: &hge_cli::context::Ctx,
    command: MigrateCommand,
    database_name: Option<&str>,
    disable_state_copy: bool,
) -> Result<()> {
    use cmd::migrate as mg;
    let one = DatabaseArgs {
        name: database_name,
        all: None,
    };
    match command {
        MigrateCommand::Apply {
            up,
            down,
            goto,
            version,
            skip_execution,
            migration_type,
            dry_run,
            all_databases,
            progressbar_logs: _,
            transactions,
        } => {
            let db = cmd::choose_database_or_all(
                ctx,
                &DatabaseArgs {
                    name: database_name,
                    all: all_databases.then_some(true),
                },
            )?;
            let opts = mg::ApplyOptions {
                up,
                down,
                goto,
                version,
                migration_type,
                skip_execution,
                dry_run,
                no_transaction: transactions.no_transaction,
                per_migration_transaction: transactions.per_migration_transaction,
                disable_state_copy,
            };
            mg::apply(ctx, &db, &opts)
        }
        MigrateCommand::Status => {
            let db = cmd::choose_database_or_all(ctx, &one)?;
            mg::status(ctx, &db, disable_state_copy)
        }
        MigrateCommand::Create {
            migration_name,
            from_server,
            sql_from_file,
            sql_from_server,
            schemas,
            metadata_from_file,
            metadata_from_server,
            up_sql,
            down_sql,
        } => {
            if sql_from_server {
                eprintln!("Flag --sql-from-server has been deprecated, use --from-server instead");
            }
            if metadata_from_server {
                return Err(Error("metadata-from-server flag is deprecated".into()));
            }
            if metadata_from_file.is_some() {
                return Err(Error("metadata-from-file flag is deprecated".into()));
            }
            let db = match cmd::choose_database(ctx, &one) {
                Ok(db) => {
                    if up_sql.is_some() && down_sql.is_none() {
                        ctx.logger
                            .warn("you are creating an up migration without a down migration");
                    }
                    if down_sql.is_some() && up_sql.is_none() {
                        ctx.logger
                            .warn("you are creating a down migration without an up migration");
                    }
                    db
                }
                Err(e) if e.0.starts_with("database not found") => {
                    let name = database_name.unwrap_or_default().to_string();
                    ctx.logger
                        .warn(&format!("database '{name}' is not connected to hasura"));
                    Database {
                        name,
                        kind: "postgres".into(),
                        all: false,
                    }
                }
                Err(e) => return Err(e),
            };
            let opts = mg::CreateOptions {
                name: migration_name,
                from_server,
                sql_from_file,
                sql_from_server,
                schemas,
                exclude_schemas: Vec::new(),
                up_sql,
                down_sql,
            };
            mg::create(ctx, &db, &opts)
        }
        MigrateCommand::Delete {
            version,
            all,
            force,
            only_server,
        } => {
            ctx.logger
                .warn("[PREVIEW] this command is in preview. usage may change in future\n");
            let db = cmd::choose_database_or_all(ctx, &one)?;
            if !all && version.is_none() {
                return Err(Error(
                    "at least one flag [--all , --version] should be set".into(),
                ));
            }
            if all && version.is_some() {
                return Err(Error(
                    "only one of [--all , --version] should be set".into(),
                ));
            }
            mg::delete(
                ctx,
                &db,
                &mg::DeleteOptions {
                    version,
                    all,
                    force,
                    only_server,
                },
            )
        }
        MigrateCommand::Squash {
            from,
            to,
            name,
            delete_source,
        } => {
            let db = cmd::choose_database(ctx, &one)?;
            mg::squash(
                ctx,
                &db,
                &mg::SquashOptions {
                    from: from.unwrap_or(0),
                    to,
                    name,
                    delete_source,
                },
            )
        }
    }
}

fn seed(
    ctx: &hge_cli::context::Ctx,
    command: SeedCommand,
    database_name: Option<&str>,
) -> Result<()> {
    match command {
        SeedCommand::Create {
            seed_name,
            from_tables,
        } => {
            let db = cmd::choose_any_database(
                ctx,
                &DatabaseArgs {
                    name: database_name,
                    all: None,
                },
            )?;
            cmd::seed::check_supported(&db)?;
            cmd::seed::create(ctx, &db, &seed_name, &from_tables)
        }
        SeedCommand::Apply {
            files,
            all_databases,
        } => {
            let db = cmd::choose_database_or_all(
                ctx,
                &DatabaseArgs {
                    name: database_name,
                    all: all_databases.then_some(true),
                },
            )?;
            if !db.all {
                cmd::seed::check_supported(&db)?;
            }
            cmd::seed::apply(ctx, &db, &files)
        }
    }
}
