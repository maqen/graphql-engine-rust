//! V2 log types and levels, and the parsers of `HASURA_GRAPHQL_ENABLED_LOG_TYPES` and
//! `HASURA_GRAPHQL_LOG_LEVEL` (spec 11 §5.1-5.3).

use std::fmt;

/// Log levels, ordered as V2 orders them (`debug < info < warn < error < other`).
/// `Critical` is V2's `LevelOther "critical"` (failed JWK fetches); it cannot be chosen
/// with `HASURA_GRAPHQL_LOG_LEVEL`, and `tracing` has no such level, so an event asks for
/// it with a `level = "critical"` field (see the layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
    Critical,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
            Level::Critical => "critical",
        }
    }

    /// `HASURA_GRAPHQL_LOG_LEVEL`: case-insensitive, surrounding whitespace ignored.
    pub fn parse(s: &str) -> Result<Level, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "debug" => Ok(Level::Debug),
            "info" => Ok(Level::Info),
            "warn" => Ok(Level::Warn),
            "error" => Ok(Level::Error),
            _ => Err("Valid log levels: debug, info, warn or error".to_owned()),
        }
    }

    pub(crate) fn from_tracing(level: &tracing::Level) -> Level {
        match *level {
            tracing::Level::ERROR => Level::Error,
            tracing::Level::WARN => Level::Warn,
            tracing::Level::INFO => Level::Info,
            _ => Level::Debug,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

macro_rules! log_types {
    (
        public { $($pv:ident => $ps:literal,)* }
        internal { $($iv:ident => $is:literal,)* }
    ) => {
        /// Every log type the engine emits. Public types are switched on and off with
        /// `HASURA_GRAPHQL_ENABLED_LOG_TYPES`; internal ones are always on (subject to level).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum LogType {
            $($pv,)*
            $($iv,)*
        }

        impl LogType {
            /// The public types, in V2's order (used in its error message).
            pub const PUBLIC: &'static [LogType] = &[$(LogType::$pv,)*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(LogType::$pv => $ps,)*
                    $(LogType::$iv => $is,)*
                }
            }

            pub fn is_public(self) -> bool {
                matches!(self, $(LogType::$pv)|*)
            }

            /// A type by its V2 name (public or internal).
            pub fn from_name(name: &str) -> Option<LogType> {
                match name {
                    $($ps => Some(LogType::$pv),)*
                    $($is => Some(LogType::$iv),)*
                    _ => None,
                }
            }
        }
    };
}

log_types! {
    public {
        Startup => "startup",
        HttpLog => "http-log",
        WebhookLog => "webhook-log",
        WebsocketLog => "websocket-log",
        QueryLog => "query-log",
        ExecutionLog => "execution-log",
        LivequeryPollerLog => "livequery-poller-log",
        ActionHandlerLog => "action-handler-log",
        DataConnectorLog => "data-connector-log",
        JwkRefreshLog => "jwk-refresh-log",
        ValidateInsertInputLog => "validate-insert-input-log",
    }
    internal {
        Unstructured => "unstructured",
        UnhandledInternalError => "unhandled-internal-error",
        EventTrigger => "event-trigger",
        EventTriggerProcess => "event-trigger-process",
        ScheduledTrigger => "scheduled-trigger",
        ScheduledTriggerProcess => "scheduled-trigger-process",
        CronEventGeneratorProcess => "cron-event-generator-process",
        WsServer => "ws-server",
        PgClient => "pg-client",
        Metadata => "metadata",
        TelemetryLog => "telemetry-log",
        SchemaSync => "schema-sync",
        SourceCatalogMigration => "source-catalog-migration",
        StoredIntrospection => "stored-introspection",
        StoredIntrospectionStorage => "stored-introspection-storage",
        ModelInfo => "model-info",
        RemoteSchemaHealing => "remote-schema-healing",
    }
}

/// Names only the enterprise build knows; accepted (with a warning) so that configuration
/// copied from an EE deployment still starts.
const EE_ONLY: &[&str] = &[
    "health-check-log",
    "event-trigger-log",
    "scheduled-trigger-log",
    "cron-trigger-log",
];

/// A set of log types, as a bitset over [`LogType`]'s discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogTypes(u64);

impl LogTypes {
    /// V2's default: `startup,http-log,webhook-log,websocket-log,jwk-refresh-log`.
    pub const DEFAULT: LogTypes = LogTypes(
        1 << LogType::Startup as u8
            | 1 << LogType::HttpLog as u8
            | 1 << LogType::WebhookLog as u8
            | 1 << LogType::WebsocketLog as u8
            | 1 << LogType::JwkRefreshLog as u8,
    );

    pub fn contains(self, ty: LogType) -> bool {
        self.0 & (1 << ty as u8) != 0
    }

    pub fn insert(&mut self, ty: LogType) {
        self.0 |= 1 << ty as u8;
    }

    /// The enabled public types, in V2's order.
    pub fn names(self) -> Vec<&'static str> {
        LogType::PUBLIC
            .iter()
            .filter(|t| self.contains(**t))
            .map(|t| t.as_str())
            .collect()
    }

    /// Parse `HASURA_GRAPHQL_ENABLED_LOG_TYPES`. Returns the set and the EE-only names that
    /// were ignored. The error is V2's message (after `Error in $: `).
    pub fn parse(s: &str) -> Result<(LogTypes, Vec<String>), String> {
        let mut set = LogTypes(0);
        let mut ignored = vec![];
        for item in s.split(',') {
            let name = item.trim().trim_matches('"').trim().to_ascii_lowercase();
            // The docs spell it `validate-input-log`; the source name wins, both are accepted.
            let name = if name == "validate-input-log" {
                "validate-insert-input-log".to_owned()
            } else {
                name
            };
            match LogType::from_name(&name) {
                Some(ty) if ty.is_public() => set.insert(ty),
                _ if EE_ONLY.contains(&name.as_str()) => ignored.push(name),
                _ => return Err(Self::invalid()),
            }
        }
        Ok((set, ignored))
    }

    fn invalid() -> String {
        let names: Vec<&str> = LogType::PUBLIC.iter().map(|t| t.as_str()).collect();
        format!(
            "Error in $: Valid list of comma-separated log types: {}",
            serde_json::to_string(&names).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_types() {
        assert_eq!(
            LogTypes::DEFAULT.names(),
            [
                "startup",
                "http-log",
                "webhook-log",
                "websocket-log",
                "jwk-refresh-log"
            ]
        );
        assert!(!LogTypes::DEFAULT.contains(LogType::QueryLog));
    }

    #[test]
    fn parse_types() {
        let (set, ignored) = LogTypes::parse(" Startup, query-log ,validate-input-log").unwrap();
        assert_eq!(
            set.names(),
            ["startup", "query-log", "validate-insert-input-log"]
        );
        assert!(ignored.is_empty());
        let (set, ignored) = LogTypes::parse("http-log,event-trigger-log").unwrap();
        assert_eq!(set.names(), ["http-log"]);
        assert_eq!(ignored, ["event-trigger-log"]);
        for bad in ["", "http-log,", "unstructured", "nope"] {
            assert_eq!(
                LogTypes::parse(bad).unwrap_err(),
                "Error in $: Valid list of comma-separated log types: [\"startup\",\"http-log\",\"webhook-log\",\"websocket-log\",\"query-log\",\"execution-log\",\"livequery-poller-log\",\"action-handler-log\",\"data-connector-log\",\"jwk-refresh-log\",\"validate-insert-input-log\"]",
                "{bad}"
            );
        }
    }

    #[test]
    fn parse_level() {
        assert_eq!(Level::parse(" WARN "), Ok(Level::Warn));
        assert_eq!(
            Level::parse("trace").unwrap_err(),
            "Valid log levels: debug, info, warn or error"
        );
        assert!(Level::Debug < Level::Info && Level::Warn < Level::Error);
        assert!(Level::Error < Level::Critical);
        assert!(Level::parse("critical").is_err());
    }
}
