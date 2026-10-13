//! Observability: V2-compatible structured logs (spec 11 §5).
//!
//! Every log is one JSON line on stdout,
//! `{"timestamp":…,"level":…,"type":…,"detail":…}`, filtered by
//! `HASURA_GRAPHQL_LOG_LEVEL` and `HASURA_GRAPHQL_ENABLED_LOG_TYPES` exactly as V2 filters
//! them, so existing log pipelines keep working.
//!
//! Two ways to log:
//! * [`emit`] for structured logs on hot paths (`http-log`): one check of the filter,
//!   then the detail is serialised straight into the line buffer.
//! * `tracing` events anywhere in the workspace. The event's `target` names the V2 log type
//!   (`tracing::info!(target: "webhook-log", …)`); a `detail` field holding JSON
//!   (`detail = %json_value`) becomes the `detail` verbatim, otherwise one is built from the
//!   message and fields. Events with other targets are `unstructured`. Disabled callsites are
//!   switched off in `tracing`'s callsite cache, so their arguments are never evaluated.
//!
//! The line is written by a dedicated thread (see `writer`), never by the request task.

pub mod endpoints;
pub mod http;
mod layer;
pub mod startup;
mod time;
mod types;
mod writer;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde::Serialize;

pub use layer::V2Layer;
pub use types::{Level, LogType, LogTypes};
use writer::Sink;

/// The log filter: minimum level and enabled public log types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogConfig {
    pub level: Level,
    pub types: LogTypes,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            level: Level::Info,
            types: LogTypes::DEFAULT,
        }
    }
}

impl LogConfig {
    /// Whether a log of this type and level is written: level at least the configured one,
    /// and the type enabled or internal.
    pub fn enabled(&self, ty: LogType, level: Level) -> bool {
        level >= self.level && (!ty.is_public() || self.types.contains(ty))
    }
}

/// A configured log destination.
pub struct Logger {
    config: LogConfig,
    sink: Sink,
}

impl Logger {
    /// A logger writing to stdout from a background thread.
    pub fn stdout(config: LogConfig) -> Logger {
        Logger {
            config,
            sink: Sink::stdout(),
        }
    }

    /// A logger keeping its lines in memory (tests).
    pub fn capture(config: LogConfig) -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(vec![]));
        let logger = Logger {
            config,
            sink: Sink::Capture(lines.clone()),
        };
        (logger, lines)
    }

    pub fn config(&self) -> &LogConfig {
        &self.config
    }

    pub fn enabled(&self, ty: LogType, level: Level) -> bool {
        self.config.enabled(ty, level)
    }

    /// Write one log whose detail is `detail` serialised as JSON (no filtering).
    pub fn write(&self, ty: LogType, level: Level, detail: &impl Serialize) {
        self.write_with(ty.as_str(), level, |buf| {
            serde_json::to_writer(&mut *buf, detail).is_ok()
        });
    }

    /// Write one log; `detail` appends the detail JSON to the buffer and returns whether it
    /// succeeded (a failed detail drops the line rather than writing broken JSON).
    pub(crate) fn write_with(
        &self,
        ty: &str,
        level: Level,
        detail: impl FnOnce(&mut Vec<u8>) -> bool,
    ) {
        let mut line = Vec::with_capacity(256);
        line.extend_from_slice(b"{\"timestamp\":\"");
        time::write_timestamp(&mut line, SystemTime::now());
        line.extend_from_slice(b"\",\"level\":\"");
        line.extend_from_slice(level.as_str().as_bytes());
        line.extend_from_slice(b"\",\"type\":\"");
        line.extend_from_slice(ty.as_bytes());
        line.extend_from_slice(b"\",\"detail\":");
        if !detail(&mut line) {
            return;
        }
        line.extend_from_slice(b"}\n");
        self.sink.write(line);
    }

    /// Wait (at most `timeout`) for every line logged so far to be written.
    pub fn flush(&self, timeout: Duration) {
        self.sink.flush(timeout);
    }
}

static LOGGER: OnceLock<Arc<Logger>> = OnceLock::new();
static STARTED: OnceLock<std::time::Instant> = OnceLock::new();

/// Seconds since [`init`] (or since first asked), for `time_taken` in startup logs.
pub fn uptime() -> f64 {
    STARTED
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}

/// The process-wide logger, once [`init`] ran.
pub fn logger() -> Option<&'static Arc<Logger>> {
    LOGGER.get()
}

/// Whether [`emit`] would write a log of this type and level.
#[inline]
pub fn enabled(ty: LogType, level: Level) -> bool {
    LOGGER.get().is_some_and(|l| l.enabled(ty, level))
}

/// Whether a public log type is switched on (whatever the level).
pub fn type_enabled(ty: LogType) -> bool {
    LOGGER
        .get()
        .is_some_and(|l| !ty.is_public() || l.config.types.contains(ty))
}

/// Log `detail` as a `ty` log at `level`, if enabled. Build costly details only after
/// checking [`enabled`].
pub fn emit(ty: LogType, level: Level, detail: &impl Serialize) {
    if let Some(logger) = LOGGER.get().filter(|l| l.enabled(ty, level)) {
        logger.write(ty, level, detail);
    }
}

/// Install the process-wide logger and route `tracing` events through it. Returns a guard
/// that flushes pending lines when dropped (keep it alive until the process exits).
pub fn init(config: LogConfig) -> FlushGuard {
    use tracing_subscriber::layer::SubscriberExt;
    STARTED.get_or_init(std::time::Instant::now);
    let logger = LOGGER
        .get_or_init(|| Arc::new(Logger::stdout(config)))
        .clone();
    let subscriber = tracing_subscriber::registry().with(V2Layer::new(logger));
    // Ignore a second initialisation (tests embedding the server).
    if tracing::subscriber::set_global_default(subscriber).is_ok() {
        // `log` records (tokio-postgres notices, reqwest …) as tracing events; their targets
        // are dependency paths, so only warnings and errors get through.
        let _ = tracing_log::LogTracer::builder()
            .with_max_level(tracing_log::log::LevelFilter::Warn)
            .init();
        // Release builds abort on panic, skipping the guard: flush what is queued first.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            if let Some(logger) = LOGGER.get() {
                logger.flush(Duration::from_millis(500));
            }
        }));
    }
    FlushGuard(())
}

/// Install a process-wide logger that keeps its lines in memory, for tests that check what
/// the server logs. Only the first call installs; later calls return the same lines.
pub fn init_capture(config: LogConfig) -> Arc<Mutex<Vec<String>>> {
    use tracing_subscriber::layer::SubscriberExt;
    static LINES: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();
    LINES
        .get_or_init(|| {
            let (logger, lines) = Logger::capture(config);
            let logger = LOGGER.get_or_init(|| Arc::new(logger)).clone();
            let subscriber = tracing_subscriber::registry().with(V2Layer::new(logger));
            let _ = tracing::subscriber::set_global_default(subscriber);
            lines
        })
        .clone()
}

/// Flushes the process-wide logger on drop.
#[must_use = "dropping the guard flushes the logs; keep it until exit"]
pub struct FlushGuard(());

impl FlushGuard {
    pub fn flush(&self) {
        if let Some(logger) = LOGGER.get() {
            logger.flush(Duration::from_secs(2));
        }
    }
}

impl Drop for FlushGuard {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope() {
        let (logger, lines) = Logger::capture(LogConfig::default());
        logger.write(
            LogType::Startup,
            Level::Info,
            &serde_json::json!({"kind": "server", "info": "hi"}),
        );
        let line = lines.lock().unwrap()[0].clone();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, ["timestamp", "level", "type", "detail"]);
        assert_eq!(v["level"], "info");
        assert_eq!(v["type"], "startup");
        assert_eq!(
            v["detail"],
            serde_json::json!({"kind": "server", "info": "hi"})
        );
        assert_eq!(v["timestamp"].as_str().unwrap().len(), 28);
    }

    #[test]
    fn filtering() {
        let config = LogConfig {
            level: Level::Warn,
            types: LogTypes::parse("startup").unwrap().0,
        };
        assert!(config.enabled(LogType::Startup, Level::Warn));
        assert!(!config.enabled(LogType::Startup, Level::Info));
        assert!(!config.enabled(LogType::HttpLog, Level::Error));
        // Internal types ignore the type list.
        assert!(config.enabled(LogType::Unstructured, Level::Error));
        assert!(config.enabled(LogType::EventTrigger, Level::Warn));
    }
}
