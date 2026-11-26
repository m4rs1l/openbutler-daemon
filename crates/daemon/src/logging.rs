//! The host's structured log: one JSON record per line, rotating by size,
//! with credential-shaped values redacted before anything reaches the disk.
//!
//! Source: `logging.ts` (read 2026-11-26). Levels, the redaction patterns,
//! `formatLogLine`/`parseLogLine`, the file sink's rotation and the
//! `OPENBUTLER_LOG_LEVEL` override are ports; the sink path is the data
//! directory's `logs/openbutler.log`, as in the source.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value, json};

/// The four levels, ordered like the source's `LEVEL_ORDER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Something failed.
    Error,
    /// Something looks wrong but the caller continued.
    Warn,
    /// Normal lifecycle news.
    Info,
    /// Detail for diagnosis.
    Debug,
}

impl Level {
    fn order(self) -> u8 {
        match self {
            Level::Error => 0,
            Level::Warn => 1,
            Level::Info => 2,
            Level::Debug => 3,
        }
    }

    /// The lowercase word the record carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }

    /// Parses the CLI's `--level` values.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "error" => Some(Level::Error),
            "warn" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }
}

/// `(key|token|secret|…)` — a field named like this is redacted whole.
const SECRET_KEY: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
    "cookie",
    "credential",
];

/// One logger: the minimum level and the sink a record goes to.
pub struct Logger {
    level: Level,
    sink: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    context: Map<String, Value>,
}

impl Logger {
    /// The production logger: the data directory's rotating file, level from
    /// `OPENBUTLER_LOG_LEVEL` (defaults to `info`).
    pub fn for_data_dir(data_dir: &Path) -> Self {
        let path = log_path(data_dir);
        let shared = std::sync::Arc::new(Mutex::new(()));
        let sink = move |line: &str| {
            let _guard = shared.lock();
            append_rotated(&path, line);
        };
        Self {
            level: env_level(),
            sink: std::sync::Arc::new(sink),
            context: Map::new(),
        }
    }

    /// A logger writing somewhere else (tests).
    pub fn with_sink(level: Level, sink: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self {
            level,
            sink: std::sync::Arc::new(sink),
            context: Map::new(),
        }
    }

    /// A logger that carries `fields` into every record.
    pub fn child(&self, fields: Map<String, Value>) -> Self {
        let mut context = self.context.clone();
        for (key, value) in fields {
            context.insert(key, value);
        }
        Self {
            level: self.level,
            sink: std::sync::Arc::clone(&self.sink),
            context,
        }
    }

    /// Writes one record when `level` is enabled.
    pub fn log(&self, level: Level, event: &str, fields: Map<String, Value>) {
        if level.order() > self.level.order() {
            return;
        }
        let mut record = Map::new();
        record.insert("ts".to_owned(), json!(now_iso()));
        record.insert("level".to_owned(), json!(level.as_str()));
        record.insert("event".to_owned(), json!(event));
        let mut merged = self.context.clone();
        for (key, value) in fields {
            merged.insert(key, value);
        }
        for (key, value) in redact_object(&merged) {
            record.insert(key, value);
        }
        let line = serde_json::to_string(&Value::Object(record)).unwrap_or_else(|_| {
            format!(
                "{{\"ts\":\"{}\",\"level\":\"{}\",\"event\":\"{event}\",\"unserializable\":true}}",
                now_iso(),
                level.as_str()
            )
        });
        (self.sink)(&line);
    }

    /// `info`: normal lifecycle news.
    pub fn info(&self, event: &str, fields: Map<String, Value>) {
        self.log(Level::Info, event, fields);
    }

    /// `warn`: the caller continued but something looks wrong.
    pub fn warn(&self, event: &str, fields: Map<String, Value>) {
        self.log(Level::Warn, event, fields);
    }

    /// `error`: something failed.
    pub fn error(&self, event: &str, fields: Map<String, Value>) {
        self.log(Level::Error, event, fields);
    }
}

/// The log file inside a data directory.
pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("openbutler.log")
}

/// `OPENBUTLER_LOG_LEVEL`, defaulting to `info` like the source.
fn env_level() -> Level {
    std::env::var("OPENBUTLER_LOG_LEVEL")
        .ok()
        .and_then(|value| Level::parse(&value.to_lowercase()))
        .unwrap_or(Level::Info)
}

/// Appends one line, rotating at 5 MiB and keeping three older files — the
/// source's `fileSink` defaults. A failed write never fails the caller.
fn append_rotated(path: &Path, line: &str) {
    const MAX_BYTES: u64 = 5 * 1024 * 1024;
    const KEEP: usize = 3;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let too_big = std::fs::metadata(path)
        .map(|meta| meta.len() + line.len() as u64 + 1 > MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        for index in (1..KEEP).rev() {
            let from = if index == 1 {
                path.to_path_buf()
            } else {
                PathBuf::from(format!("{}.{}", path.display(), index - 1))
            };
            if from.exists() {
                let _ = std::fs::rename(&from, format!("{}.{index}", path.display()));
            }
        }
        let _ = std::fs::remove_file(format!("{}.{KEEP}", path.display()));
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Replaces anything that looks like a credential, by key name or shape.
pub fn redact(value: &Value, key: Option<&str>) -> Value {
    if let Some(key) = key
        && SECRET_KEY
            .iter()
            .any(|secret| key.to_lowercase().contains(secret))
    {
        return json!("[redacted]");
    }
    match value {
        Value::String(text) => json!(redact_text(text)),
        Value::Array(items) => Value::Array(items.iter().map(|item| redact(item, None)).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (entry_key, entry_value) in map {
                out.insert(entry_key.clone(), redact(entry_value, Some(entry_key)));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn redact_object(map: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in map {
        out.insert(key.clone(), redact(value, Some(key)));
    }
    out
}

/// The value shapes the source scrubs: provider keys, bearer tokens, the web
/// cookie, and any bare 32+ character run.
fn redact_text(text: &str) -> String {
    let mut out = text.to_owned();
    for prefix in ["sk-", "pk-", "rk-"] {
        out = scrub_prefixed(&out, prefix);
    }
    out = scrub_bearer(&out);
    out = scrub_cookie(&out);
    scrub_long_run(&out)
}

fn is_token_char(char: char) -> bool {
    char.is_ascii_alphanumeric() || char == '_' || char == '-'
}

fn scrub_prefixed(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(prefix) {
        let boundary = index == 0 || !rest[..index].chars().next_back().is_some_and(is_token_char);
        out.push_str(&rest[..index]);
        let tail = &rest[index + prefix.len()..];
        let length = tail.chars().take_while(|char| is_token_char(*char)).count();
        if boundary && length >= 8 {
            out.push_str("[redacted]");
            rest = &tail[length..];
        } else {
            out.push_str(prefix);
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn scrub_bearer(text: &str) -> String {
    let lower = text.to_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(index) = lower[cursor..].find("bearer ") {
        let start = cursor + index;
        out.push_str(&text[cursor..start]);
        let tail = &text[start + "bearer ".len()..];
        let length = tail
            .chars()
            .take_while(|char| is_token_char(*char) || *char == '.')
            .count();
        if length >= 8 {
            out.push_str("[redacted]");
            cursor = start + "bearer ".len() + length;
        } else {
            out.push_str(&text[start..start + "bearer ".len()]);
            cursor = start + "bearer ".len();
        }
    }
    out.push_str(&text[cursor..]);
    out
}

fn scrub_cookie(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("openbutler_web=") {
        out.push_str(&rest[..index]);
        let tail = &rest[index + "openbutler_web=".len()..];
        let length = tail
            .chars()
            .take_while(|char| !char.is_whitespace() && *char != ';')
            .count();
        out.push_str("openbutler_web=[redacted]");
        rest = &tail[length..];
    }
    out.push_str(rest);
    out
}

fn scrub_long_run(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for char in text.chars() {
        if is_token_char(char) {
            run.push(char);
            continue;
        }
        if run.chars().count() >= 32 {
            out.push_str("[redacted]");
        } else {
            out.push_str(&run);
        }
        run.clear();
        out.push(char);
    }
    if run.chars().count() >= 32 {
        out.push_str("[redacted]");
    } else {
        out.push_str(&run);
    }
    out
}

/// One record as a person reads it: time, level, event, then the fields
/// (`formatLogLine`, L151). Unparsable lines come back unchanged.
pub fn format_line(line: &str) -> String {
    let Some(record) = parse_line(line) else {
        return line.to_owned();
    };
    let time = record
        .get("ts")
        .and_then(Value::as_str)
        .map(|ts| ts.chars().skip(11).take(12).collect::<String>())
        .unwrap_or_default();
    let level = record
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_uppercase();
    let event = record.get("event").and_then(Value::as_str).unwrap_or("");
    let rest = record
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "ts" | "level" | "event"))
        .map(|(key, value)| match value {
            Value::String(text) => format!("{key}={text}"),
            other => format!("{key}={other}"),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let padding = format!("{level:<5}");
    if rest.is_empty() {
        format!("{time} {padding} {event}")
    } else {
        format!("{time} {padding} {event} {rest}")
    }
}

pub fn parse_line(line: &str) -> Option<Map<String, Value>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let parsed: Value = serde_json::from_str(trimmed).ok()?;
    let map = parsed.as_object()?.clone();
    for key in ["ts", "event", "level"] {
        if !map.get(key).is_some_and(Value::is_string) {
            return None;
        }
    }
    Some(map)
}

/// `now_iso`: the record stamp, millisecond precision, `Z` suffix.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn secrets_are_redacted_by_key_and_by_shape() {
        assert_eq!(
            redact(&json!("anything"), Some("apiKey")),
            json!("[redacted]")
        );
        assert_eq!(
            redact(&json!("Bearer abc"), Some("authorization")),
            json!("[redacted]")
        );
        assert_eq!(
            redact(&json!("sk-abcdefgh12345678"), None),
            json!("[redacted]")
        );
        assert_eq!(
            redact(&json!("Bearer abcdefghijklmnop"), None),
            json!("[redacted]")
        );
        assert_eq!(
            redact(&json!("cookie openbutler_web=abc; path=/"), None),
            json!("cookie openbutler_web=[redacted]; path=/")
        );
        assert_eq!(redact(&json!("short"), None), json!("short"));
    }

    #[test]
    fn a_record_round_trips_through_format_and_parse() {
        let mut fields = Map::new();
        fields.insert("pid".to_owned(), json!(7));
        let wrapped = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = std::sync::Arc::clone(&wrapped);
        let logger = Logger::with_sink(Level::Debug, move |line| {
            sink.lock().unwrap().push(line.to_owned());
        });
        logger.info("host.started", fields);
        let line = wrapped.lock().unwrap()[0].clone();
        let record = parse_line(&line).expect("a record");
        assert_eq!(record["event"], "host.started");
        assert_eq!(record["level"], "info");
        assert_eq!(record["pid"], 7);
        let formatted = format_line(&line);
        assert!(
            formatted.contains("INFO  host.started pid=7"),
            "{formatted}"
        );
        assert_eq!(format_line("not json"), "not json");
    }

    #[test]
    fn the_level_gate_follows_the_source_order() {
        let wrapped = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = std::sync::Arc::clone(&wrapped);
        let logger = Logger::with_sink(Level::Info, move |line| {
            sink.lock().unwrap().push(line.to_owned());
        });
        logger.error("e", Map::new());
        logger.info("i", Map::new());
        logger.log(Level::Debug, "d", Map::new());
        let lines = wrapped.lock().unwrap();
        assert_eq!(lines.len(), 2, "debug is below the level");
        assert!(lines[0].contains("\"event\":\"e\""));
    }
}
