//! Helper servers: the webhooks and GraphQL services upstream's tests-py starts next to the
//! engine (auth webhooks, remote schemas, action and event handlers).
//!
//! A suite lists the helpers it needs (`helpers: [webhook]`). Each one is
//! `conformance/helpers/<name>.py`, run as `python3 <name>.py <port>` (or with
//! `$CONFORMANCE_PYTHON`) and serving HTTP on `127.0.0.1:<port>`. A slot starts a helper the
//! first time one of its suites needs it and keeps it until the slot finishes, so the engine's
//! environment stays the same from suite to suite and the engine is reused.
//!
//! `{{helper:<name>}}` in the suite's env, its metadata and its cases becomes the helper's
//! URL, `http://127.0.0.1:<port>`. Recording turns the URL back into the placeholder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::process::{Child, Command};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where the helper scripts are (`conformance/helpers`, next to the suites).
pub fn set_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

fn dir() -> &'static Path {
    DIR.get()
        .map(PathBuf::as_path)
        .unwrap_or(Path::new("conformance/helpers"))
}

/// Check a helper name from a suite: a script of that name must exist.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("bad helper name {name:?}");
    }
    let script = dir().join(format!("{name}.py"));
    if !script.exists() {
        bail!("helper {name}: {} not found", script.display());
    }
    Ok(())
}

struct Helper {
    port: u16,
    child: Child,
}

/// The helpers one slot is running.
#[derive(Default)]
pub struct Helpers {
    running: BTreeMap<String, Helper>,
}

impl Helpers {
    /// Start every helper in `names` that is not running yet (restarting one that died).
    pub async fn ensure(&mut self, slot: &str, names: &[String]) -> Result<()> {
        for name in names {
            if let Some(h) = self.running.get_mut(name)
                && h.child.try_wait()?.is_none()
            {
                continue;
            }
            let helper = start(slot, name)
                .await
                .with_context(|| format!("helper {name}"))?;
            self.running.insert(name.clone(), helper);
        }
        Ok(())
    }

    fn url(port: u16) -> String {
        format!("http://127.0.0.1:{port}")
    }

    /// Replace `{{helper:<name>}}` with the URL of helper `name`, one of `listed` (the suite's
    /// `helpers`), which must be running.
    pub fn expand(&self, listed: &[String], s: &str) -> Result<String, String> {
        replace_placeholders(s, |name| {
            if !listed.iter().any(|l| l == name) {
                return Err(not_listed(name));
            }
            let h = self
                .running
                .get(name)
                .ok_or_else(|| format!("helper {name} is not running"))?;
            Ok(Self::url(h.port))
        })
    }

    /// [`Self::expand`] every string in `v`, keys included.
    /// A string that is exactly `{{helper_port:<name>}}` becomes the port number (error
    /// details such as `{host, port}` give it on its own).
    pub fn expand_value(&self, listed: &[String], v: &Value) -> Result<Value, String> {
        let v = map_strings(v, &mut |s| self.expand(listed, s))?;
        self.expand_ports(listed, v)
    }

    fn expand_ports(&self, listed: &[String], v: Value) -> Result<Value, String> {
        Ok(match v {
            Value::String(s) => match port_placeholder(&s) {
                Some(name) => {
                    if !listed.iter().any(|l| l == name) {
                        return Err(not_listed(name));
                    }
                    let h = self
                        .running
                        .get(name)
                        .ok_or_else(|| format!("helper {name} is not running"))?;
                    Value::from(h.port)
                }
                None => Value::String(s),
            },
            Value::Array(a) => Value::Array(
                a.into_iter()
                    .map(|v| self.expand_ports(listed, v))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(o) => Value::Object(
                o.into_iter()
                    .map(|(k, v)| Ok((k, self.expand_ports(listed, v)?)))
                    .collect::<Result<_, String>>()?,
            ),
            v => v,
        })
    }

    /// Turn helper URLs in `v` back into placeholders (for recorded responses).
    pub fn unexpand_value(&self, v: &Value) -> Value {
        let pairs: Vec<(String, String)> = self
            .running
            .iter()
            .map(|(name, h)| (Self::url(h.port), format!("{{{{helper:{name}}}}}")))
            .collect();
        let v = map_strings(v, &mut |s| {
            let mut s = s.to_owned();
            for (url, placeholder) in &pairs {
                s = replace_url(&s, url, placeholder);
            }
            Ok::<_, String>(s)
        })
        .expect("infallible");
        self.unexpand_ports(v)
    }

    /// A `port` field holding a helper's port becomes `{{helper_port:<name>}}`.
    fn unexpand_ports(&self, v: Value) -> Value {
        match v {
            Value::Array(a) => {
                Value::Array(a.into_iter().map(|v| self.unexpand_ports(v)).collect())
            }
            Value::Object(o) => Value::Object(
                o.into_iter()
                    .map(|(k, v)| {
                        let port = (k == "port").then(|| v.as_u64()).flatten().and_then(|p| {
                            self.running.iter().find(|(_, h)| u64::from(h.port) == p)
                        });
                        let v = match port {
                            Some((name, _)) => Value::String(format!("{{{{helper_port:{name}}}}}")),
                            None => self.unexpand_ports(v),
                        };
                        (k, v)
                    })
                    .collect(),
            ),
            v => v,
        }
    }

    /// Whether any helper is running (nothing to expand otherwise).
    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    pub async fn stop_all(&mut self) {
        for (_, mut h) in std::mem::take(&mut self.running) {
            let _ = h.child.kill().await;
        }
    }
}

/// The helper name of a string that is exactly `{{helper_port:<name>}}`.
fn port_placeholder(s: &str) -> Option<&str> {
    s.strip_prefix("{{helper_port:")?.strip_suffix("}}")
}

fn not_listed(name: &str) -> String {
    format!("{{{{helper:{name}}}}} used, but the suite does not list {name} in helpers")
}

/// Replace every `{{helper:<name>}}` in `s` with `url(name)`.
fn replace_placeholders(
    s: &str,
    mut url: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    const OPEN: &str = "{{helper:";
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        let after = &rest[i + OPEN.len()..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("unclosed {OPEN} in {s:?}"))?;
        out.push_str(&url(&after[..end])?);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Check that every `{{helper:<name>}}` in a suite's file names one of its `helpers`, so a
/// placeholder never reaches the engine unexpanded.
pub fn check_uses(text: &str, listed: &[String]) -> Result<(), String> {
    const PORT: &str = "{{helper_port:";
    let mut rest = text;
    while let Some(i) = rest.find(PORT) {
        let after = &rest[i + PORT.len()..];
        let name = &after[..after.find("}}").unwrap_or(after.len())];
        if !listed.iter().any(|l| l == name) {
            return Err(not_listed(name));
        }
        rest = after;
    }
    replace_placeholders(text, |name| {
        if listed.iter().any(|l| l == name) {
            Ok(String::new())
        } else {
            Err(not_listed(name))
        }
    })
    .map(drop)
}

/// Replace `url` in `s` where it is not followed by another digit of a longer port.
fn replace_url(s: &str, url: &str, with: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(url) {
        let after = &rest[i + url.len()..];
        out.push_str(&rest[..i]);
        if after.starts_with(|c: char| c.is_ascii_digit()) {
            out.push_str(url);
        } else {
            out.push_str(with);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn map_strings<E>(v: &Value, f: &mut impl FnMut(&str) -> Result<String, E>) -> Result<Value, E> {
    Ok(match v {
        Value::String(s) => Value::String(f(s)?),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|v| map_strings(v, f))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| Ok((f(k)?, map_strings(v, f)?)))
                .collect::<Result<_, E>>()?,
        ),
        v => v.clone(),
    })
}

/// Where a slot writes its engine's URL and admin secret (`{"url": ..., "admin_secret": ...}`)
/// once the engine serves a suite; helpers get the path as `$HGE_CONFORMANCE_ENGINE`.
pub fn engine_file(slot: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hge-conformance-{slot}-engine.json"))
}

async fn start(slot: &str, name: &str) -> Result<Helper> {
    check_name(name)?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let log = std::env::temp_dir().join(format!("hge-conformance-{slot}-{name}.log"));
    // Appended to, so a restarted helper keeps the log of the run that died.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let python = std::env::var("CONFORMANCE_PYTHON").unwrap_or_else(|_| "python3".into());
    let mut child = Command::new(&python)
        .arg(dir().join(format!("{name}.py")))
        .arg(port.to_string())
        .env("PYTHONUNBUFFERED", "1")
        .env("HGE_CONFORMANCE_ENGINE", engine_file(slot))
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {python}"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            // Something else could have taken the port before the helper bound it; the
            // helper then exits on its bind error.
            tokio::time::sleep(Duration::from_millis(200)).await;
            if child.try_wait()?.is_none() {
                return Ok(Helper { port, child });
            }
        }
        let failed = if let Some(status) = child.try_wait()? {
            Some(format!("exited ({status})"))
        } else if tokio::time::Instant::now() > deadline {
            let _ = child.kill().await;
            Some("not listening after 20 s".to_owned())
        } else {
            None
        };
        if let Some(why) = failed {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            bail!(
                "{why}:\n{}",
                lines[lines.len().saturating_sub(20)..].join("\n")
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn helpers(ports: &[(&str, u16)]) -> Helpers {
        let mut h = Helpers::default();
        for (name, port) in ports {
            let child = Command::new("true").spawn().unwrap();
            h.running
                .insert((*name).to_owned(), Helper { port: *port, child });
        }
        h
    }

    #[tokio::test]
    async fn placeholders_expand_and_come_back() {
        let h = helpers(&[("webhook", 4001), ("graphql_server", 4002)]);
        let v = json!({"url": "{{helper:webhook}}/hook", "{{helper:graphql_server}}": ["x {{helper:webhook}}"], "n": 1});
        let listed = ["webhook".to_owned(), "graphql_server".to_owned()];
        let e = h.expand_value(&listed, &v).unwrap();
        assert_eq!(
            e,
            json!({"url": "http://127.0.0.1:4001/hook", "http://127.0.0.1:4002": ["x http://127.0.0.1:4001"], "n": 1})
        );
        assert_eq!(h.unexpand_value(&e), v);
        assert!(
            h.expand(&listed, "{{helper:other}}")
                .unwrap_err()
                .contains("helpers")
        );
        // Running for this slot, but not listed by this suite.
        assert!(h.expand(&listed[1..], "{{helper:webhook}}").is_err());
        assert!(h.expand(&listed, "{{helper:webhook").is_err());
        assert_eq!(h.expand(&listed, "plain").unwrap(), "plain");
        assert_eq!(
            h.unexpand_value(&json!("http://127.0.0.1:40011 http://127.0.0.1:4001/")),
            json!("http://127.0.0.1:40011 {{helper:webhook}}/")
        );
        assert!(check_uses("a {{helper:webhook}}", &listed).is_ok());
        assert!(check_uses("{{helper_port:graphql_server}}", &listed).is_ok());
        assert!(check_uses("{{helper_port:x}}", &listed).is_err());
        let detail = json!({"host": "127.0.0.1", "port": 4002, "other": {"port": 4003}});
        let recorded = h.unexpand_value(&detail);
        assert_eq!(
            recorded,
            json!({"host": "127.0.0.1", "port": "{{helper_port:graphql_server}}", "other": {"port": 4003}})
        );
        assert_eq!(h.expand_value(&listed, &recorded).unwrap(), detail);
        assert!(check_uses("a {{helper:x}}", &listed).is_err());
    }

    #[test]
    fn names_are_checked() {
        assert!(check_name("../x").is_err());
        assert!(check_name("").is_err());
    }

    #[tokio::test]
    async fn a_helper_starts_and_serves() {
        set_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/helpers"));
        let mut h = Helpers::default();
        h.ensure("test", &["webhook".to_owned()]).await.unwrap();
        let port = h.running["webhook"].port;
        // No Authorization header: upstream's webhook answers 401.
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        h.stop_all().await;
    }
}
