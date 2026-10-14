//! Running cases against a prepared [`Slot`].

use serde_json::Value;

use crate::compare;
use crate::jwt;
use crate::suite::{Case, HttpStep, Step, Suite};
use crate::target::Slot;
use crate::ws;

#[derive(Debug, Clone, serde::Serialize)]
pub struct CaseResult {
    pub id: String,
    pub area: String,
    pub passed: bool,
    /// Why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What a step got back.
struct Actual {
    status: u16,
    body: Value,
    headers: reqwest::header::HeaderMap,
}

/// Run every case of a suite, setting it up first. With `record`, each case file's expected
/// statuses, bodies and listed response headers are rewritten from what the engine returned.
pub async fn run_suite(
    slot: &mut Slot,
    suite: &Suite,
    filter: &(dyn Fn(&str) -> bool + Sync),
    record: bool,
) -> Vec<CaseResult> {
    let cases: Vec<&Case> = suite.cases.iter().filter(|c| filter(&c.id)).collect();
    if cases.is_empty() {
        return vec![];
    }
    let result = |case: &Case, outcome: Result<(), String>| CaseResult {
        id: case.id.clone(),
        area: suite.area.clone(),
        passed: outcome.is_ok(),
        detail: outcome.err(),
    };
    let expanded = match slot.with_helpers(suite).await {
        Ok(e) => e,
        Err(e) => {
            let detail = format!("starting helpers failed: {e:#}");
            return cases
                .iter()
                .map(|c| result(c, Err(detail.clone())))
                .collect();
        }
    };
    let suite = expanded.as_ref().unwrap_or(suite);
    let cases: Vec<&Case> = suite.cases.iter().filter(|c| filter(&c.id)).collect();
    if let Err(e) = slot.setup_suite(suite).await {
        let detail = format!("suite setup failed: {e:#}");
        return cases
            .iter()
            .map(|c| result(c, Err(detail.clone())))
            .collect();
    }
    let mut results = vec![];
    for (i, case) in cases.iter().enumerate() {
        let outcome = match slot.before_case(suite, i == 0).await {
            Ok(()) => run_case(slot, case, record).await,
            Err(e) => Err(format!("case setup failed: {e:#}")),
        };
        results.push(result(case, outcome));
    }
    results
}

/// Run a case's steps in order. With `record`, failed expectations are rewritten from
/// what the engine returned and the case file is saved.
async fn run_case(slot: &Slot, case: &Case, record: bool) -> Result<(), String> {
    let mut file = case.file.clone();
    let mut conns = ws::Connections::default();
    let mut vars = Vars::default();
    let mut outcome = Ok(());
    for (i, step) in file.steps.iter_mut().enumerate() {
        let r = match step {
            Step::Http(step) => var_step(slot, case, step, record, &mut vars).await,
            Step::Ws(step) => {
                ws::run_step(
                    &mut conns,
                    slot.base_url(),
                    slot.admin_secret(),
                    step,
                    record,
                )
                .await
            }
        };
        if let Err(e) = r {
            let what = match step {
                Step::Http(h) => h.description.clone().unwrap_or_else(|| h.url.clone()),
                Step::Ws(w) => ws::label(w),
            };
            outcome = Err(format!("step {i} ({what}): {e}"));
            break;
        }
    }
    conns.close_all().await;
    if record && outcome.is_ok() {
        if !slot.helpers().is_empty() {
            let v = serde_json::to_value(&file).map_err(|e| e.to_string())?;
            file = serde_json::from_value(slot.helpers().unexpand_value(&v))
                .map_err(|e| e.to_string())?;
        }
        let text = serde_yaml::to_string(&file).map_err(|e| e.to_string())?;
        std::fs::write(&case.path, text).map_err(|e| format!("{}: {e}", case.path.display()))?;
    }
    outcome
}

/// Values captured from earlier responses of a case (`capture`), by name.
#[derive(Default)]
struct Vars(std::collections::BTreeMap<String, Value>);

const VAR_PREFIX: &str = "{{var:";

impl Vars {
    fn placeholder(name: &str) -> String {
        format!("{VAR_PREFIX}{name}}}}}")
    }

    /// `{{var:name}}` replaced by the captured value: a string that is just the placeholder
    /// becomes the value itself (a number stays a number), one inside a longer string is
    /// replaced by its text. A placeholder for nothing captured is an error.
    fn expand(&self, v: &Value) -> Result<Value, String> {
        Ok(match v {
            Value::String(s) => {
                if let Some(value) = s
                    .strip_prefix(VAR_PREFIX)
                    .and_then(|r| r.strip_suffix("}}"))
                    .and_then(|name| self.0.get(name))
                {
                    return Ok(value.clone());
                }
                let mut s = s.clone();
                for (name, value) in &self.0 {
                    let text = match value {
                        Value::String(t) => t.clone(),
                        other => other.to_string(),
                    };
                    s = s.replace(&Self::placeholder(name), &text);
                }
                if let Some(i) = s.find(VAR_PREFIX) {
                    return Err(format!("{}: nothing captured under that name", &s[i..]));
                }
                Value::String(s)
            }
            Value::Array(a) => {
                Value::Array(a.iter().map(|v| self.expand(v)).collect::<Result<_, _>>()?)
            }
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, v)| Ok((k.clone(), self.expand(v)?)))
                    .collect::<Result<_, String>>()?,
            ),
            v => v.clone(),
        })
    }

    /// Recorded values: a string equal to a captured string becomes its placeholder again.
    /// Only whole strings, so a short value ("1") does not rewrite unrelated text; captured
    /// numbers and booleans are not written back.
    fn unexpand(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => self
                .0
                .iter()
                .find(|(_, value)| value.as_str() == Some(s.as_str()))
                .map_or_else(
                    || v.clone(),
                    |(name, _)| Value::String(Self::placeholder(name)),
                ),
            Value::Array(a) => Value::Array(a.iter().map(|v| self.unexpand(v)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, v)| (k.clone(), self.unexpand(v)))
                    .collect(),
            ),
            v => v.clone(),
        }
    }
}

/// An HTTP step with `{{var:..}}` replaced by captured values; captures what the step lists.
async fn var_step(
    slot: &Slot,
    case: &Case,
    step: &mut HttpStep,
    record: bool,
    vars: &mut Vars,
) -> Result<(), String> {
    let v = serde_json::to_value(&*step).map_err(|e| e.to_string())?;
    let mut sent: HttpStep = serde_json::from_value(vars.expand(&v)?).map_err(|e| e.to_string())?;
    let body = http_step(slot, case, &mut sent, record).await?;
    if record {
        step.status = sent.status;
        step.response = vars.unexpand(&sent.response);
        for (name, value) in &mut step.resp_headers {
            *value = sent.resp_headers[name].as_ref().map(|h| {
                match vars.unexpand(&Value::String(h.clone())) {
                    Value::String(u) => u,
                    _ => h.clone(),
                }
            });
        }
        // What a step captures differs between runs: it is recorded as a wildcard, and later
        // steps compare against `{{var:..}}`.
        for pointer in step.capture.values() {
            if let Some(slot) = step.response.pointer_mut(pointer) {
                *slot = Value::String(compare::ANY.to_owned());
            }
        }
    }
    for (name, pointer) in &step.capture {
        let value = body
            .pointer(pointer)
            .ok_or_else(|| format!("capture {name}: no {pointer} in the response"))?;
        vars.0.insert(name.clone(), value.clone());
    }
    Ok(())
}

/// Send a step and check its response; returns the response body.
async fn http_step(
    slot: &Slot,
    case: &Case,
    step: &mut HttpStep,
    record: bool,
) -> Result<Value, String> {
    let deadline = match step.retry_for {
        Some(s) => Some(
            std::time::Instant::now()
                + std::time::Duration::try_from_secs_f64(s)
                    .map_err(|e| format!("retry_for: {s}: {e}"))?,
        ),
        None => None,
    };
    let mut actual = send(slot, case, step).await?;
    while let Some(deadline) = deadline {
        if check(step, &actual).is_ok() || std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        actual = send(slot, case, step).await?;
    }
    match check(step, &actual) {
        Ok(()) => Ok(actual.body),
        Err(_) if record => {
            step.status = actual.status;
            step.response = compare::keep_any(&step.response, &actual.body);
            for (name, value) in step.resp_headers.iter_mut() {
                *value = header_value(&actual.headers, name);
            }
            Ok(actual.body)
        }
        Err(e) => Err(e),
    }
}

async fn send(slot: &Slot, case: &Case, step: &HttpStep) -> Result<Actual, String> {
    let method = step.method.clone().unwrap_or_else(|| {
        if step.query.is_some() || step.body.is_some() {
            "POST".into()
        } else {
            "GET".into()
        }
    });
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    // A URL with a scheme (a `{{helper:..}}` one) goes to that server, not the engine.
    let absolute = step.url.starts_with("http://") || step.url.starts_with("https://");
    let url = if absolute {
        step.url.clone()
    } else {
        format!("{}{}", slot.base_url(), step.url)
    };
    let mut req = slot.http().request(method, url);
    let mut has_content_type = false;
    for (k, v) in &step.headers {
        has_content_type |= k.eq_ignore_ascii_case("content-type");
        let v = match v {
            Value::String(s) => s.clone(),
            other => match jwt::header_value(other) {
                Some(token) => token?,
                None => other.to_string(),
            },
        };
        req = req.header(k.as_str(), v);
    }
    let sends_secret = step
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("x-hasura-admin-secret"));
    if case.file.admin_secret_header
        && !absolute
        && !sends_secret
        && let Some(secret) = slot.admin_secret()
    {
        req = req.header("x-hasura-admin-secret", secret);
    }
    let body = match (&step.body, &step.query) {
        (Some(Value::String(raw)), _) => Some(raw.clone()),
        (Some(other), _) => Some(other.to_string()),
        (None, Some(q)) => Some(q.to_string()),
        (None, None) => None,
    };
    if let Some(body) = body {
        if !has_content_type {
            req = req.header("content-type", "application/json");
        }
        req = req.body(body);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let body: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Ok(Actual {
        status,
        body,
        headers,
    })
}

/// A response header's values joined with ", " (as Python's requests reports a repeated
/// header such as Set-Cookie); `None` when absent.
fn header_value(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    let values: Vec<&str> = headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

fn check(step: &HttpStep, actual: &Actual) -> Result<(), String> {
    let mut problems = vec![];
    if actual.status != step.status {
        problems.push(format!("status {} != {}", step.status, actual.status));
    }
    if let Some(d) = compare::diff(
        &step.response,
        &actual.body,
        compare::is_graphql_url(&step.url),
    ) {
        problems.push(format!("body differs at {d}"));
    }
    for (name, want) in &step.resp_headers {
        let got = header_value(&actual.headers, name);
        if got.as_deref() != want.as_deref() {
            problems.push(format!("header {name}: {want:?} != {got:?}"));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{}\n      expected: {}\n      actual:   {}",
        problems.join("; "),
        compare::clip(&step.response.to_string(), 600),
        compare::clip(&actual.body.to_string(), 600),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars() -> Vars {
        let mut v = Vars::default();
        v.0.insert("id".into(), json!("6f0c6a2e-0000-4000-8000-000000000001"));
        v.0.insert("n".into(), json!(1));
        v
    }

    #[test]
    fn expand_and_unexpand() {
        let v = vars();
        let step =
            json!({"variables": {"id": "{{var:id}}", "n": "{{var:n}}", "s": "id={{var:id}}"}});
        let sent = v.expand(&step).unwrap();
        assert_eq!(
            sent,
            json!({"variables": {"id": "6f0c6a2e-0000-4000-8000-000000000001", "n": 1,
                   "s": "id=6f0c6a2e-0000-4000-8000-000000000001"}})
        );
        // Whole strings only, and only captured strings.
        let got = json!({"id": "6f0c6a2e-0000-4000-8000-000000000001", "n": 1, "x": "1", "1": "a"});
        assert_eq!(
            v.unexpand(&got),
            json!({"id": "{{var:id}}", "n": 1, "x": "1", "1": "a"})
        );
    }

    #[test]
    fn unknown_var_is_an_error() {
        assert!(vars().expand(&json!({"q": "{{var:nope}}"})).is_err());
        assert!(Vars::default().expand(&json!(["x {{var:id}}"])).is_err());
    }
}
