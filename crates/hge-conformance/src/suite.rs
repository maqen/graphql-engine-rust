//! The on-disk format of the conformance suite (see `conformance/README.md`).
//!
//! ```text
//! conformance/suites/<area>/<suite>/
//!   suite.yaml      config: admin_secret, env, isolation
//!   setup.sql       schema + data, applied to a fresh database
//!   data.sql        (isolation: data) data restored before every case
//!   metadata.json   V2 metadata (export_metadata format)
//!   cases/*.yaml    one case per file: a list of steps (HTTP request/response, or
//!                   websocket connect/send/expect)
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

/// One step of a case.
#[derive(Debug, Clone)]
pub enum Step {
    Http(Box<HttpStep>),
    Ws(WsStep),
}

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        // A websocket step is a mapping with a single `ws_*` key; anything else is HTTP.
        // Dispatching by hand keeps serde's error messages (an untagged enum loses them).
        // Reading a JSON value (not a YAML one) keeps keys like `1:` as written.
        let v = Value::deserialize(d)?;
        let ws_tag = v.as_object().and_then(|m| {
            m.keys()
                .find(|k| k.starts_with("ws_"))
                .map(|k| (k.clone(), m.len()))
        });
        let Some((tag, len)) = ws_tag else {
            return serde_json::from_value(v)
                .map(|h| Step::Http(Box::new(h)))
                .map_err(D::Error::custom);
        };
        if len != 1 {
            return Err(D::Error::custom(
                "a websocket step has exactly one key (ws_connect, ws_send, ...)",
            ));
        }
        let body = v[&tag].clone();
        let ws = match tag.as_str() {
            "ws_connect" => serde_json::from_value(body).map(WsStep::Connect),
            "ws_send" => serde_json::from_value(body).map(WsStep::Send),
            "ws_expect" => serde_json::from_value(body).map(WsStep::Expect),
            "ws_expect_silence" => serde_json::from_value(body).map(WsStep::ExpectSilence),
            "ws_expect_close" => serde_json::from_value(body).map(WsStep::ExpectClose),
            other => return Err(D::Error::custom(format!("unknown step {other}"))),
        };
        ws.map(Step::Ws)
            .map_err(|e| D::Error::custom(format!("{tag}: {e}")))
    }
}

impl Serialize for Step {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Step::Http(h) => h.serialize(s),
            Step::Ws(w) => {
                // As written: `- ws_send: {...}` (serde_yaml would emit a `!ws_send` tag).
                use serde::ser::SerializeMap;
                let mut m = s.serialize_map(Some(1))?;
                match w {
                    WsStep::Connect(b) => m.serialize_entry("ws_connect", b)?,
                    WsStep::Send(b) => m.serialize_entry("ws_send", b)?,
                    WsStep::Expect(b) => m.serialize_entry("ws_expect", b)?,
                    WsStep::ExpectSilence(b) => m.serialize_entry("ws_expect_silence", b)?,
                    WsStep::ExpectClose(b) => m.serialize_entry("ws_expect_close", b)?,
                }
                m.end()
            }
        }
    }
}

/// One request and its expected response, in tests-py's fixture format.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpStep {
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

/// A websocket step. Connections are named (`conn`, default `default`) and live until the
/// case ends. In sent messages and connect headers, `{{HASURA_GRAPHQL_ADMIN_SECRET}}` is
/// replaced by the suite's admin secret.
#[derive(Debug, Clone)]
pub enum WsStep {
    /// Open a connection (`protocol`: `graphql-ws` (Apollo legacy) or `graphql-transport-ws`).
    /// (`ws_connect`)
    Connect(WsConnect),
    /// Send a JSON message.
    /// (`ws_send`)
    Send(WsSend),
    /// Wait for the next message with the same `id` as `message` (or with no id) and compare
    /// it. Keep-alives (`ka`, `ping`, `pong`) are skipped; pings are answered.
    /// (`ws_expect`)
    Expect(WsExpect),
    /// No message (with `id`, if given) arrives within `ms`.
    /// (`ws_expect_silence`)
    ExpectSilence(WsSilence),
    /// The server closes the connection, with this code and reason if given.
    /// (`ws_expect_close`)
    ExpectClose(WsClose),
}

fn default_conn() -> String {
    "default".to_owned()
}

fn is_default_conn(c: &str) -> bool {
    c == "default"
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WsConnect {
    #[serde(default = "default_conn", skip_serializing_if = "is_default_conn")]
    pub conn: String,
    #[serde(default = "default_ws_url", skip_serializing_if = "is_default_ws_url")]
    pub url: String,
    pub protocol: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

fn default_ws_url() -> String {
    "/v1/graphql".to_owned()
}

fn is_default_ws_url(u: &str) -> bool {
    u == "/v1/graphql"
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WsSend {
    #[serde(default = "default_conn", skip_serializing_if = "is_default_conn")]
    pub conn: String,
    pub message: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WsExpect {
    #[serde(default = "default_conn", skip_serializing_if = "is_default_conn")]
    pub conn: String,
    pub message: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WsSilence {
    #[serde(default = "default_conn", skip_serializing_if = "is_default_conn")]
    pub conn: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WsClose {
    #[serde(default = "default_conn", skip_serializing_if = "is_default_conn")]
    pub conn: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
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

#[cfg(test)]
mod tests {
    use super::*;

    const CASE: &str = "\
steps:
- url: /v1/graphql
  query:
    variables:
      value:
        1: one
- ws_connect:
    protocol: graphql-ws
- ws_send:
    conn: other
    message:
      type: connection_init
- ws_expect_close:
    code: 4400
";

    #[test]
    fn steps_parse_and_round_trip() {
        let file: CaseFile = serde_yaml::from_str(CASE).unwrap();
        let [
            Step::Http(h),
            Step::Ws(WsStep::Connect(c)),
            Step::Ws(WsStep::Send(s)),
            Step::Ws(WsStep::ExpectClose(x)),
        ] = &file.steps[..]
        else {
            panic!("unexpected steps: {:?}", file.steps);
        };
        assert_eq!(h.query.as_ref().unwrap()["variables"]["value"]["1"], "one");
        assert_eq!(
            (c.conn.as_str(), c.url.as_str()),
            ("default", "/v1/graphql")
        );
        assert_eq!(s.conn, "other");
        assert_eq!(x.code, Some(4400));
        let text = serde_yaml::to_string(&file).unwrap();
        assert!(
            text.contains("- ws_connect:\n    protocol: graphql-ws\n"),
            "{text}"
        );
        let again: CaseFile = serde_yaml::from_str(&text).unwrap();
        assert_eq!(serde_yaml::to_string(&again).unwrap(), text);
    }

    #[test]
    fn ws_step_errors_are_reported() {
        let err = serde_yaml::from_str::<CaseFile>("steps:\n- ws_send:\n    msg: {}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ws_send") && err.contains("msg"), "{err}");
        let err = serde_yaml::from_str::<CaseFile>("steps:\n- ws_bogus: {}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown step ws_bogus"), "{err}");
    }
}
