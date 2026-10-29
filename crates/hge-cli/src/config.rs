//! Project configuration: `config.yaml`, `HASURA_GRAPHQL_*` environment variables, the
//! `.env` file and flags, with the official CLI's precedence (flag > env > file > default).

use std::path::{Path, PathBuf};

use serde_yaml::Value;

use crate::error::{Context, Error, Result};

#[derive(Clone, Debug, Default)]
pub struct ServerFlags {
    pub endpoint: Option<String>,
    pub admin_secret: Option<String>,
    pub access_key: Option<String>,
    pub insecure_skip_tls_verify: Option<bool>,
    pub certificate_authority: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ApiPaths {
    pub v1_query: String,
    pub v2_query: String,
    pub v1_metadata: String,
    pub graphql: String,
    pub config: String,
    pub pg_dump: String,
    pub version: String,
}

#[derive(Clone, Debug)]
pub struct ActionsConfig {
    pub kind: String,
    pub handler_webhook_baseurl: String,
    pub codegen_framework: String,
    pub codegen_output_dir: String,
    pub codegen_uri: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub version: i64,
    pub endpoint: String,
    pub admin_secret: String,
    pub admin_secrets: Vec<String>,
    pub api_paths: ApiPaths,
    pub insecure_skip_tls_verify: bool,
    pub certificate_authority: String,
    pub disable_interactive: bool,
    pub metadata_directory: String,
    pub metadata_file: String,
    pub migrations_directory: String,
    pub seeds_directory: String,
    pub actions: ActionsConfig,
}

impl Config {
    pub fn get_admin_secret(&self) -> &str {
        self.admin_secrets
            .first()
            .map(String::as_str)
            .unwrap_or(&self.admin_secret)
    }

    fn url(&self, path: &str) -> String {
        join_url(&self.endpoint, path)
    }

    pub fn version_endpoint(&self) -> String {
        self.url(&self.api_paths.version)
    }
    pub fn v1_metadata_endpoint(&self) -> String {
        self.url(&self.api_paths.v1_metadata)
    }
    pub fn v2_query_endpoint(&self) -> String {
        self.url(&self.api_paths.v2_query)
    }
    pub fn v1_query_endpoint(&self) -> String {
        self.url(&self.api_paths.v1_query)
    }
    pub fn pg_dump_endpoint(&self) -> String {
        self.url(&self.api_paths.pg_dump)
    }
    pub fn graphql_endpoint(&self) -> String {
        self.url(&self.api_paths.graphql)
    }
}

/// `url.Path = path.Join(url.Path, p)` on the endpoint.
pub fn join_url(endpoint: &str, p: &str) -> String {
    let (base, query) = match endpoint.find(['?', '#']) {
        Some(i) => (&endpoint[..i], &endpoint[i..]),
        None => (endpoint, ""),
    };
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let path_start = base[scheme_end..]
        .find('/')
        .map(|i| i + scheme_end)
        .unwrap_or(base.len());
    let (origin, path) = base.split_at(path_start);
    let joined = clean_path(&format!("{path}/{p}"));
    let joined = if joined == "/" && path.is_empty() && p.is_empty() {
        String::new()
    } else {
        joined
    };
    format!("{origin}{joined}{query}")
}

/// Go's `path.Clean` for an absolute-or-relative slash path.
fn clean_path(p: &str) -> String {
    let rooted = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let body = parts.join("/");
    if rooted {
        format!("/{body}")
    } else if body.is_empty() {
        ".".into()
    } else {
        body
    }
}

/// Load `KEY=VALUE` lines into the environment without overriding variables that are
/// already set (gotenv.Load).
pub fn load_envfile(path: &Path) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    for (k, v) in parse_env(&text) {
        if std::env::var_os(&k).is_none() {
            // `std::env::set_var` needs `unsafe`, which the workspace forbids, so values
            // from `.env` live in an overlay that `env()` consults.
            if let Ok(mut o) = ENV_OVERLAY.lock() {
                o.push((k, v));
            }
        }
    }
    Ok(())
}

static ENV_OVERLAY: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// An environment variable, including those loaded from the `.env` file.
pub fn env(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        return Some(v);
    }
    let overlay = ENV_OVERLAY.lock().ok()?;
    overlay
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

fn parse_env(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(idx) = line.find(['=', ':']) else {
            continue;
        };
        let key = line[..idx].trim().to_string();
        let mut val = line[idx + 1..].trim().to_string();
        if let Some(q) = val.chars().next().filter(|c| *c == '"' || *c == '\'') {
            if let Some(end) = val[1..].rfind(q) {
                let inner = &val[1..=end];
                val = if q == '"' {
                    inner
                        .replace("\\n", "\n")
                        .replace("\\\"", "\"")
                        .replace("\\\\", "\\")
                } else {
                    inner.to_string()
                };
            }
        } else if let Some(hash) = val.find(" #") {
            val = val[..hash].trim_end().to_string();
        }
        // ${VAR} expansion as gotenv does.
        let val = expand(&val, &out);
        out.push((key, val));
    }
    out
}

fn expand(val: &str, defined: &[(String, String)]) -> String {
    let mut out = String::new();
    let mut rest = val;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find('}') {
            Some(j) => {
                let name = &after[..j];
                let v = defined
                    .iter()
                    .rev()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
                    .or_else(|| std::env::var(name).ok())
                    .unwrap_or_default();
                out.push_str(&v);
                rest = &after[j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// A viper-style lookup: flag, then `HASURA_GRAPHQL_<KEY>`, then config file, then default.
struct Lookup<'a> {
    file: &'a Value,
}

impl Lookup<'_> {
    fn raw(&self, key: &str) -> Option<String> {
        let env_key = format!("HASURA_GRAPHQL_{}", key.to_uppercase().replace('.', "_"));
        if let Some(v) = env(&env_key) {
            return Some(v);
        }
        let mut cur = self.file;
        for part in key.split('.') {
            cur = lookup_ci(cur, part)?;
        }
        match cur {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Number(n) => Some(n.to_string()),
            Value::Sequence(_) => {
                Some(serde_json::to_string(&yaml_to_json(cur)).unwrap_or_default())
            }
            other => Some(
                serde_yaml::to_string(other)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            ),
        }
    }

    fn string(&self, key: &str, default: &str) -> String {
        self.raw(key).unwrap_or_else(|| default.to_string())
    }

    fn bool(&self, key: &str) -> bool {
        matches!(
            self.raw(key)
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("true" | "1" | "t")
        )
    }
}

fn lookup_ci<'v>(v: &'v Value, key: &str) -> Option<&'v Value> {
    match v {
        Value::Mapping(m) => m.iter().find_map(|(k, val)| {
            k.as_str()
                .filter(|k| k.eq_ignore_ascii_case(key))
                .map(|_| val)
        }),
        _ => None,
    }
}

pub fn yaml_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(u) = n.as_u64() {
                u.into()
            } else {
                serde_json::Number::from_f64(n.as_f64().unwrap_or(0.0))
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
        }
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Sequence(s) => serde_json::Value::Array(s.iter().map(yaml_to_json).collect()),
        Value::Mapping(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let key = match k {
                        Value::String(s) => s.clone(),
                        other => serde_yaml::to_string(other)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                    };
                    (key, yaml_to_json(v))
                })
                .collect(),
        ),
        Value::Tagged(t) => yaml_to_json(&t.value),
    }
}

/// Read `config.yaml` in `dir` and apply env, `.env` and flags (`readConfig`).
pub fn read_config(dir: &Path, flags: &ServerFlags) -> Result<Config> {
    let file_path = dir.join("config.yaml");
    let text = std::fs::read_to_string(&file_path).map_err(|e| {
        Error(format!(
            "cannot read config from file/env: Config File \"config\" Not Found in \"[{}]\"{}",
            dir.display(),
            if e.kind() == std::io::ErrorKind::NotFound {
                String::new()
            } else {
                format!(": {e}")
            }
        ))
    })?;
    let file: Value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_yaml::from_str(&text).ctx("cannot read config from file/env: While parsing config")?
    };
    let l = Lookup { file: &file };
    let flag_or = |flag: &Option<String>, key: &str, default: &str| -> String {
        flag.clone().unwrap_or_else(|| l.string(key, default))
    };
    let mut admin_secret = flag_or(&flags.admin_secret, "admin_secret", "");
    if admin_secret.is_empty() {
        admin_secret = flag_or(&flags.access_key, "access_key", "");
    }
    let admin_secrets_raw = l.string("admin_secrets", "");
    let admin_secrets: Vec<String> = if admin_secrets_raw.is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&admin_secrets_raw).map_err(|e| {
            Error(format!(
                "parsing 'admin_secrets' from config.yaml / environment variable HASURA_GRAPHQL_ADMIN_SECRETS failed: expected value of format [\"secret1\", \"secret2\"]: {e}"
            ))
        })?
    };
    let version_raw = l.string("version", "1");
    let version: i64 = version_raw.trim().parse().unwrap_or(0);
    if !(1..=3).contains(&version) {
        return Err(Error("invalid config version".into()));
    }
    let config = Config {
        version,
        endpoint: flag_or(&flags.endpoint, "endpoint", "http://localhost:8080"),
        admin_secret,
        admin_secrets,
        api_paths: ApiPaths {
            v1_query: l.string("api_paths.query", "v1/query"),
            v2_query: l.string("api_paths.v2_query", "v2/query"),
            v1_metadata: l.string("api_paths.v1_metadata", "v1/metadata"),
            graphql: l.string("api_paths.graphql", "v1/graphql"),
            config: l.string("api_paths.config", "v1alpha1/config"),
            pg_dump: l.string("api_paths.pg_dump", "v1alpha1/pg_dump"),
            version: l.string("api_paths.version", "v1/version"),
        },
        insecure_skip_tls_verify: flags
            .insecure_skip_tls_verify
            .unwrap_or_else(|| l.bool("insecure_skip_tls_verify")),
        certificate_authority: flag_or(&flags.certificate_authority, "certificate_authority", ""),
        disable_interactive: l.bool("disable_interactive"),
        metadata_directory: l.string("metadata_directory", "metadata"),
        metadata_file: l.string("metadata_file", ""),
        migrations_directory: l.string("migrations_directory", "migrations"),
        seeds_directory: l.string("seeds_directory", "seeds"),
        actions: ActionsConfig {
            kind: l.string("actions.kind", "synchronous"),
            handler_webhook_baseurl: l
                .string("actions.handler_webhook_baseurl", "http://localhost:3000"),
            codegen_framework: l.string("actions.codegen.framework", ""),
            codegen_output_dir: l.string("actions.codegen.output_dir", ""),
            codegen_uri: l.string("actions.codegen.uri", ""),
        },
    };
    parse_request_uri(&config.endpoint)
        .ctx("cannot read config: unable to parse server endpoint")?;
    Ok(config)
}

/// Go's `url.ParseRequestURI`: an absolute URL or an absolute path.
fn parse_request_uri(s: &str) -> std::result::Result<(), String> {
    if s.is_empty() {
        return Err("parse \"\": empty url".into());
    }
    if s.starts_with('/') {
        return Ok(());
    }
    match s.find(':') {
        Some(i)
            if i > 0
                && s[..i]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) =>
        {
            Ok(())
        }
        _ => Err(format!(
            "parse {}: invalid URI for request",
            crate::gofmt::quote(s)
        )),
    }
}

/// Find the project directory: `--project` or the current directory, walking up until a
/// directory with `config.yaml` is found (`validateDirectory`).
pub fn find_project_dir(project: Option<&str>) -> Result<PathBuf> {
    let start = match project {
        Some(p) if !p.is_empty() => std::path::absolute(p)
            .ctx("validating current directory failed: error finding absolute path for project directory")?,
        _ => std::env::current_dir()
            .ctx("validating current directory failed: error getting current working directory")?,
    };
    match std::fs::metadata(&start) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error(format!(
                "validating current directory failed: did not find required directory. use 'init'?: stat {}: no such file or directory",
                start.display()
            )));
        }
        Err(e) => {
            return Err(Error(format!(
                "validating current directory failed: error getting directory details: {e}"
            )));
        }
        Ok(m) if !m.is_dir() => {
            return Err(Error(format!(
                "validating current directory failed: '{}' is not a directory: %!w(<nil>)",
                start
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            )));
        }
        Ok(_) => {}
    }
    let mut dir = start.clone();
    loop {
        if dir.join("config.yaml").is_file() {
            return Ok(dir);
        }
        let Some(parent) = dir.parent().map(Path::to_path_buf) else {
            return Err(Error(format!(
                "validating current directory failed: failed recursively find config.yaml: search stopped due to a possible infinite filesystem traversal at {}",
                dir.display()
            )));
        };
        if is_filesystem_boundary(&parent) {
            return Err(Error(format!(
                "validating current directory failed: cannot find [config.yaml] | search stopped: filesystem boundary at {}",
                parent.display()
            )));
        }
        dir = parent;
    }
}

fn is_filesystem_boundary(dir: &Path) -> bool {
    dir.parent().is_none() || std::env::var_os("HOME").is_some_and(|h| Path::new(&h) == dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_endpoint_paths() {
        assert_eq!(
            join_url("http://localhost:8080", "v1/metadata"),
            "http://localhost:8080/v1/metadata"
        );
        assert_eq!(
            join_url("http://h:1/base/", "v1/version"),
            "http://h:1/base/v1/version"
        );
    }

    #[test]
    fn parses_env_files() {
        let env = parse_env("# c\nA=1\nexport B=\"x y\"\nC='z' \nD=${A}2 # note\n");
        assert_eq!(
            env,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "x y".into()),
                ("C".into(), "z".into()),
                ("D".into(), "12".into())
            ]
        );
    }
}
