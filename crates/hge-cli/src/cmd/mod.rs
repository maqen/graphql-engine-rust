//! The commands. Each follows the official CLI's command of the same name: the same
//! checks in the same order, the same log lines and the same errors.

pub mod deploy;
pub mod init;
pub mod metadata;
pub mod migrate;
pub mod seed;
pub mod version;

use std::io::{BufRead, IsTerminal, Write};

use crate::config::ServerFlags;
use crate::context::{Ctx, GlobalFlags, make_logger};
use crate::error::{Error, Result};

/// Flags shared by the commands that talk to the server.
#[derive(Clone, Debug, Default)]
pub struct ServerArgs {
    pub server: ServerFlags,
    pub disable_interactive: bool,
}

/// `ec.Prepare()` + `ec.Validate()`.
pub fn validate(flags: &GlobalFlags, server: &ServerArgs) -> Result<Ctx> {
    let logger = make_logger(flags);
    let mut ctx = Ctx::validate(flags, logger, &server.server)?;
    ctx.config.disable_interactive |= server.disable_interactive;
    Ok(ctx)
}

/// The database a command works on.
#[derive(Clone, Debug, Default)]
pub struct Database {
    pub name: String,
    pub kind: String,
    pub all: bool,
}

/// Which databases a command may target and how they were named on the command line.
pub struct DatabaseArgs<'a> {
    pub name: Option<&'a str>,
    /// `Some(value)` when `--all-databases` was given.
    pub all: Option<bool>,
}

/// `name` and `kind` of each source on the server, in metadata order.
pub fn sources(ctx: &Ctx) -> Result<Vec<(String, String)>> {
    let md = ctx.client.export_metadata()?;
    Ok(md["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = s["name"].as_str().unwrap_or_default();
            let kind = s["kind"].as_str().unwrap_or_default();
            (!name.is_empty() && !kind.is_empty()).then(|| (name.to_string(), kind.to_string()))
        })
        .collect())
}

fn source_kind(ctx: &Ctx, name: &str) -> Result<Option<String>> {
    let md = ctx.client.export_metadata()?;
    Ok(md["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"].as_str() == Some(name))
        .and_then(|s| s["kind"].as_str().map(str::to_string)))
}

/// `validateSourceInfo`: look up the database's kind on the server.
pub fn source_info(ctx: &Ctx, name: &str) -> Result<String> {
    match source_kind(ctx, name) {
        Err(e) => Err(Error(format!("determining database kind of '{name}': {e}"))),
        Ok(None) => Err(Error(format!(
            "database not found: error determining database kind for '{name}', check if database exists on hasura"
        ))),
        Ok(Some(k)) => Ok(k),
    }
}

pub fn is_terminal() -> bool {
    std::io::stdout().is_terminal()
}

fn prechecks(ctx: &Ctx, db: &DatabaseArgs) -> Result<()> {
    if (!is_terminal() || ctx.config.disable_interactive) && db.all.is_none() && db.name.is_none() {
        return Err(Error("--database-name flag is required".into()));
    }
    Ok(())
}

/// `validateConfigV3Flags`: one database, chosen by flag or prompt.
pub fn choose_database(ctx: &Ctx, db: &DatabaseArgs) -> Result<Database> {
    let d = choose_any_database(ctx, db)?;
    check_migrations_supported(&d.name, &d.kind)?;
    Ok(d)
}

/// One database, chosen by flag or prompt, of any kind.
pub fn choose_any_database(ctx: &Ctx, db: &DatabaseArgs) -> Result<Database> {
    prechecks(ctx, db)?;
    let name = match db.name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => prompt_database(ctx, false)?,
    };
    let kind = source_info(ctx, &name)?;
    Ok(Database {
        name,
        kind,
        all: false,
    })
}

/// `validateConfigV3FlagsWithAll`: one database or `--all-databases`.
pub fn choose_database_or_all(ctx: &Ctx, db: &DatabaseArgs) -> Result<Database> {
    prechecks(ctx, db)?;
    if let Some(all) = db.all {
        return Ok(Database {
            name: db.name.unwrap_or_default().to_string(),
            kind: String::new(),
            all,
        });
    }
    let name = match db.name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => {
            let n = prompt_database(ctx, true)?;
            if n == ALL_DATABASES {
                return Ok(Database {
                    all: true,
                    ..Default::default()
                });
            }
            n
        }
    };
    let kind = source_info(ctx, &name)?;
    check_migrations_supported(&name, &kind)?;
    Ok(Database {
        name,
        kind,
        all: false,
    })
}

fn check_migrations_supported(name: &str, kind: &str) -> Result<()> {
    if !crate::migrate::SUPPORTED_KINDS.contains(&kind) {
        return Err(Error(format!(
            "migrations on database '{name}' of kind '{kind}' is not supported"
        )));
    }
    Ok(())
}

const ALL_DATABASES: &str = "All (all available databases)";

/// The CLI's "Select a database to use" prompt, as a numbered list.
fn prompt_database(ctx: &Ctx, with_all: bool) -> Result<String> {
    let names: Vec<String> = sources(ctx)
        .map_err(|e| Error(format!("unable to get available databases: {e}")))?
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    if names.is_empty() {
        return Err(Error("no connected databases found in the server".into()));
    }
    let mut options = names;
    if with_all {
        options.push(ALL_DATABASES.into());
    }
    select_prompt("Select a database to use", &options)
        .map_err(|e| Error(format!("error in selecting a database to use: {e}")))
}

pub fn select_prompt(question: &str, options: &[String]) -> Result<String> {
    let mut err = std::io::stderr();
    let _ = writeln!(err, "? {question}");
    for (i, o) in options.iter().enumerate() {
        let _ = writeln!(err, "  {}) {o}", i + 1);
    }
    let _ = write!(err, "Choose 1-{}: ", options.len());
    let _ = err.flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let n: usize = line
        .trim()
        .parse()
        .map_err(|_| Error("invalid choice".into()))?;
    options
        .get(n.wrapping_sub(1))
        .cloned()
        .ok_or_else(|| Error("invalid choice".into()))
}

pub fn yes_no_prompt(question: &str) -> Result<bool> {
    let mut err = std::io::stderr();
    let _ = write!(err, "? {question} (y/N) ");
    let _ = err.flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Milliseconds since the epoch: migration and seed versions.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
