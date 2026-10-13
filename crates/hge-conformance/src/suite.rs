//! The on-disk format of the conformance suite (see `conformance/README.md`).
//!
//! ```text
//! conformance/suites/<area>/<suite>/
//!   suite.yaml      config: admin_secret, env, isolation
//!   setup.sql       schema + data, applied to a fresh database
//!   data.sql        (isolation: data) data restored before every case
//!   metadata.json   V2 metadata (export_metadata format)
//!   cases/*.yaml    one case per file: a list of request/response steps
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How much state is reset between the cases of a suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// Cases only read: the suite is set up once.
    #[default]
    None,
    /// Cases write data: every table is truncated and `data.sql` re-applied before each case.
    Data,
    /// Cases change schema or metadata: the whole suite is set up again before each case.
    Database,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteConfig {
    #[serde(default)]
    #[expect(dead_code, reason = "documentation for readers of the suite")]
    pub description: Option<String>,
    /// Where the suite came from.
    #[serde(default)]
    #[expect(dead_code, reason = "documentation for readers of the suite")]
    pub upstream: Option<Value>,
    /// Run the engine with an admin secret and send it with every request
    /// (unless a case sets `admin_secret_header: false`).
    #[serde(default)]
    pub admin_secret: bool,
    /// `HASURA_GRAPHQL_*` environment the engine is started with.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub isolation: Isolation,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<Value>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub admin_secret_header: bool,
    pub steps: Vec<Step>,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// One request and its expected response, in tests-py's fixture format.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default = "ok")]
    pub status: u16,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, Value>,
    /// JSON request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<Value>,
    /// Raw request body (instead of `query`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// Expected response body (`--record` fills it in from Hasura).
    #[serde(default)]
    pub response: Value,
    /// Response headers that must be present with these values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resp_headers: BTreeMap<String, Option<String>>,
}

fn ok() -> u16 {
    200
}

#[derive(Debug, Clone)]
pub struct Case {
    /// `<area>/<suite>/<case>`
    pub id: String,
    pub path: PathBuf,
    pub file: CaseFile,
}

#[derive(Debug, Clone)]
pub struct Suite {
    /// `<area>/<suite>`
    pub id: String,
    pub area: String,
    pub config: SuiteConfig,
    pub setup_sql: String,
    pub data_sql: Option<String>,
    pub metadata: Value,
    pub cases: Vec<Case>,
}

impl Suite {
    /// The environment that decides which engine process can serve this suite.
    pub fn engine_key(&self) -> String {
        format!("{}|{:?}", self.config.admin_secret, self.config.env)
    }
}

fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    v.sort();
    Ok(v)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Load every suite under `root` (`<root>/<area>/<suite>/`).
pub fn load_all(root: &Path) -> Result<Vec<Suite>> {
    let mut suites = vec![];
    for area_dir in sorted_entries(root)? {
        if !area_dir.is_dir() {
            continue;
        }
        for suite_dir in sorted_entries(&area_dir)? {
            if suite_dir.is_dir() {
                suites.push(
                    load_suite(&file_name(&area_dir), &suite_dir)
                        .with_context(|| format!("loading suite {}", suite_dir.display()))?,
                );
            }
        }
    }
    Ok(suites)
}

pub fn load_suite(area: &str, dir: &Path) -> Result<Suite> {
    let name = file_name(dir);
    let id = format!("{area}/{name}");
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).with_context(|| f.to_owned());
    let config: SuiteConfig = serde_yaml::from_str(&read("suite.yaml")?).context("suite.yaml")?;
    let setup_sql = read("setup.sql")?;
    let data_sql = if dir.join("data.sql").exists() {
        Some(read("data.sql")?)
    } else {
        None
    };
    if config.isolation == Isolation::Data && data_sql.is_none() {
        bail!("isolation: data needs a data.sql");
    }
    let metadata: Value = serde_json::from_str(&read("metadata.json")?).context("metadata.json")?;
    let mut cases = vec![];
    for path in sorted_entries(&dir.join("cases"))? {
        if path.extension().is_none_or(|e| e != "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        let file: CaseFile =
            serde_yaml::from_str(&text).with_context(|| format!("{}", path.display()))?;
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        cases.push(Case {
            id: format!("{id}/{stem}"),
            path,
            file,
        });
    }
    Ok(Suite {
        id,
        area: area.to_owned(),
        config,
        setup_sql,
        data_sql,
        metadata,
        cases,
    })
}
