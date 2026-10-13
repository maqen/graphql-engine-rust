//! Running cases against a prepared [`Slot`].

use serde_json::Value;

use crate::compare;
use crate::suite::{Case, Step, Suite};
use crate::target::Slot;

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
            Ok(()) if record => record_case(slot, case).await,
            Ok(()) => run_case(slot, case).await,
            Err(e) => Err(format!("case setup failed: {e:#}")),
        };
        results.push(result(case, outcome));
    }
    results
}

async fn run_case(slot: &Slot, case: &Case) -> Result<(), String> {
    for (i, step) in case.file.steps.iter().enumerate() {
        let what = step.description.as_deref().unwrap_or(&step.url);
        let actual = send(slot, case, step)
            .await
            .map_err(|e| format!("step {i} ({what}): {e}"))?;
        check(step, &actual).map_err(|e| format!("step {i} ({what}): {e}"))?;
    }
    Ok(())
}

async fn record_case(slot: &Slot, case: &Case) -> Result<(), String> {
    let mut file = case.file.clone();
    for (i, step) in file.steps.iter_mut().enumerate() {
        let actual = send(slot, case, step)
            .await
            .map_err(|e| format!("step {i}: {e}"))?;
        if check(step, &actual).is_ok() {
            continue;
        }
        step.status = actual.status;
        step.response = actual.body;
        for (name, value) in step.resp_headers.iter_mut() {
            *value = actual
                .headers
                .get(name.as_str())
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
        }
    }
    let text = serde_yaml::to_string(&file).map_err(|e| e.to_string())?;
    std::fs::write(&case.path, text).map_err(|e| format!("{}: {e}", case.path.display()))
}

async fn send(slot: &Slot, case: &Case, step: &Step) -> Result<Actual, String> {
    let method = step.method.clone().unwrap_or_else(|| {
        if step.query.is_some() || step.body.is_some() {
            "POST".into()
        } else {
            "GET".into()
        }
    });
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let mut req = slot
        .http()
        .request(method, format!("{}{}", slot.base_url(), step.url));
    let mut has_content_type = false;
    for (k, v) in &step.headers {
        has_content_type |= k.eq_ignore_ascii_case("content-type");
        let v = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        req = req.header(k.as_str(), v);
    }
    let sends_secret = step
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("x-hasura-admin-secret"));
    if case.file.admin_secret_header
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

fn check(step: &Step, actual: &Actual) -> Result<(), String> {
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
        let got = actual
            .headers
            .get(name.as_str())
            .and_then(|v| v.to_str().ok());
        if got != want.as_deref() {
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
