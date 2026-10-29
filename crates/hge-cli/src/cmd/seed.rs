//! `hasura seed …`: create and apply seed files.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::cmd::{Database, now_millis, sources};
use crate::context::{Ctx, io_message};
use crate::error::{Error, Result};
use crate::logger::{Field, Level};

pub const SUPPORTED_KINDS: &[&str] = &["mssql", "postgres", "citus"];

pub fn check_supported(db: &Database) -> Result<()> {
    if !SUPPORTED_KINDS.contains(&db.kind.as_str()) {
        return Err(Error(format!(
            "seed operations on database '{}' of kind '{}' is not supported",
            db.name, db.kind
        )));
    }
    Ok(())
}

// --- create

pub fn create(ctx: &Ctx, db: &Database, name: &str, from_tables: &[String]) -> Result<()> {
    let dir = ctx.seeds_dir.join(&db.name);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    let body = if !from_tables.is_empty() {
        if db.kind != "postgres" {
            return Err(Error(
                "--from-table is supported only for postgres databases".into(),
            ));
        }
        export_data(ctx, from_tables, &db.name)
            .map_err(|e| Error(format!("exporting seed data: {e}")))?
    } else {
        capture_from_editor()
            .map_err(|e| Error(format!("cannot find default editor from env: {e}")))?
    };
    let path = dir.join(format!("{}_{name}.sql", now_millis()));
    write_seed(&path, body.as_bytes())
        .map_err(|e| Error(format!("failed to create seed file: {e}")))?;
    ctx.logger.log_fields(
        Level::Info,
        "created seed file successfully",
        &[("file", Field::Str(&path.display().to_string()))],
    );
    Ok(())
}

/// `os.OpenFile(O_WRONLY|O_CREATE)`: an existing file is overwritten from the start, not
/// truncated.
fn write_seed(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| Error(format!("open {}: {}", path.display(), io_message(&e))))?;
    f.write_all(data)?;
    Ok(())
}

/// `pg_dump --data-only --column-inserts` of the given tables.
fn export_data(ctx: &Ctx, tables: &[String], source: &str) -> Result<String> {
    let mut opts: Vec<String> = vec![
        "--no-owner".into(),
        "--no-acl".into(),
        "--data-only".into(),
        "--column-inserts".into(),
    ];
    for t in tables {
        let parts: Vec<&str> = t.split('.').collect();
        let quoted = match parts.as_slice() {
            [table] => format!("\"{table}\""),
            [schema, table] => format!("\"{schema}\".\"{table}\""),
            _ => return Err(Error(format!("invalid schema/table provided \"{t}\""))),
        };
        opts.push("--table".into());
        opts.push(quoted);
    }
    ctx.client.pg_dump(&opts, source)
}

/// Open `$EDITOR` (or `$VISUAL`, or `vi`) on an empty `.sql` file and return what was saved.
fn capture_from_editor() -> Result<String> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".into());
    let file = std::env::temp_dir().join(format!("hasura-seed-{}.sql", now_millis()));
    std::fs::write(&file, b"")?;
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(&file)
        .status()?;
    if !status.success() {
        return Err(Error(format!("exit status {}", status.code().unwrap_or(1))));
    }
    let text = std::fs::read_to_string(&file)?;
    let _ = std::fs::remove_file(&file);
    Ok(text)
}

// --- apply

pub fn apply(ctx: &Ctx, db: &Database, files: &[String]) -> Result<()> {
    ctx.logger.spin("Applying seeds...");
    let r = apply_run(ctx, db, files);
    ctx.logger.stop_spinner();
    r.map_err(|e| Error(format!("operation failed \n{e}")))
}

fn apply_run(ctx: &Ctx, db: &Database, files: &[String]) -> Result<()> {
    if db.all {
        let list = sources(ctx)
            .map_err(|e| Error(format!("got error while getting the sources list : {e}")))?;
        for (name, kind) in list {
            let d = Database {
                name: name.clone(),
                kind,
                all: false,
            };
            match apply_on_source(ctx, &d, files) {
                Ok(()) => ctx
                    .logger
                    .info(&format!("Seed data planted for database: {name}")),
                Err(SeedError::NotFound(_)) if files.is_empty() => ctx
                    .logger
                    .info(&format!("No seed data to plant for database: {name}")),
                Err(e) => {
                    return Err(Error(format!(
                        "error while applying seeds for database '{name}': {e}"
                    )));
                }
            }
        }
        return Ok(());
    }
    apply_on_source(ctx, db, files).map_err(|e| Error(e.to_string()))?;
    ctx.logger.info("Seeds planted");
    Ok(())
}

enum SeedError {
    NotFound(String),
    Other(String),
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedError::NotFound(s) | SeedError::Other(s) => f.write_str(s),
        }
    }
}

fn has_sql_extension(name: &str) -> bool {
    matches!(
        Path::new(name).extension().and_then(|e| e.to_str()),
        Some("sql" | "SQL")
    )
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out)?;
        } else if has_sql_extension(&e.file_name().to_string_lossy()) {
            out.push(p);
        }
    }
    Ok(())
}

fn apply_on_source(
    ctx: &Ctx,
    db: &Database,
    files: &[String],
) -> std::result::Result<(), SeedError> {
    let dir = if db.name.is_empty() {
        ctx.seeds_dir.clone()
    } else {
        ctx.seeds_dir.join(&db.name)
    };
    let mut sqls = Vec::new();
    if !files.is_empty() {
        for f in files {
            let path = dir.join(f);
            if !has_sql_extension(&path.to_string_lossy()) {
                let ext = path
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                return Err(SeedError::Other(format!(
                    "expected extension to be one of [.sql .SQL] but got {ext} on file {}",
                    path.display()
                )));
            }
            let text = std::fs::read_to_string(&path).map_err(|e| {
                let msg = format!(
                    "error opening file: open {}: {}",
                    path.display(),
                    io_message(&e)
                );
                if e.kind() == std::io::ErrorKind::NotFound {
                    SeedError::NotFound(msg)
                } else {
                    SeedError::Other(msg)
                }
            })?;
            sqls.push(text);
        }
    } else {
        if let Err(e) = std::fs::symlink_metadata(&dir) {
            let msg = format!(
                "error walking the directory path: lstat {}: {}",
                dir.display(),
                io_message(&e)
            );
            return Err(if e.kind() == std::io::ErrorKind::NotFound {
                SeedError::NotFound(msg)
            } else {
                SeedError::Other(msg)
            });
        }
        let mut paths = Vec::new();
        walk(&dir, &mut paths)
            .map_err(|e| SeedError::Other(format!("error walking the directory path: {e}")))?;
        for p in paths {
            let text = std::fs::read_to_string(&p).map_err(|e| {
                SeedError::Other(format!(
                    "error walking the directory path: error opening file: {e}"
                ))
            })?;
            sqls.push(text);
        }
    }
    let kind = if db.name.is_empty() {
        "postgres"
    } else {
        db.kind.as_str()
    };
    let request = match kind {
        "postgres" => "run_sql",
        "mssql" => "mssql_run_sql",
        "citus" => "citus_run_sql",
        _ => {
            return Err(SeedError::Other(format!(
                "database {} of kind {} is not supported",
                db.name, db.kind
            )));
        }
    };
    let args: Vec<Value> = sqls
        .iter()
        .map(|sql| json!({"type": request, "args": {"sql": sql, "source": db.name}}))
        .collect();
    if args.is_empty() {
        return Err(SeedError::Other(format!(
            "no SQL files found in {}",
            dir.display()
        )));
    }
    let r = ctx
        .client
        .v2_query(&json!({"type": "bulk", "args": args}))
        .map_err(|e| SeedError::Other(e.0))?;
    if !r.ok() {
        return Err(SeedError::Other(r.body));
    }
    Ok(())
}
