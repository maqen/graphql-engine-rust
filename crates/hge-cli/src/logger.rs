//! Logging in the official CLI's formats (logrus).
//!
//! When stdout is not a terminal every entry is a JSON line on stderr (keys sorted:
//! fields, `level`, `msg`, `time`). On a terminal it is logrus' text format without
//! timestamps, colored unless `--no-color`, and a spinner shows long-running steps.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::gofmt::{json_string, now_rfc3339, quote};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Fatal = 1,
    Error = 2,
    Warn = 3,
    Info = 4,
    Debug = 5,
    Trace = 6,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s.to_ascii_lowercase().as_str() {
            "panic" | "fatal" => Level::Fatal,
            "error" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "info" => Level::Info,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Level::Fatal => "fatal",
            Level::Error => "error",
            Level::Warn => "warning",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }

    fn color(self) -> u8 {
        match self {
            Level::Debug | Level::Trace => 37,
            Level::Warn => 33,
            Level::Error | Level::Fatal => 31,
            Level::Info => 36,
        }
    }
}

pub struct Logger {
    pub level: Level,
    pub terminal: bool,
    pub no_color: bool,
    spinner: Mutex<Option<Spinner>>,
}

struct Spinner {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Logger {
    pub fn new(level: Level, no_color: bool) -> Logger {
        Logger {
            level,
            terminal: std::io::stdout().is_terminal(),
            no_color,
            spinner: Mutex::new(None),
        }
    }

    pub fn info(&self, msg: &str) {
        self.log(Level::Info, msg, &[]);
    }

    pub fn warn(&self, msg: &str) {
        self.log(Level::Warn, msg, &[]);
    }

    pub fn error(&self, msg: &str) {
        self.log(Level::Error, msg, &[]);
    }

    pub fn debug(&self, msg: &str) {
        self.log(Level::Debug, msg, &[]);
    }

    /// Log with extra fields; values are already-rendered strings.
    pub fn log(&self, level: Level, msg: &str, fields: &[(&str, &str)]) {
        let fields: Vec<(&str, Field)> = fields.iter().map(|(k, v)| (*k, Field::Str(v))).collect();
        self.log_fields(level, msg, &fields);
    }

    /// Log with typed fields (numbers are written as JSON numbers).
    pub fn log_fields(&self, level: Level, msg: &str, fields: &[(&str, Field)]) {
        if level > self.level {
            return;
        }
        let was_spinning = self.pause_spinner();
        let line = format_entry(level, msg, fields, self.terminal, self.no_color);
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(line.as_bytes());
        let _ = err.flush();
        drop(err);
        if let Some(prefix) = was_spinning {
            self.spin(&prefix);
        }
    }

    /// `ec.Spin`: a spinner on a terminal, otherwise the message logged at info level.
    pub fn spin(&self, msg: &str) {
        if !self.terminal {
            self.info(msg);
            return;
        }
        self.stop_spinner();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let prefix = msg.to_string();
        let handle = std::thread::spawn(move || {
            let chars = ['◐', '◓', '◑', '◒'];
            let mut i = 0;
            while !flag.load(Ordering::Relaxed) {
                let mut err = std::io::stderr().lock();
                let _ = write!(err, "\r\x1b[K{prefix}{}", chars[i % chars.len()]);
                let _ = err.flush();
                drop(err);
                i += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[K");
            let _ = err.flush();
        });
        if let Ok(mut s) = self.spinner.lock() {
            *s = Some(Spinner { stop, handle });
        }
        if let Ok(mut p) = SPIN_PREFIX.lock() {
            *p = msg.to_string();
        }
    }

    pub fn stop_spinner(&self) {
        self.pause_spinner();
    }

    fn pause_spinner(&self) -> Option<String> {
        let spinner = self.spinner.lock().ok()?.take()?;
        spinner.stop.store(true, Ordering::Relaxed);
        let _ = spinner.handle.join();
        SPIN_PREFIX.lock().ok().map(|p| p.clone())
    }
}

static SPIN_PREFIX: Mutex<String> = Mutex::new(String::new());

/// One log entry in the format the official CLI uses for its logger.
/// A log field value.
#[derive(Clone, Copy, Debug)]
pub enum Field<'a> {
    Str(&'a str),
    Int(i64),
}

impl Field<'_> {
    fn json(&self) -> String {
        match self {
            Field::Str(s) => json_string(s),
            Field::Int(i) => i.to_string(),
        }
    }

    fn text(&self) -> String {
        match self {
            Field::Str(s) => text_value(s),
            Field::Int(i) => i.to_string(),
        }
    }
}

pub fn format_entry(
    level: Level,
    msg: &str,
    fields: &[(&str, Field)],
    terminal: bool,
    no_color: bool,
) -> String {
    if !terminal {
        let mut entries: Vec<(&str, String)> = fields.iter().map(|(k, v)| (*k, v.json())).collect();
        entries.push(("level", json_string(level.name())));
        entries.push(("msg", json_string(msg)));
        entries.push(("time", json_string(&now_rfc3339())));
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let body: Vec<String> = entries
            .iter()
            .map(|(k, v)| format!("{}:{v}", json_string(k)))
            .collect();
        return format!("{{{}}}\n", body.join(","));
    }
    let msg = msg.strip_suffix('\n').unwrap_or(msg);
    let mut sorted: Vec<&(&str, Field)> = fields.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    if no_color {
        let mut out = format!("level={} msg={}", level.name(), text_value(msg));
        for (k, v) in sorted {
            out.push_str(&format!(" {k}={}", v.text()));
        }
        out.push('\n');
        out
    } else {
        let c = level.color();
        let text = level.name().to_uppercase();
        let mut out = format!("\x1b[{c}m{}\x1b[0m {msg:<44} ", &text[..4]);
        for (k, v) in sorted {
            out.push_str(&format!(" \x1b[{c}m{k}\x1b[0m={}", v.text()));
        }
        out.push('\n');
        out
    }
}

/// logrus' `appendValue`: quote unless every character is "safe".
pub fn text_value(s: &str) -> String {
    let safe = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '/' | '@' | '^' | '+'));
    if safe { s.to_string() } else { quote(s) }
}

/// The fatal line `log.Fatal(err)` prints through logrus' standard logger before exiting.
pub fn fatal_line(msg: &str) -> String {
    if std::io::stderr().is_terminal() {
        format!("\x1b[31mFATA\x1b[0m[0000] {msg:<44} \n")
    } else {
        format!(
            "time={} level=fatal msg={}\n",
            quote(&now_rfc3339()),
            text_value(msg)
        )
    }
}
