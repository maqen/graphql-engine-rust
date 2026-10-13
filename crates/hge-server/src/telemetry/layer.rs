//! The bridge from `tracing` events to V2-shaped log lines.

use std::fmt::{self, Write as _};
use std::sync::Arc;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

use super::{Level, LogType, Logger};

/// A `tracing` layer writing events as V2 log lines through a [`Logger`].
pub struct V2Layer {
    logger: Arc<Logger>,
}

impl V2Layer {
    pub fn new(logger: Arc<Logger>) -> V2Layer {
        V2Layer { logger }
    }

    fn wants(&self, meta: &Metadata<'_>) -> bool {
        if !meta.is_event() {
            return false;
        }
        let level = Level::from_tracing(meta.level());
        let target = meta.target();
        let ty = log_type(target);
        // Dependencies (hyper, h2, tokio-postgres …) only get through with problems.
        if ty == LogType::Unstructured && !is_own(target) && level < Level::Warn {
            return false;
        }
        self.logger.enabled(ty, level)
    }
}

/// The V2 log type of an event target. Our own module paths and unknown targets are
/// `unstructured`; shutdown messages are `startup` logs as in V2.
fn log_type(target: &str) -> LogType {
    match LogType::from_name(target) {
        Some(ty) => ty,
        None if target == "shutdown" => LogType::Startup,
        None => LogType::Unstructured,
    }
}

/// Our own code: a known log type, one of our other targets, or one of our crates.
fn is_own(target: &str) -> bool {
    LogType::from_name(target).is_some()
        || matches!(target, "shutdown" | "async-actions" | "migrations")
        || target.starts_with("hge_")
        || target.starts_with("graphql_engine")
}

impl<S: Subscriber> Layer<S> for V2Layer {
    fn register_callsite(&self, meta: &'static Metadata<'static>) -> Interest {
        if self.wants(meta) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, meta: &Metadata<'_>, _: Context<'_, S>) -> bool {
        self.wants(meta)
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let meta = event.metadata();
        let level = Level::from_tracing(meta.level());
        let ty = log_type(meta.target());
        let mut fields = Fields::default();
        event.record(&mut fields);
        let detail = fields.into_detail(ty, meta.target());
        self.logger
            .write_with(ty.as_str(), level, |buf| match detail {
                Detail::Raw(raw) => {
                    buf.extend_from_slice(raw.as_bytes());
                    true
                }
                Detail::Value(v) => serde_json::to_writer(&mut *buf, &v).is_ok(),
            });
    }
}

enum Detail {
    /// Pre-serialised, validated JSON from a `detail` field.
    Raw(String),
    Value(Value),
}

#[derive(Default)]
struct Fields {
    message: Option<String>,
    detail: Option<String>,
    kind: Option<String>,
    other: Map<String, Value>,
}

impl Fields {
    fn put(&mut self, field: &Field, value: Value) {
        match (field.name(), value) {
            ("message", Value::String(s)) => self.message = Some(s),
            ("detail", Value::String(s)) => self.detail = Some(s),
            ("kind", Value::String(s)) => self.kind = Some(s),
            (name, v) => {
                self.other.insert(name.to_owned(), v);
            }
        }
    }

    fn into_detail(mut self, ty: LogType, target: &str) -> Detail {
        if let Some(raw) = self.detail.take() {
            match serde_json::from_str::<&serde_json::value::RawValue>(&raw) {
                // One log, one line: re-serialise pretty-printed JSON compactly.
                Ok(v) if v.get().contains(['\n', '\r']) => {
                    if let Ok(v) = serde_json::from_str::<Value>(v.get()) {
                        return Detail::Value(v);
                    }
                }
                Ok(v) => return Detail::Raw(v.get().to_owned()),
                Err(_) => {}
            }
            self.other.insert("detail".to_owned(), Value::String(raw));
        }
        let message = self.message.take().unwrap_or_default();
        match ty {
            // V2's `unstructured` detail is a plain string.
            LogType::Unstructured => {
                let mut text = message;
                if let Some(kind) = &self.kind {
                    self.other
                        .insert("kind".to_owned(), Value::String(kind.clone()));
                }
                let module = target.contains("::").then_some(target);
                if !self.other.is_empty() || module.is_some() {
                    let mut sep = if text.is_empty() { "" } else { " " };
                    for (k, v) in &self.other {
                        let _ = match v {
                            Value::String(s) => write!(text, "{sep}{k}={s}"),
                            v => write!(text, "{sep}{k}={v}"),
                        };
                        sep = " ";
                    }
                    if let Some(m) = module {
                        let _ = write!(text, "{sep}({m})");
                    }
                }
                Detail::Value(Value::String(text))
            }
            // `{"kind": …, "info": …}`; the info is the message alone when there is nothing
            // else to say.
            LogType::Startup => {
                let info = if self.other.is_empty() {
                    Value::String(message)
                } else {
                    let mut info = Map::new();
                    if !message.is_empty() {
                        info.insert("message".to_owned(), Value::String(message));
                    }
                    info.extend(self.other);
                    Value::Object(info)
                };
                let mut detail = Map::new();
                detail.insert(
                    "kind".to_owned(),
                    Value::String(self.kind.unwrap_or_else(|| "server".to_owned())),
                );
                detail.insert("info".to_owned(), info);
                Detail::Value(Value::Object(detail))
            }
            _ => {
                let mut detail = Map::new();
                if let Some(kind) = self.kind {
                    detail.insert("kind".to_owned(), Value::String(kind));
                }
                if !message.is_empty() {
                    detail.insert("message".to_owned(), Value::String(message));
                }
                detail.extend(self.other);
                Detail::Value(Value::Object(detail))
            }
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::String(value.to_owned()));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::Bool(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, Value::String(value.to_string()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, Value::String(format!("{value:?}")));
    }
}

#[cfg(test)]
mod tests {
    use super::super::{LogConfig, LogTypes};
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    fn capture(config: LogConfig, f: impl FnOnce()) -> Vec<Value> {
        let (logger, lines) = Logger::capture(config);
        let subscriber = tracing_subscriber::registry().with(V2Layer::new(Arc::new(logger)));
        tracing::subscriber::with_default(subscriber, f);
        let lines = lines.lock().unwrap();
        lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("valid JSON line"))
            .collect()
    }

    #[test]
    fn typed_targets() {
        let logs = capture(LogConfig::default(), || {
            let detail = serde_json::json!({"status_code": 200, "url": "http://x"});
            tracing::info!(target: "webhook-log", detail = %detail);
            tracing::info!(target: "startup", kind = "catalog_migrate", "Already at the latest catalog version (48); nothing to do.");
            tracing::info!(target: "startup", sources = 1, "metadata loaded");
            tracing::info!(target: "shutdown", "Gracefully shutting down server");
            tracing::warn!(target: "event-trigger", event_id = "e1", "delivery failed");
        });
        let detail: Vec<(&str, &Value)> = logs
            .iter()
            .map(|l| (l["type"].as_str().unwrap(), &l["detail"]))
            .collect();
        assert_eq!(
            detail[0],
            (
                "webhook-log",
                &serde_json::json!({"status_code": 200, "url": "http://x"})
            )
        );
        assert_eq!(
            detail[1].1,
            &serde_json::json!({"kind": "catalog_migrate", "info": "Already at the latest catalog version (48); nothing to do."})
        );
        assert_eq!(
            detail[2].1,
            &serde_json::json!({"kind": "server", "info": {"message": "metadata loaded", "sources": 1}})
        );
        assert_eq!(
            detail[3],
            (
                "startup",
                &serde_json::json!({"kind": "server", "info": "Gracefully shutting down server"})
            )
        );
        assert_eq!(
            detail[4],
            (
                "event-trigger",
                &serde_json::json!({"message": "delivery failed", "event_id": "e1"})
            )
        );
        assert_eq!(logs[4]["level"], "warn");
    }

    #[test]
    fn unstructured_and_filters() {
        let config = LogConfig {
            level: Level::Info,
            types: LogTypes::parse("http-log").unwrap().0,
        };
        let logs = capture(config, || {
            tracing::info!(target: "startup", "filtered: type disabled");
            tracing::debug!(target: "metadata", "filtered: level");
            tracing::info!(target: "hyper::proto", "filtered: dependency below warn");
            tracing::info!(target: "h2", "filtered: dependency crate root");
            tracing::warn!(target: "hyper::proto", "kept");
            tracing::info!(target: "hge_server::load", count = 2, "plain");
            tracing::error!(target: "metadata", "always on");
        });
        let detail: Vec<&Value> = logs.iter().map(|l| &l["detail"]).collect();
        assert_eq!(
            detail,
            [
                &Value::from("kept (hyper::proto)"),
                &Value::from("plain count=2 (hge_server::load)"),
                &serde_json::json!({"message": "always on"}),
            ]
        );
        assert_eq!(logs[0]["type"], "unstructured");
    }

    #[test]
    fn pretty_detail_stays_on_one_line() {
        let logs = capture(LogConfig::default(), || {
            tracing::info!(target: "webhook-log", detail = " {\n \"a\": 1\n}\n");
            tracing::info!(target: "webhook-log", detail = " {\"b\":2} ");
        });
        assert_eq!(logs[0]["detail"], serde_json::json!({"a": 1}));
        assert_eq!(logs[1]["detail"], serde_json::json!({"b": 2}));
    }

    #[test]
    fn invalid_detail_is_kept_as_field() {
        let logs = capture(LogConfig::default(), || {
            tracing::info!(target: "webhook-log", detail = "not json");
        });
        assert_eq!(logs[0]["detail"], serde_json::json!({"detail": "not json"}));
    }
}
