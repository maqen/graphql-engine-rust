//! `hasura migrate …`: apply, status, create, delete and squash.

use std::path::{Path, PathBuf};

use crate::cmd::{Database, now_millis, sources};
use crate::context::{Ctx, io_message};
use crate::error::{Error, Result};
use crate::logger::{Field, Level};
use crate::migrate::{Migrate, MigrateError, Status};

// --- apply

#[derive(Clone, Default)]
pub struct ApplyOptions {
    pub up: String,
    pub down: String,
    pub goto: String,
    pub version: String,
    pub migration_type: String,
    pub skip_execution: bool,
    pub dry_run: bool,
    pub no_transaction: bool,
    pub per_migration_transaction: bool,
    pub disable_state_copy: bool,
}

/// `MigrateApplyOptions.Run`: apply, log each database's outcome, fail if any failed.
pub fn apply(ctx: &Ctx, db: &Database, opts: &ApplyOptions) -> Result<()> {
    let results = apply_results(ctx, db, opts)?;
    let mut failed = Vec::new();
    for (name, r) in results {
        match r {
            Err(e) => {
                failed.push(name);
                ctx.logger.error(&e.0);
            }
            Ok(msg) if !msg.is_empty() => ctx.logger.info(&msg),
            Ok(_) => {}
        }
    }
    if !failed.is_empty() {
        return Err(Error(format!(
            "applying migrations failed on database(s): {}",
            failed.join(",")
        )));
    }
    Ok(())
}

type ApplyResult = (String, std::result::Result<String, Error>);

fn apply_results(ctx: &Ctx, db: &Database, opts: &ApplyOptions) -> Result<Vec<ApplyResult>> {
    if opts.no_transaction && opts.per_migration_transaction {
        return Err(Error(
            "--no-transaction and --per-migration-transaction are mutually exclusive".into(),
        ));
    }
    if opts.dry_run && opts.skip_execution {
        return Err(Error(
            "both --skip-execution and --dry-run flags cannot be used together".into(),
        ));
    }
    if opts.dry_run && db.all {
        return Err(Error(
            "both --all-databases and --dry-run flags cannot be used together".into(),
        ));
    }
    let mut db = db.clone();
    if !db.all {
        if db.name.is_empty() {
            return Err(Error(
                "unable to determine database on which migration should be applied".into(),
            ));
        }
        if db.kind.is_empty() {
            db.kind = match crate::cmd::source_info(ctx, &db.name) {
                Ok(k) => k,
                Err(e) if e.0.starts_with("database not found: ") => {
                    return Err(Error(
                        e.0.trim_start_matches("database not found: ").to_string(),
                    ));
                }
                Err(e) => return Err(e),
            };
        }
    }
    if db.all {
        ctx.logger.spin("getting lists of databases from server ");
        let list = sources(ctx);
        ctx.logger.stop_spinner();
        let mut out = Vec::new();
        for (name, kind) in list? {
            let d = Database {
                name: name.clone(),
                kind,
                all: true,
            };
            let r = match exec(ctx, &d, opts) {
                Ok(()) => Ok(format!("migrations applied on database: {name}")),
                Err(e) => handle_error(&name, e),
            };
            out.push((name, r));
        }
        return Ok(out);
    }
    let r = match exec(ctx, &db, opts) {
        Ok(()) => Ok("migrations applied".to_string()),
        Err(e) => handle_error(&db.name, e),
    };
    Ok(vec![(db.name.clone(), r)])
}

enum ExecError {
    DirNotFound(String),
    Migrate(MigrateError),
}

impl From<Error> for ExecError {
    fn from(e: Error) -> Self {
        ExecError::Migrate(MigrateError::Other(e.0))
    }
}

impl From<MigrateError> for ExecError {
    fn from(e: MigrateError) -> Self {
        ExecError::Migrate(e)
    }
}

fn handle_error(name: &str, e: ExecError) -> std::result::Result<String, Error> {
    match e {
        ExecError::Migrate(MigrateError::NoChange | MigrateError::NoMigrations(_)) => {
            Ok(format!("nothing to apply on database: {name}"))
        }
        ExecError::DirNotFound(msg) | ExecError::Migrate(MigrateError::Other(msg)) => Err(Error(
            format!("skipping applying migrations on database '{name}', encountered: \n{msg}"),
        )),
    }
}

fn migration_dir_check(ctx: &Ctx, name: &str) -> std::result::Result<(), ExecError> {
    let dir = ctx.migrations_dir.join(name);
    if let Err(e) = std::fs::metadata(&dir) {
        return Err(ExecError::DirNotFound(format!(
            "expected to find a migrations directory for database '{name}' in {}, but encountered error: stat {}: {}",
            ctx.migrations_dir.display(),
            dir.display(),
            io_message(&e)
        )));
    }
    Ok(())
}

fn exec(ctx: &Ctx, db: &Database, opts: &ApplyOptions) -> std::result::Result<(), ExecError> {
    migration_dir_check(ctx, &db.name)?;
    if db.all && (!opts.goto.is_empty() || !opts.version.is_empty()) {
        return Err(Error(
            "cannot use --goto or --version in conjunction with --all-databases".into(),
        )
        .into());
    }
    let (kind, step) =
        migration_type_and_step(opts).map_err(|e| Error(format!("error validating flags: {e}")))?;
    let mut m = Migrate::new(
        &ctx.client,
        &ctx.logger,
        &ctx.migrations_dir,
        &db.name,
        &db.kind,
        opts.disable_state_copy,
    )?;
    m.skip_execution = opts.skip_execution;
    m.dry_run = opts.dry_run;
    m.no_transaction = opts.no_transaction;
    m.per_migration_transaction = opts.per_migration_transaction;
    match kind {
        "up" => m.up(step)?,
        "down" => m.down(step)?,
        "gotoVersion" => m.goto(step)?,
        _ => {
            if step >= 0 {
                m.version(step as u64, true)?
            } else {
                m.version(step.unsigned_abs(), false)?
            }
        }
    }
    Ok(())
}

/// `getMigrationTypeAndStep`.
fn migration_type_and_step(o: &ApplyOptions) -> Result<(&'static str, i64)> {
    let mut count = 0;
    let mut step = "all".to_string();
    let mut name = "up";
    if !o.up.is_empty() {
        step = o.up.clone();
        count += 1;
    }
    if !o.down.is_empty() {
        name = "down";
        step = o.down.clone();
        count += 1;
    }
    if !o.version.is_empty() {
        name = "version";
        step = if o.migration_type == "down" {
            format!("-{}", o.version)
        } else {
            o.version.clone()
        };
        count += 1;
    }
    if !o.goto.is_empty() {
        name = "gotoVersion";
        step = o.goto.clone();
        count += 1;
    }
    if count > 1 {
        return Err(Error(
            "only one migration type can be applied at a time (--up, --down, --type or --goto)"
                .into(),
        ));
    }
    if o.skip_execution && !matches!(name, "version" | "up" | "down") {
        return Err(Error(
            "--skip-execution flag can be set only with --version, --up, --down flags".into(),
        ));
    }
    if step == "all" && name != "version" {
        return Ok((name, -1));
    }
    let n: i64 = step.parse().map_err(|_| {
        Error(format!(
            "not a valid input for steps/version: strconv.ParseInt: parsing {}: invalid syntax",
            crate::gofmt::quote(&step)
        ))
    })?;
    Ok((name, n))
}

// --- status

pub fn status(ctx: &Ctx, db: &Database, disable_state_copy: bool) -> Result<()> {
    ctx.logger.spin("Fetching migration status...");
    let r = status_run(ctx, db, disable_state_copy);
    ctx.logger.stop_spinner();
    let statuses = r?;
    let mut text = String::new();
    for (name, status) in &statuses {
        if !name.is_empty() {
            text.push_str(&format!("\nDatabase: {name}\n"));
        }
        text.push_str("VERSION\tNAME\tSOURCE STATUS\tDATABASE STATUS\n");
        for s in status.values() {
            let b = |x: bool| if x { "Present" } else { "Not Present" };
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                s.version,
                s.name,
                b(s.is_present),
                b(s.is_applied)
            ));
        }
    }
    print!("{}", crate::table::tabwrite(&text));
    Ok(())
}

fn status_run(ctx: &Ctx, db: &Database, disable_state_copy: bool) -> Result<Vec<(String, Status)>> {
    let one = |name: &str, kind: &str| -> Result<Status> {
        let m = Migrate::new(
            &ctx.client,
            &ctx.logger,
            &ctx.migrations_dir,
            name,
            kind,
            disable_state_copy,
        )
        .map_err(|e| Error(format!("error getting status for database '{name}': {e}")))?;
        Ok(m.status())
    };
    if db.all {
        let list = sources(ctx)
            .map_err(|e| Error(format!("got error while getting the sources list : {e}")))?;
        return list
            .into_iter()
            .map(|(n, k)| one(&n, &k).map(|s| (n, s)))
            .collect();
    }
    Ok(vec![(db.name.clone(), one(&db.name, &db.kind)?)])
}

// --- create

#[derive(Default)]
pub struct CreateOptions {
    pub name: String,
    pub from_server: bool,
    pub sql_from_file: String,
    pub sql_from_server: bool,
    pub schemas: Vec<String>,
    pub exclude_schemas: Vec<String>,
    pub up_sql: Option<String>,
    pub down_sql: Option<String>,
}

pub fn create(ctx: &Ctx, db: &Database, opts: &CreateOptions) -> Result<()> {
    ctx.logger.spin("Creating migration files...");
    let r = create_run(ctx, db, opts);
    ctx.logger.stop_spinner();
    let version = r?;
    ctx.logger.log_fields(
        Level::Info,
        "Migrations files created",
        &[
            ("name", Field::Str(&opts.name)),
            ("version", Field::Int(version)),
        ],
    );
    Ok(())
}

/// Write `<dir>/<version>_<name>/{up,down}.sql` (`CreateOptions.Create`).
fn create_files(
    dir: &Path,
    version: i64,
    name: &str,
    up: Option<&str>,
    down: Option<&str>,
) -> Result<()> {
    let path = dir.join(format!("{version}_{name}"));
    std::fs::create_dir_all(&path)?;
    if up.is_none() && down.is_none() {
        return Err(Error("none of the files has been set with data".into()));
    }
    if let Some(up) = up {
        std::fs::write(path.join("up.sql"), up)?;
    }
    if let Some(down) = down {
        std::fs::write(path.join("down.sql"), down)?;
    }
    Ok(())
}

/// Remove `<version>_*` entries (`CreateOptions.Delete`).
fn delete_version(dir: &Path, version: &str) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if e.file_name()
            .to_string_lossy()
            .starts_with(&format!("{version}_"))
        {
            let p = e.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p)?;
                return Ok(());
            }
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

pub fn create_run(ctx: &Ctx, db: &Database, opts: &CreateOptions) -> Result<i64> {
    let version = now_millis();
    let dir = ctx.migrations_dir.join(&db.name);
    let sql_server = opts.from_server || opts.sql_from_server;
    let m = if sql_server || opts.up_sql.is_some() || opts.down_sql.is_some() {
        Some(
            Migrate::new(
                &ctx.client,
                &ctx.logger,
                &ctx.migrations_dir,
                &db.name,
                &db.kind,
                true,
            )
            .map_err(|e| Error(format!("cannot create migrate instance: {e}")))?,
        )
    } else {
        None
    };
    let _ = &m;
    let mut up: Option<String> = None;
    let mut down: Option<String> = None;
    if !opts.sql_from_file.is_empty() {
        let data = std::fs::read_to_string(&opts.sql_from_file).map_err(|e| {
            Error(format!(
                "cannot set sql file: open {}: {}",
                opts.sql_from_file,
                io_message(&e)
            ))
        })?;
        up = Some(data);
    }
    if sql_server {
        if db.kind != "postgres" {
            return Err(Error(format!(
                "cannot fetch schema dump: schema dump for source {} of kind {} is not supported",
                db.name, db.kind
            )));
        }
        let mut args: Vec<String> = vec!["-O".into(), "-x".into(), "--schema-only".into()];
        for s in &opts.schemas {
            args.push("--schema".into());
            args.push(s.clone());
        }
        for s in &opts.exclude_schemas {
            args.push("--exclude-schema".into());
            args.push(s.clone());
        }
        let dump = ctx
            .client
            .pg_dump(&args, &db.name)
            .map_err(|e| Error(format!("cannot fetch schema dump: {e}")))?;
        up = Some(dump);
    }
    if let Some(s) = &opts.up_sql {
        up = Some(s.clone());
    }
    if let Some(s) = &opts.down_sql {
        down = Some(s.clone());
    }
    if opts.sql_from_file.is_empty() && !sql_server {
        up.get_or_insert_with(String::new);
        down.get_or_insert_with(String::new);
    }
    if let Err(e) = create_files(&dir, version, &opts.name, up.as_deref(), down.as_deref()) {
        if let Err(d) = delete_version(&dir, &version.to_string()) {
            ctx.logger
                .warn(&format!("cannot delete dangling migrations: {d}"));
        }
        return Err(Error(format!("error creating migration files: {e}")));
    }
    ctx.logger.stop_spinner();
    ctx.logger.info("Created Migrations");
    if opts.from_server {
        let apply_opts = ApplyOptions {
            version: version.to_string(),
            skip_execution: true,
            migration_type: "up".into(),
            ..Default::default()
        };
        if let Err(e) = apply(ctx, db, &apply_opts) {
            ctx.logger.warn(&format!(
                "cannot mark created migration {version} as applied: {e}"
            ));
            ctx.logger.warn(&format!(
                "manually mark it as applied using command:  hasura migrate apply --skip-execution --version {version}"
            ));
        }
    }
    Ok(version)
}

// --- delete

pub struct DeleteOptions {
    pub version: Option<u64>,
    pub all: bool,
    pub force: bool,
    pub only_server: bool,
}

pub fn delete(ctx: &Ctx, db: &Database, opts: &DeleteOptions) -> Result<()> {
    if opts.all && !opts.force && crate::cmd::is_terminal() {
        let ok = crate::cmd::yes_no_prompt(
            "clear all migrations of database and it's history on the server?",
        )
        .map_err(|e| Error(format!("error getting user input: {e}")))?;
        if !ok {
            return Ok(());
        }
    }
    if db.all && !opts.force {
        let ok = crate::cmd::yes_no_prompt(
            "clear all mentioned migrations of all databases and it's history on the server?",
        )
        .map_err(|e| Error(format!("error getting user input: {e}")))?;
        if !ok {
            return Ok(());
        }
    }
    ctx.logger.spin("Removing migrations");
    let r = delete_run(ctx, db, opts);
    ctx.logger.stop_spinner();
    r.map_err(|e| Error(format!("operation failed: {e}")))
}

fn delete_run(ctx: &Ctx, db: &Database, opts: &DeleteOptions) -> Result<()> {
    if db.all {
        let list = sources(ctx)
            .map_err(|e| Error(format!("got error while getting the sources list : {e}")))?;
        for (name, kind) in list {
            delete_on_source(ctx, &name, &kind, opts).map_err(|e| {
                Error(format!(
                    "error while deleting status for database '{name}': {e}"
                ))
            })?;
        }
        return Ok(());
    }
    delete_on_source(ctx, &db.name, &db.kind, opts)
}

fn delete_on_source(ctx: &Ctx, name: &str, kind: &str, opts: &DeleteOptions) -> Result<()> {
    ctx.logger.spin("Deleting migration...");
    let mut m = Migrate::new(
        &ctx.client,
        &ctx.logger,
        &ctx.migrations_dir,
        name,
        kind,
        false,
    )
    .map_err(|e| Error(format!("error in creation of new migrate instance {e}")))?;
    let status = m.status();
    let (source_versions, server_versions): (Vec<u64>, Vec<u64>) = if !opts.all {
        let v = opts.version.unwrap_or(0);
        if !status.contains_key(&v) {
            return Err(Error(format!("version {v} not found")));
        }
        (vec![v], vec![v])
    } else {
        (
            status
                .values()
                .filter(|s| s.is_present)
                .map(|s| s.version)
                .collect(),
            status
                .values()
                .filter(|s| s.is_applied)
                .map(|s| s.version)
                .collect(),
        )
    };
    m.remove_versions(&server_versions)
        .map_err(|e| Error(format!("error removing migration from server: {e}")))?;
    if !opts.only_server {
        let dir = ctx.migrations_dir.join(name);
        for v in source_versions {
            delete_version(&dir, &v.to_string()).map_err(|e| {
                Error(format!(
                    "error removing migrations from project: unable to delete migrations from project for: {v} : {e}"
                ))
            })?;
        }
    }
    ctx.logger.stop_spinner();
    ctx.logger.info("Deleted migrations");
    Ok(())
}

// --- squash

pub struct SquashOptions {
    pub from: u64,
    pub to: i64,
    pub name: String,
    pub delete_source: bool,
}

pub fn squash(ctx: &Ctx, db: &Database, opts: &SquashOptions) -> Result<()> {
    ctx.logger.warn("This command is currently experimental and hence in preview, correctness of squashed migration is not guaranteed!");
    ctx.logger.spin(&format!(
        "Squashing migrations from {} to latest...",
        opts.from
    ));
    let r = squash_run(ctx, db, opts);
    ctx.logger.stop_spinner();
    r
}

fn squash_run(ctx: &Ctx, db: &Database, opts: &SquashOptions) -> Result<()> {
    let new_version = now_millis();
    let mut m = Migrate::new(
        &ctx.client,
        &ctx.logger,
        &ctx.migrations_dir,
        &db.name,
        &db.kind,
        false,
    )
    .map_err(|e| Error(format!("unable to initialize migrations driver: {e}")))?;
    let status = m.status();
    let Some(from) = status.get(&opts.from) else {
        return Err(Error(format!(
            "validating 'from' migration failed. Make sure migration with version {} exists",
            opts.from
        )));
    };
    let to = if opts.to == -1 {
        status
            .values()
            .next_back()
            .cloned()
            .ok_or_else(|| Error("no migrations".into()))?
    } else {
        if opts.from as i64 > opts.to {
            return Err(Error(format!(
                "cannot squash from {0} to {1}: {0} (from) should be less than {1} (to)",
                opts.from, opts.to
            )));
        }
        status.get(&(opts.to as u64)).cloned().ok_or_else(|| {
            Error(format!(
                "validating 'to' migration failed. Make sure migration with version {} exists",
                opts.to
            ))
        })?
    };
    // validateMigrations: all applied or all unapplied, and all present.
    let prev_applied = from.is_applied;
    for s in status.range(from.version + 1..=to.version).map(|(_, s)| s) {
        if s.is_applied != prev_applied || !s.is_present {
            return Err(Error(format!(
                "migrations are out of sync. all migrations selected to squash should be applied or all should be unapplied. found first mismatch at {}. use 'hasura migrate status' to inspect",
                s.version
            )));
        }
    }
    let dir = ctx.migrations_dir.join(&db.name);
    let (versions, up, down) = m
        .squash(opts.from, opts.to)
        .map_err(|e| Error(format!("unable to squash migrations: {e}")))?;
    let (Some(first), Some(last)) = (versions.first().copied(), versions.last().copied()) else {
        return Err(Error(
            "unable to squash migrations: no migrations to squash".into(),
        ));
    };
    create_files(
        &dir,
        new_version,
        &opts.name,
        up.as_deref(),
        down.as_deref(),
    )
    .map_err(|e| {
        Error(format!(
            "unable to squash migrations: cannot create migration: {e}"
        ))
    })?;
    ctx.logger.stop_spinner();
    let destination = dir.join(format!("squashed_{first}_to_{last}"));
    std::fs::create_dir_all(&destination).map_err(|e| {
        Error(format!(
            "creating directory to move squashed migrations: {e}"
        ))
    })?;
    for v in &versions {
        move_version(&dir, &v.to_string(), &destination).map_err(|e| {
            Error(format!(
                "moving squashed migrations: unable to move migrations from project for: {v} : {e}"
            ))
        })?;
    }
    let old_path = dir.join(format!("{new_version}_{}", opts.name));
    let new_path = dir.join(format!("{}_{}", to.version, opts.name));
    std::fs::rename(&old_path, &new_path).map_err(|e| {
        Error(format!(
            "renaming squashed migrations: rename {} {}: {}",
            old_path.display(),
            new_path.display(),
            io_message(&e)
        ))
    })?;
    ctx.logger.info(&format!(
        "Created '{}_{}' after squashing '{first}' till '{last}'",
        to.version, opts.name
    ));
    let mut delete_source = opts.delete_source;
    if !delete_source && crate::cmd::is_terminal() {
        ctx.logger
            .info("The following migrations are squashed into a new one:");
        let list: String = versions.iter().map(|v| format!("{v}\n")).collect();
        println!("{list}");
        delete_source = crate::cmd::yes_no_prompt(&format!(
            "migrations which were squashed is moved to {}. Delete them permanently?",
            destination
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        ))
        .unwrap_or(false);
    }
    if delete_source {
        if let Err(e) = std::fs::remove_dir_all(&destination) {
            ctx.logger.error(&format!(
                "deleting directory {} failed: {e}",
                destination.display()
            ));
        }
        if let Err(e) = m.remove_versions(&versions[..versions.len() - 1]) {
            ctx.logger.error(&format!(
                "removing squashed migration state from server failed: {e}"
            ));
        }
    }
    Ok(())
}

fn move_version(dir: &Path, version: &str, destination: &Path) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with(&format!("{version}_")) && e.path().is_dir() {
            let to: PathBuf = destination.join(&name);
            std::fs::rename(e.path(), to)?;
            return Ok(());
        }
    }
    Ok(())
}
