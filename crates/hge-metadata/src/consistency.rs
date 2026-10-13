//! Consistency of metadata objects that need no database and no other resolved object:
//! the OpenTelemetry configuration.
//!
//! These are V2's schema cache rules for such objects: the server's resolver calls
//! [`check`] with everything else it resolves, so every path that builds the schema cache
//! (startup, metadata commands, `reload_metadata`, `replace_metadata`, `run_sql`, …)
//! reports the same inconsistencies. Inherited roles are checked where permissions are
//! resolved (hge-schema), since they need the roles of every permission.

use hge_core::InconsistentObject;
use serde_json::Value;

use crate::Metadata;

/// Every inconsistency of the OpenTelemetry configuration, in V2's order.
pub fn check(md: &Metadata) -> Vec<InconsistentObject> {
    md.opentelemetry
        .as_ref()
        .map(opentelemetry)
        .unwrap_or_default()
}

/// V2's `buildOpenTelemetry` validation (environment templates are not rendered).
fn opentelemetry(otel: &Value) -> Vec<InconsistentObject> {
    let issue = |sub: &str, reason: String, definition: &Value| {
        InconsistentObject::new(
            "open_telemetry",
            format!("open_telemetry {sub}"),
            reason,
            definition.clone(),
        )
    };
    let mut out = vec![];
    let batch = &otel["batch_span_processor"];
    if batch["max_export_batch_size"]
        .as_i64()
        .is_some_and(|n| n <= 0)
    {
        out.push(issue(
            "batch_span_processor",
            "max_export_batch_size must be a positive integer".into(),
            batch,
        ));
    }
    if otel["status"] != "enabled" {
        return out;
    }
    let exporter = &otel["exporter_otlp"];
    let enabled: Vec<&str> = otel["data_types"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let kinds = [
        (
            "traces",
            "otlp_traces_endpoint",
            "Traces export is enabled but tracing",
        ),
        (
            "metrics",
            "otlp_metrics_endpoint",
            "Metrics export is enabled but metrics",
        ),
        (
            "logs",
            "otlp_logs_endpoint",
            "Logs export is enabled but logs",
        ),
    ];
    for (kind, key, label) in kinds {
        if !enabled.contains(&kind) {
            continue;
        }
        let problem = match exporter.get(key).and_then(Value::as_str) {
            None => Some(format!("{label} endpoint missing")),
            Some(url) if url.contains("{{") => None,
            Some(url) => url_problem(url),
        };
        if let Some(reason) = problem {
            out.push(issue("exporter_otlp", reason, exporter));
            break;
        }
    }
    out
}

/// What http-client's `parseRequest` says about an exporter URL, if it rejects it: it must
/// be an absolute URI (RFC 3986) with an `http` or `https` scheme.
fn url_problem(url: &str) -> Option<String> {
    let Some((scheme, rest)) = url.split_once(':') else {
        return Some("Invalid URL".into());
    };
    let scheme_ok = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    let bytes = rest.as_bytes();
    let mut i = 0;
    let mut chars_ok = true;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            let hex = |k: usize| bytes.get(k).is_some_and(u8::is_ascii_hexdigit);
            if !(hex(i + 1) && hex(i + 2)) {
                chars_ok = false;
                break;
            }
            i += 3;
            continue;
        }
        if !(b.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=".contains(&b)) {
            chars_ok = false;
            break;
        }
        i += 1;
    }
    if !scheme_ok || !chars_ok {
        return Some("Invalid URL".into());
    }
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Some(format!("InvalidUrlException \"{url}\" \"Invalid scheme\""));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn md(v: Value) -> Metadata {
        crate::from_json(v).unwrap()
    }

    #[test]
    fn exporter_urls() {
        assert_eq!(url_problem("foo").as_deref(), Some("Invalid URL"));
        assert_eq!(url_problem("http://a b").as_deref(), Some("Invalid URL"));
        assert_eq!(
            url_problem("localhost:4318").as_deref(),
            Some("InvalidUrlException \"localhost:4318\" \"Invalid scheme\"")
        );
        assert_eq!(url_problem("http://"), None);
        assert_eq!(url_problem("https://x:99/v1/traces"), None);
    }

    #[test]
    fn opentelemetry_rules() {
        let m = md(json!({
            "version": 3, "sources": [],
            "opentelemetry": {"status": "enabled", "data_types": ["traces"],
                "exporter_otlp": {}, "batch_span_processor": {"max_export_batch_size": 0}},
        }));
        let reasons: Vec<String> = check(&m).iter().map(|o| o.reason.clone()).collect();
        assert_eq!(
            reasons,
            [
                "max_export_batch_size must be a positive integer",
                "Traces export is enabled but tracing endpoint missing",
            ]
        );
    }
}
