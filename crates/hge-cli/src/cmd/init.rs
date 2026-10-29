//! `hasura init`: create a project directory, and fill it from a server when `--endpoint`
//! is given.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::cmd::{Database, is_terminal, migrate, validate, yes_no_prompt};
use crate::context::{GlobalFlags, make_logger};
use crate::error::{Error, Result};
use crate::logger::Logger;
use crate::yaml::Node;

pub struct InitOptions {
    pub version: i64,
    pub dir: Option<String>,
    pub endpoint: String,
    pub admin_secret: String,
    pub fetch: bool,
}

const DEFAULT_DIRECTORY: &str = "hasura";
const DEFAULT_ENDPOINT: &str = "http://localhost:8080";

pub fn run(flags: &GlobalFlags, opts: &InitOptions) -> Result<()> {
    if opts.version <= 1 {
        return Err(Error(
            "config v1 is deprecated, please consider using config v3".into(),
        ));
    }
    let logger = make_logger(flags);
    let mut fetch = opts.fetch;
    let exec_dir = match create_project(flags, &logger, opts, &mut fetch) {
        Ok(d) => d,
        Err(e) => {
            logger.error("initializing project directory failed");
            return Err(Error(format!("operation failed: {e}")));
        }
    };
    let mut shown_dir = exec_dir.display().to_string();
    if !opts.endpoint.is_empty() && fetch {
        let mut project_flags = flags.clone();
        project_flags.project = Some(exec_dir.display().to_string());
        match validate(&project_flags, &Default::default()) {
            Err(e) => logger.error(&format!("validating server failed: {e}")),
            Ok(ctx) => {
                shown_dir = ctx.dir.display().to_string();
                ctx.logger.spin("Exporting metadata...");
                let r = crate::cmd::metadata::export_files(&ctx)
                    .and_then(|files| crate::cmd::metadata::write_files(&ctx.metadata_dir, &files));
                ctx.logger.stop_spinner();
                match r {
                    Ok(()) => ctx.logger.info("Metadata exported"),
                    Err(e) => ctx.logger.error(&format!(
                        "exporting metadata failed: \n{e}\nrun `hasura metadata export` from your project directory to retry"
                    )),
                }
                if let Err(e) = create_migrations(&ctx) {
                    ctx.logger.error(&format!(
                        "creating migrations failed: \n{e}\nrun `hasura migrate create --from-server` from your project directory to retry"
                    ));
                }
            }
        }
    }
    let cwd = std::env::current_dir()?;
    let msg = if exec_dir != cwd {
        format!(
            "directory created. execute the following commands to continue:\n\n  cd {shown_dir}\n  hasura console\n"
        )
    } else {
        "hasura project initialised. execute the following command to continue:\n\t\thasura console\n\t  ".to_string()
    };
    logger.info(&msg);
    Ok(())
}

fn create_migrations(ctx: &crate::context::Ctx) -> Result<()> {
    let sources = crate::cmd::sources(ctx)?;
    let mut errs = Vec::new();
    for (name, kind) in sources {
        ctx.logger
            .info(&format!("Creating migrations for source: {name}"));
        let db = Database {
            name: name.clone(),
            kind,
            all: false,
        };
        let opts = migrate::CreateOptions {
            name: "init".into(),
            from_server: true,
            exclude_schemas: vec!["hdb_catalog".into(), "hdb_views".into()],
            ..Default::default()
        };
        if let Err(e) = migrate::create_run(ctx, &db, &opts) {
            errs.push(format!("applying migrations on source: {name}: {e}"));
        }
    }
    if !errs.is_empty() {
        return Err(Error(errs.join("\n")));
    }
    Ok(())
}

fn prompt_with_default(question: &str, default: &str) -> Result<String> {
    let mut err = std::io::stderr();
    let _ = write!(err, "? {question} ({default}) ");
    let _ = err.flush();
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line)?;
    if n == 0 {
        return Err(Error("EOF".into()));
    }
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn create_project(
    flags: &GlobalFlags,
    logger: &Logger,
    opts: &InitOptions,
    fetch: &mut bool,
) -> Result<PathBuf> {
    let init_dir = match &opts.dir {
        Some(d) if !d.is_empty() => d.clone(),
        _ => {
            let r = prompt_with_default("Name of project directory ?", DEFAULT_DIRECTORY)
                .map_err(|e| Error(format!("prompt exited: {e}")))?;
            if r.trim().is_empty() {
                DEFAULT_DIRECTORY.to_string()
            } else {
                r
            }
        }
    };
    if !opts.endpoint.is_empty() && !*fetch && is_terminal() {
        *fetch = yes_no_prompt(&format!(
            "Initialize project with metadata & migrations from {} ?",
            opts.endpoint
        ))
        .map_err(|e| Error(format!("prompt exited: {e}")))?;
    }
    if !is_terminal() {
        *fetch = true;
    }
    let cwd = std::env::current_dir()
        .map_err(|e| Error(format!("error getting current working directory: {e}")))?;
    let init_path = cwd.join(&init_dir);
    let exec_dir = if normalize(&init_path) == cwd {
        if cwd.parent().is_none() {
            return Err(Error(
                "can't initialise hasura project in filesystem root".into(),
            ));
        }
        if cwd.join("config.yaml").is_file() {
            return Err(Error(
                "current working directory is already a hasura project directory".into(),
            ));
        }
        cwd.clone()
    } else {
        let dir = match &flags.project {
            Some(p) if !p.is_empty() => Path::new(p).join(&init_dir),
            _ => PathBuf::from(&init_dir),
        };
        if dir.exists() {
            return Err(Error(format!(
                "directory '{}' already exists",
                dir.display()
            )));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error(format!("error creating setup directories: {e}")))?;
        dir
    };
    create_files(&exec_dir, opts)?;
    logger.debug("Creating project directory");
    Ok(exec_dir)
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The files `createFiles` writes: config.yaml and the empty metadata, migrations and
/// seeds directories.
fn create_files(dir: &Path, opts: &InitOptions) -> Result<()> {
    let endpoint = if opts.endpoint.is_empty() {
        DEFAULT_ENDPOINT.to_string()
    } else {
        opts.endpoint.clone()
    };
    if !opts.endpoint.is_empty() && !is_request_uri(&opts.endpoint) {
        return Err(Error(format!(
            "error validating endpoint URL: parse {}: invalid URI for request",
            crate::gofmt::quote(&opts.endpoint)
        )));
    }
    let mut config = vec![(Node::string("endpoint"), Node::string(endpoint))];
    if !opts.admin_secret.is_empty() {
        config.push((
            Node::string("admin_secret"),
            Node::string(opts.admin_secret.clone()),
        ));
    }
    config.push((Node::string("version"), Node::raw(opts.version.to_string())));
    config.push((Node::string("metadata_directory"), Node::string("metadata")));
    config.push((
        Node::string("actions"),
        Node::Map(vec![
            (Node::string("kind"), Node::string("synchronous")),
            (
                Node::string("handler_webhook_baseurl"),
                Node::string("http://localhost:3000"),
            ),
        ]),
    ));
    std::fs::write(dir.join("config.yaml"), Node::Map(config).to_yaml())
        .map_err(|e| Error(format!("cannot write config file: {e}")))?;
    std::fs::create_dir_all(dir.join("migrations"))?;
    let md = dir.join("metadata");
    std::fs::create_dir_all(&md)?;
    for f in [
        "query_collections.yaml",
        "allow_list.yaml",
        "remote_schemas.yaml",
    ] {
        std::fs::write(md.join(f), "[]\n")?;
    }
    std::fs::write(
        md.join("actions.yaml"),
        "actions: []\ncustom_types:\n    enums: []\n    input_objects: []\n    objects: []\n    scalars: []\n",
    )?;
    std::fs::write(md.join("actions.graphql"), "")?;
    std::fs::write(md.join("cron_triggers.yaml"), "[]\n")?;
    if opts.version == 3 {
        std::fs::write(md.join("version.yaml"), "version: 3\n")?;
        std::fs::create_dir_all(md.join("databases"))?;
        std::fs::write(md.join("databases/databases.yaml"), "[]\n")?;
    } else {
        std::fs::write(md.join("version.yaml"), "version: 2\n")?;
        std::fs::write(md.join("tables.yaml"), "[]\n")?;
        std::fs::write(md.join("functions.yaml"), "[]\n")?;
    }
    std::fs::create_dir_all(dir.join("seeds"))?;
    Ok(())
}

/// `url.ParseRequestURI` accepting it: an absolute URL or an absolute path.
fn is_request_uri(s: &str) -> bool {
    if s.starts_with('/') {
        return !s.contains(' ');
    }
    match s.split_once(':') {
        Some((scheme, rest)) => {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
                && !rest.contains(' ')
        }
        None => false,
    }
}
