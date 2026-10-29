//! Migrations: the project's `migrations/<database>/<version>_<name>/{up,down}.sql` files,
//! the applied versions the CLI keeps in the engine's catalog state (`cli_state`), and the
//! official CLI's rules for which migrations `apply`, `--down`, `--goto` and `--version`
//! run and in what order.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::context::io_message;
use crate::error::Error;
use crate::logger::Logger;

/// Why a migration run stopped. `NoChange` and `NoMigrations` are reported as "nothing to
/// apply" by `migrate apply`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateError {
    NoChange,
    /// The source has no migrations (`os.PathError` with op "first").
    NoMigrations(String),
    Other(String),
}

impl fmt::Display for MigrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrateError::NoChange => f.write_str("no change"),
            MigrateError::NoMigrations(dir) => write!(f, "first {dir}: file does not exist"),
            MigrateError::Other(s) => f.write_str(s),
        }
    }
}

impl From<Error> for MigrateError {
    fn from(e: Error) -> Self {
        MigrateError::Other(e.0)
    }
}

impl From<MigrateError> for Error {
    fn from(e: MigrateError) -> Self {
        Error(e.to_string())
    }
}

type MResult<T> = std::result::Result<T, MigrateError>;

fn other(s: impl Into<String>) -> MigrateError {
    MigrateError::Other(s.into())
}

// --- source files

#[derive(Debug, Clone, Default)]
pub struct SourceMigration {
    pub name: String,
    pub up: Option<String>,
    pub down: Option<String>,
}

/// The migration files of one database directory.
#[derive(Debug, Clone, Default)]
pub struct Source {
    pub dir: PathBuf,
    pub migrations: BTreeMap<u64, SourceMigration>,
}

/// `<version>_<name>.(up|down).sql`; the CLI matches a directory's files as
/// `<dir>.<file>`.
fn parse_name(raw: &str) -> Option<(u64, String, bool)> {
    let (rest, up) = if let Some(r) = raw.strip_suffix(".up.sql") {
        (r, true)
    } else {
        let r = raw.strip_suffix(".down.sql")?;
        (r, false)
    };
    let (version, name) = rest.split_once('_')?;
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((version.parse().ok()?, name.to_string(), up))
}

impl Source {
    pub fn scan(dir: &Path) -> MResult<Source> {
        let mut src = Source {
            dir: dir.to_path_buf(),
            migrations: BTreeMap::new(),
        };
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| other(format!("open {}: {}", dir.display(), io_message(&e))))?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            // Names that are not UTF-8 cannot be migrations; skip them like Go's parser.
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let path = dir.join(&name);
            let meta = std::fs::metadata(&path)
                .map_err(|e| other(format!("lstat {}: {}", path.display(), io_message(&e))))?;
            if meta.is_dir() {
                let mut files: Vec<_> = std::fs::read_dir(&path)
                    .map_err(|e| other(format!("open {}: {}", path.display(), io_message(&e))))?
                    .filter_map(|e| e.ok())
                    .collect();
                files.sort_by_key(|e| e.file_name());
                for f in files {
                    if f.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let Some(file) = f.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if let Some(parsed) = parse_name(&format!("{name}.{file}")) {
                        src.add(parsed, format!("{name}/{file}"))?;
                    }
                }
            } else if let Some(parsed) = parse_name(&name) {
                src.add(parsed, name)?;
            }
        }
        Ok(src)
    }

    fn add(&mut self, (version, name, up): (u64, String, bool), raw: String) -> MResult<()> {
        let data = std::fs::read(self.dir.join(&raw))
            .map_err(|e| other(format!("cannot read file {raw}: {}", io_message(&e))))?;
        if data.is_empty() {
            return Ok(());
        }
        let m = self
            .migrations
            .entry(version)
            .or_insert_with(|| SourceMigration {
                name: name.clone(),
                ..Default::default()
            });
        let slot = if up { &mut m.up } else { &mut m.down };
        if let Some(existing) = slot {
            return Err(other(format!(
                "found duplicate migrations for version {version}\n- {raw}\n- {existing}"
            )));
        }
        *slot = Some(raw);
        Ok(())
    }

    fn first(&self) -> Option<u64> {
        self.migrations.keys().next().copied()
    }

    fn last(&self) -> Option<u64> {
        self.migrations.keys().next_back().copied()
    }

    fn next(&self, v: u64) -> Option<u64> {
        if !self.migrations.contains_key(&v) {
            return None;
        }
        self.migrations.range(v + 1..).next().map(|(k, _)| *k)
    }

    fn prev(&self, v: u64) -> Option<u64> {
        if !self.migrations.contains_key(&v) {
            return None;
        }
        self.migrations.range(..v).next_back().map(|(k, _)| *k)
    }

    pub fn name(&self, v: u64) -> String {
        self.migrations
            .get(&v)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "-".into())
    }

    fn read(&self, raw: &str) -> MResult<String> {
        std::fs::read_to_string(self.dir.join(raw)).map_err(|e| {
            other(format!(
                "open {}: {}",
                self.dir.join(raw).display(),
                io_message(&e)
            ))
        })
    }

    fn not_exist(&self, op: &str) -> MigrateError {
        other(format!("{op} {}: file does not exist", self.dir.display()))
    }
}

// --- catalog state

/// The CLI's part of the engine's catalog state.
#[derive(Debug, Clone, Default)]
pub struct CliState {
    pub migrations: BTreeMap<String, BTreeMap<String, bool>>,
    pub settings: BTreeMap<String, String>,
    pub is_state_copy_completed: bool,
}

impl CliState {
    pub fn get(client: &Client) -> crate::error::Result<CliState> {
        let body = client.metadata_ok(&json!({"type": "get_catalog_state", "args": {}}))?;
        let v: Value = serde_json::from_str(&body).map_err(|e| Error(e.to_string()))?;
        let cli = &v["cli_state"];
        let mut state = CliState::default();
        if let Some(m) = cli.get("migrations").and_then(Value::as_object) {
            for (db, versions) in m {
                let mut vs = BTreeMap::new();
                if let Some(versions) = versions.as_object() {
                    for (k, dirty) in versions {
                        vs.insert(k.clone(), dirty.as_bool().unwrap_or(false));
                    }
                }
                state.migrations.insert(db.clone(), vs);
            }
        }
        if let Some(s) = cli.get("settings").and_then(Value::as_object) {
            for (k, v) in s {
                state
                    .settings
                    .insert(k.clone(), v.as_str().unwrap_or_default().to_string());
            }
        }
        state.is_state_copy_completed = cli
            .get("isStateCopyCompleted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(state)
    }

    pub fn set(&self, client: &Client) -> crate::error::Result<()> {
        let mut state = Map::new();
        if !self.migrations.is_empty() {
            let m: Map<String, Value> = self
                .migrations
                .iter()
                .map(|(db, vs)| {
                    let vs: Map<String, Value> = vs
                        .iter()
                        .map(|(k, d)| (k.clone(), Value::Bool(*d)))
                        .collect();
                    (db.clone(), Value::Object(vs))
                })
                .collect();
            state.insert("migrations".into(), Value::Object(m));
        }
        let settings: Map<String, Value> = self
            .settings
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        state.insert("settings".into(), Value::Object(settings));
        state.insert(
            "isStateCopyCompleted".into(),
            Value::Bool(self.is_state_copy_completed),
        );
        client.metadata_ok(
            &json!({"type": "set_catalog_state", "args": {"type": "cli", "state": state}}),
        )?;
        Ok(())
    }

    /// Applied versions of a database (version → dirty).
    pub fn versions(&self, db: &str) -> MResult<BTreeMap<u64, bool>> {
        let mut out = BTreeMap::new();
        for (k, d) in self.migrations.get(db).into_iter().flatten() {
            let v: u64 = k.parse().map_err(|_| {
                other(format!(
                    "parsing migration version: strconv.ParseUint: parsing {}: invalid syntax",
                    crate::gofmt::quote(k)
                ))
            })?;
            out.insert(v, *d);
        }
        Ok(out)
    }
}

// --- status

#[derive(Debug, Clone)]
pub struct MigrationStatus {
    pub version: u64,
    pub name: String,
    pub is_applied: bool,
    pub is_present: bool,
}

/// Every version known to the project or the server, in order.
pub type Status = BTreeMap<u64, MigrationStatus>;

// --- the migrator

pub struct Migrate<'a> {
    client: &'a Client,
    logger: &'a Logger,
    pub source_name: String,
    pub source_kind: String,
    pub source: Source,
    db: BTreeMap<u64, bool>,
    pub skip_execution: bool,
    pub dry_run: bool,
    pub no_transaction: bool,
    pub per_migration_transaction: bool,
}

/// A migration to run: `version` towards `target` (equal for up, the previous version or
/// -1 for down). `file` is `None` when that direction has no file.
#[derive(Debug, Clone)]
struct Step {
    version: u64,
    target: i64,
    file: Option<String>,
    identifier: String,
}

enum Item {
    Step(Step),
    Err(MigrateError),
}

pub const SUPPORTED_KINDS: &[&str] = &["mssql", "postgres", "citus", "cockroach", "bigquery"];

impl<'a> Migrate<'a> {
    /// `migrate.NewMigrate`: prepare the catalog state, copy V2-era state if needed, scan.
    pub fn new(
        client: &'a Client,
        logger: &'a Logger,
        migrations_dir: &Path,
        source_name: &str,
        source_kind: &str,
        disable_state_copy: bool,
    ) -> crate::error::Result<Migrate<'a>> {
        if source_kind.is_empty() {
            return Err(Error("invalid source kind".into()));
        }
        let dir = migrations_dir.join(source_name);
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
        }
        let wrap = |e: Error| Error(format!("cannot create migrate instance: {e}"));
        // PrepareSettingsDriver: default settings, written back.
        let mut state = CliState::get(client).map_err(wrap)?;
        state
            .settings
            .entry("migration_mode".into())
            .or_insert_with(|| "true".into());
        if state
            .settings
            .get("migration_mode")
            .is_some_and(String::is_empty)
        {
            state
                .settings
                .insert("migration_mode".into(), "true".into());
        }
        state.set(client).map_err(wrap)?;
        if !disable_state_copy && let Err(e) = copy_state_if_required(client, logger, source_name) {
            logger.warn(&e.0);
        }
        let mut m = Migrate {
            client,
            logger,
            source_name: source_name.to_string(),
            source_kind: source_kind.to_string(),
            source: Source::default(),
            db: BTreeMap::new(),
            skip_execution: false,
            dry_run: false,
            no_transaction: false,
            per_migration_transaction: false,
        };
        m.rescan(&dir).map_err(|e| wrap(e.into()))?;
        Ok(m)
    }

    fn rescan(&mut self, dir: &Path) -> MResult<()> {
        self.source = Source::scan(dir)?;
        let state = CliState::get(self.client)?;
        self.db = state.versions(&self.source_name)?;
        Ok(())
    }

    pub fn status(&self) -> Status {
        let mut status = Status::new();
        for (v, m) in &self.source.migrations {
            status.insert(
                *v,
                MigrationStatus {
                    version: *v,
                    name: m.name.clone(),
                    is_applied: false,
                    is_present: true,
                },
            );
        }
        for v in self.db.keys() {
            status
                .entry(*v)
                .or_insert_with(|| MigrationStatus {
                    version: *v,
                    name: self.source.name(*v),
                    is_applied: false,
                    is_present: false,
                })
                .is_applied = true;
        }
        status
    }

    fn migration_mode(&self) -> MResult<()> {
        let state = CliState::get(self.client)?;
        if state.settings.get("migration_mode").map(String::as_str) != Some("true") {
            return Err(other("Migration mode is disabled"));
        }
        Ok(())
    }

    fn db_version(&self) -> Option<(u64, bool)> {
        self.db.iter().next_back().map(|(v, d)| (*v, *d))
    }

    fn db_prev(&self, v: u64) -> Option<u64> {
        if !self.db.contains_key(&v) {
            return None;
        }
        self.db.range(..v).next_back().map(|(k, _)| *k)
    }

    fn up_exists(&self, v: u64) -> MResult<()> {
        match self.source.migrations.get(&v) {
            Some(m) if m.up.is_some() => Ok(()),
            _ => Err(other(format!("{v} up migration not found"))),
        }
    }

    fn down_exists(&self, v: u64) -> MResult<()> {
        match self.source.migrations.get(&v) {
            Some(m) if m.down.is_some() => Ok(()),
            _ => Err(other(format!("{v} down migration not found"))),
        }
    }

    fn step(&self, version: u64, target: i64) -> Step {
        let m = self.source.migrations.get(&version);
        let file = m.and_then(|m| {
            if target >= version as i64 {
                m.up.clone()
            } else {
                m.down.clone()
            }
        });
        let identifier = match &file {
            Some(_) => m.map(|m| m.name.clone()).unwrap_or_default(),
            None => "<empty>".into(),
        };
        Step {
            version,
            target,
            file,
            identifier,
        }
    }

    fn read_up(&self, limit: i64) -> Vec<Item> {
        let mut out = Vec::new();
        if limit == 0 {
            out.push(Item::Err(MigrateError::NoChange));
            return out;
        }
        if limit == -1
            && !self
                .status()
                .values()
                .any(|m| m.is_present && !m.is_applied)
        {
            out.push(Item::Err(MigrateError::NoChange));
            return out;
        }
        let mut count = 0i64;
        let mut from: Option<u64> = None;
        while count < limit || limit == -1 {
            let next = match from {
                None => match self.source.first() {
                    Some(v) => v,
                    None => {
                        out.push(Item::Err(MigrateError::NoMigrations(
                            self.source.dir.display().to_string(),
                        )));
                        return out;
                    }
                },
                Some(f) => match self.source.next(f) {
                    Some(v) => v,
                    None => {
                        if limit == -1 && count == 0 || limit > 0 && count == 0 {
                            out.push(Item::Err(MigrateError::NoChange));
                        } else if limit != -1 && count < limit {
                            out.push(Item::Err(other(format!("limit {} short", limit - count))));
                        }
                        return out;
                    }
                },
            };
            from = Some(next);
            if self.db.contains_key(&next) {
                continue;
            }
            if let Err(e) = self.up_exists(next) {
                out.push(Item::Err(e));
                return out;
            }
            out.push(Item::Step(self.step(next, next as i64)));
            count += 1;
        }
        out
    }

    fn read_down(&self, limit: i64) -> Vec<Item> {
        let mut out = Vec::new();
        if limit == 0 {
            out.push(Item::Err(MigrateError::NoChange));
            return out;
        }
        let Some((mut from, _)) = self.db_version() else {
            out.push(Item::Err(MigrateError::NoChange));
            return out;
        };
        let mut count = 0i64;
        while count < limit || limit == -1 {
            if let Err(e) = self.down_exists(from) {
                out.push(Item::Err(e));
                return out;
            }
            match self.db_prev(from) {
                None => {
                    if limit == -1 || limit - count > 0 {
                        out.push(Item::Step(self.step(from, -1)));
                        count += 1;
                    }
                    if count < limit {
                        out.push(Item::Err(other(format!("limit {} short", limit - count))));
                    }
                    return out;
                }
                Some(prev) => {
                    out.push(Item::Step(self.step(from, prev as i64)));
                    from = prev;
                    count += 1;
                }
            }
        }
        out
    }

    /// One version, up or down (`migrate apply --version`).
    fn read_one(&self, version: u64, up: bool) -> Vec<Item> {
        let applied = self.db.contains_key(&version);
        if up {
            if applied {
                return vec![Item::Err(other("Version already applied in database"))];
            }
            if let Err(e) = self.up_exists(version) {
                return vec![Item::Err(e)];
            }
            return vec![Item::Step(self.step(version, version as i64))];
        }
        if !applied {
            return vec![Item::Err(other("Migration not applied in database"))];
        }
        if let Err(e) = self.down_exists(version) {
            return vec![Item::Err(e)];
        }
        match self.source.prev(version) {
            None => vec![Item::Step(self.step(version, -1))],
            Some(prev) => vec![Item::Step(self.step(version, prev as i64))],
        }
    }

    fn read_up_to(&self, to: i64) -> Vec<Item> {
        let mut out = Vec::new();
        if to == -1
            || !self
                .status()
                .values()
                .any(|m| m.version <= to as u64 && m.is_present && !m.is_applied)
        {
            out.push(Item::Err(MigrateError::NoChange));
            return out;
        }
        let mut from: i64 = -1;
        loop {
            if from == to {
                return out;
            }
            let next = if from == -1 {
                match self.source.first() {
                    Some(v) => v,
                    None => {
                        out.push(Item::Err(MigrateError::NoMigrations(
                            self.source.dir.display().to_string(),
                        )));
                        return out;
                    }
                }
            } else {
                match self.source.next(from as u64) {
                    Some(v) => v,
                    None => {
                        out.push(Item::Err(
                            self.source.not_exist(&format!("next for version {from}")),
                        ));
                        return out;
                    }
                }
            };
            from = next as i64;
            if self.db.contains_key(&next) {
                continue;
            }
            if let Err(e) = self.up_exists(next) {
                out.push(Item::Err(e));
                return out;
            }
            out.push(Item::Step(self.step(next, next as i64)));
        }
    }

    fn read_down_to(&self, mut from: i64, to: i64) -> Vec<Item> {
        let mut out = Vec::new();
        if from == -1
            || !self.status().values().any(|m| {
                !(m.version > from as u64 || to >= 0 && m.version <= to as u64)
                    && m.is_present
                    && m.is_applied
            })
        {
            out.push(Item::Err(MigrateError::NoChange));
            return out;
        }
        loop {
            if from == to {
                return out;
            }
            if let Err(e) = self.down_exists(from as u64) {
                out.push(Item::Err(e));
                return out;
            }
            match self.db_prev(from as u64) {
                Some(prev) => {
                    out.push(Item::Step(self.step(from as u64, prev as i64)));
                    from = prev as i64;
                }
                None => match self.source.prev(from as u64) {
                    None if to == -1 => {
                        out.push(Item::Step(self.step(from as u64, -1)));
                        from = -1;
                    }
                    None => {
                        out.push(Item::Err(
                            self.source.not_exist(&format!("prev for version {from}")),
                        ));
                        return out;
                    }
                    Some(prev) => {
                        out.push(Item::Err(other(format!(
                            "&{{{prev} false}} not applied on database"
                        ))));
                        return out;
                    }
                },
            }
        }
    }

    // --- entry points (the CLI's `UpCmd`, `DownCmd`, `GotoCmd`, `GotoVersionCmd`)

    pub fn up(&mut self, limit: i64) -> MResult<()> {
        self.migration_mode()?;
        let items = if limit >= 0 {
            if limit == 0 {
                return Err(MigrateError::NoChange);
            }
            self.read_up(limit)
        } else {
            if let Some((v, true)) = self.db_version() {
                return Err(other(format!(
                    "Dirty database version {v}. Fix and force version."
                )));
            }
            self.read_up(-1)
        };
        self.run(items)
    }

    pub fn down(&mut self, limit: i64) -> MResult<()> {
        self.migration_mode()?;
        let items = if limit >= 0 {
            if limit == 0 {
                return Err(MigrateError::NoChange);
            }
            self.read_down(limit)
        } else {
            if let Some((v, true)) = self.db_version() {
                return Err(other(format!(
                    "Dirty database version {v}. Fix and force version."
                )));
            }
            self.read_down(-1)
        };
        self.run(items)
    }

    pub fn version(&mut self, version: u64, up: bool) -> MResult<()> {
        self.migration_mode()?;
        let items = self.read_one(version, up);
        self.run(items)
    }

    pub fn goto(&mut self, to: i64) -> MResult<()> {
        self.migration_mode()?;
        let (current, dirty) = match self.db_version() {
            Some((v, d)) => (v as i64, d),
            None => (-1, false),
        };
        if dirty {
            return Err(other(format!(
                "Dirty database version {current}. Fix and force version."
            )));
        }
        let items = if current <= to {
            self.read_up_to(to)
        } else {
            self.read_down_to(current, to)
        };
        self.run(items)
    }

    fn run(&mut self, items: Vec<Item>) -> MResult<()> {
        if self.dry_run {
            let mut rows = String::from("VERSION\tTYPE\tNAME\n");
            let mut last = None;
            for item in items {
                match item {
                    Item::Err(e) => return Err(e),
                    Item::Step(s) if s.file.is_some() && last != Some(s.version) => {
                        last = Some(s.version);
                        let dir = if s.version as i64 == s.target {
                            "up"
                        } else {
                            "down"
                        };
                        rows.push_str(&format!("{}\t{dir}\t{}\n", s.version, s.identifier));
                    }
                    Item::Step(_) => {}
                }
            }
            print!("{}", crate::table::tabwrite(&rows));
            return Ok(());
        }
        for item in items {
            let s = match item {
                Item::Err(e) => return Err(e),
                Item::Step(s) => s,
            };
            let Some(file) = &s.file else { continue };
            if !self.skip_execution {
                self.logger.debug(&format!("applying migration: {file}"));
                let mut body = self.source.read(file)?;
                let mut no_tx = self.no_transaction;
                if self.per_migration_transaction {
                    let first = body.lines().next().unwrap_or_default();
                    no_tx = first.trim() == "-- hasura:no-transaction";
                }
                if body.is_empty() {
                    body.clear();
                } else {
                    self.run_sql(&body, no_tx)?;
                }
            }
            self.set_version(s.version)?;
            if s.version as i64 != s.target {
                self.remove_version(s.version)?;
            }
        }
        Ok(())
    }

    fn run_sql(&self, sql: &str, no_tx: bool) -> MResult<()> {
        let mut args =
            json!({"sql": sql, "source": self.source_name, "check_metadata_consistency": false});
        if no_tx && self.source_kind != "mssql" && self.source_kind != "bigquery" {
            args["no_transaction"] = Value::Bool(true);
        }
        let kind = match self.source_kind.as_str() {
            "postgres" => "run_sql",
            "mssql" => "mssql_run_sql",
            "citus" => "citus_run_sql",
            "cockroach" => "cockroach_run_sql",
            "bigquery" => "bigquery_run_sql",
            k => {
                return Err(other(format!(
                    "unsupported source kind, source name: {} kind: {k}",
                    self.source_name
                )));
            }
        };
        let r = self.client.v2_query(&json!({"type": kind, "args": args}))?;
        if !r.ok() {
            if r.body.is_empty() {
                return Err(other(format!("run_sql api request failed {}", r.status)));
            }
            return Err(other(r.body));
        }
        Ok(())
    }

    pub fn set_version(&mut self, v: u64) -> MResult<()> {
        let mut state = CliState::get(self.client)?;
        state
            .migrations
            .entry(self.source_name.clone())
            .or_default()
            .insert(v.to_string(), false);
        state.set(self.client)?;
        Ok(())
    }

    pub fn remove_version(&mut self, v: u64) -> MResult<()> {
        let mut state = CliState::get(self.client)?;
        if let Some(m) = state.migrations.get_mut(&self.source_name) {
            m.remove(&v.to_string());
        }
        state.set(self.client)?;
        Ok(())
    }

    /// `RemoveVersions`: forget versions on the server (migrate delete, squash).
    pub fn remove_versions(&mut self, versions: &[u64]) -> MResult<()> {
        self.migration_mode()?;
        for v in versions {
            self.remove_version(*v)?;
        }
        Ok(())
    }

    /// `Squash`: the up SQL of `from..=to` in order and the down SQL in reverse, each
    /// file preceded by a newline, plus the squashed versions.
    pub fn squash(
        &self,
        from: u64,
        to: i64,
    ) -> MResult<(Vec<u64>, Option<String>, Option<String>)> {
        self.migration_mode()?;
        let mut versions = Vec::new();
        let mut up: Option<String> = None;
        let push = |buf: &mut Option<String>, sql: String| {
            let b = buf.get_or_insert_with(String::new);
            b.push('\n');
            b.push_str(&sql);
        };
        // Up: `from` itself, then each next version until `to` (or the end).
        if from as i64 != to {
            let mut current = from;
            let mut first = true;
            loop {
                let v = if first {
                    first = false;
                    from
                } else {
                    match self.source.next(current) {
                        Some(v) => v,
                        None => break,
                    }
                };
                self.up_exists(v)?;
                if let Some(file) = self.source.migrations.get(&v).and_then(|m| m.up.clone()) {
                    push(&mut up, self.source.read(&file)?);
                }
                versions.push(v);
                if v as i64 == to {
                    break;
                }
                current = v;
            }
        }
        // Down: from `to` (or the last version) back to `from`.
        let mut down: Option<String> = None;
        let mut v = if to == -1 {
            self.source.last().unwrap_or(0)
        } else {
            to as u64
        };
        loop {
            if v < from {
                break;
            }
            match self.source.prev(v) {
                None => {
                    if let Some(file) = self.source.migrations.get(&v).and_then(|m| m.down.clone())
                    {
                        push(&mut down, self.source.read(&file)?);
                    }
                    break;
                }
                Some(prev) => {
                    match self.down_exists(v) {
                        Ok(()) => {
                            if let Some(file) =
                                self.source.migrations.get(&v).and_then(|m| m.down.clone())
                            {
                                push(&mut down, self.source.read(&file)?);
                            }
                        }
                        Err(e) => self.logger.warn(&e.to_string()),
                    }
                    v = prev;
                }
            }
        }
        Ok((versions, up, down))
    }
}

/// Before config v3 the CLI kept applied versions in `hdb_catalog.schema_migrations`;
/// copy them into the catalog state once.
fn copy_state_if_required(
    client: &Client,
    logger: &Logger,
    source: &str,
) -> crate::error::Result<()> {
    let mut state = CliState::get(client)?;
    if state.is_state_copy_completed {
        logger.debug("skipping state migration, found IsStateCopyCompleted: true");
        return Ok(());
    }
    let count = client.run_sql(json!({
        "source": source,
        "sql": "SELECT COUNT(1) FROM information_schema.tables WHERE table_name = 'schema_migrations' AND table_schema = 'hdb_catalog' LIMIT 1",
    }));
    let has_table = match &count {
        Ok(v) => v["result"][1][0].as_str() != Some("0"),
        Err(_) => false,
    };
    if has_table {
        logger.debug("copying cli state from hdb_catalog.schema_migrations to catalog state");
        let rows = client.run_sql(json!({
            "source": source,
            "sql": "SELECT version, dirty FROM hdb_catalog.schema_migrations",
        }))?;
        let versions = state.migrations.entry(source.to_string()).or_default();
        for row in rows["result"].as_array().into_iter().flatten().skip(1) {
            if let Some(v) = row[0].as_str() {
                versions.insert(v.to_string(), row[1].as_str() == Some("t"));
            }
        }
    } else {
        logger.debug("hdb_catalog.schema_migrations was not found, skipping state migration");
    }
    state.is_state_copy_completed = true;
    state
        .set(client)
        .map_err(|e| Error(format!("error setting state: {e}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_migration_names() {
        assert_eq!(
            super::parse_name("1700_init.up.sql"),
            Some((1700, "init".into(), true))
        );
        assert_eq!(
            super::parse_name("1_a_b.down.sql"),
            Some((1, "a_b".into(), false))
        );
        assert_eq!(super::parse_name("x_init.up.sql"), None);
        assert_eq!(super::parse_name("1_init.up.yaml"), None);
    }
}
