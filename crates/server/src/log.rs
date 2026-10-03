//! Structured logging for systemd/journald and log collectors.
//!
//! One record per line on stdout. The default format is a JSON object whose first keys are
//! always `t` (UTC ISO-8601 with milliseconds), `level`, `c` (component) and `msg`, followed by
//! the process base fields and the record's own fields. Under systemd (`JOURNAL_STREAM` set) each
//! line starts with the sd-daemon priority prefix (`<6>` for info...), so journald stores the
//! right priority. Lines are written by a dedicated thread: logging never blocks a runtime
//! thread; when the queue is full the record is dropped and counted.
//!
//! Usage:
//! ```ignore
//! let log = Logger::root().child("auth");
//! log_info!(log, "login", { "user": id, "ip": log::ip(addr) });
//! log_warn!(log, "journal write failed");
//! ```
//!
//! Field values go through [`scrub`]: keys that look sensitive are replaced by `[redacted]`, and
//! token-like substrings inside strings are masked.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};

/// Severity of a record. `Security` sits between warn and error and is kept unless the threshold
/// is `Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 10,
    Info = 20,
    Warn = 30,
    Security = 35,
    Error = 40,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Security => "security",
            Level::Error => "error",
        }
    }

    /// sd-daemon(3) priority used by journald.
    pub fn syslog_priority(self) -> u8 {
        match self {
            Level::Debug => 7,
            Level::Info => 6,
            Level::Security => 5,
            Level::Warn => 4,
            Level::Error => 3,
        }
    }
}

/// Output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Pretty,
}

/// Logging setup, applied once by [`init`].
#[derive(Clone, Debug)]
pub struct Options {
    pub level: Level,
    pub format: Format,
    /// Prefix lines with `<N>` priorities (journald). `None` = detect `JOURNAL_STREAM`.
    pub journald: Option<bool>,
    /// Fields added to every JSON record after `msg` (for example `inst`).
    pub base: Map<String, Value>,
    /// Write to stderr instead of stdout (administration commands).
    pub stderr: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options { level: Level::Info, format: Format::Json, journald: None, base: Map::new(), stderr: false }
    }
}

enum Line {
    Text(String),
    Flush(SyncSender<()>),
}

struct State {
    tx: SyncSender<Line>,
    format: Format,
    journald: bool,
    base: Map<String, Value>,
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static STATE: OnceLock<State> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
static CAPTURE_ON: AtomicBool = AtomicBool::new(false);
static CAPTURE: Mutex<Vec<String>> = Mutex::new(Vec::new());

const QUEUE: usize = 16_384;

/// Configures logging. Later calls only change the level.
pub fn init(opts: Options) {
    LEVEL.store(opts.level as u8, Ordering::Relaxed);
    let journald = opts.journald.unwrap_or_else(|| std::env::var_os("JOURNAL_STREAM").is_some());
    let _ = STATE.get_or_init(|| {
        let (tx, rx) = sync_channel(QUEUE);
        let stderr = opts.stderr;
        std::thread::Builder::new()
            .name("log-writer".into())
            .spawn(move || writer(rx, stderr))
            .expect("cannot start the log writer thread");
        State { tx, format: opts.format, journald, base: opts.base }
    });
}

fn state() -> &'static State {
    STATE.get_or_init(|| {
        let (tx, rx) = sync_channel(QUEUE);
        std::thread::Builder::new()
            .name("log-writer".into())
            .spawn(move || writer(rx, false))
            .expect("cannot start the log writer thread");
        State {
            tx,
            format: Format::Json,
            journald: std::env::var_os("JOURNAL_STREAM").is_some(),
            base: Map::new(),
        }
    })
}

fn writer(rx: Receiver<Line>, stderr: bool) {
    let mut out: Box<dyn std::io::Write> = if stderr {
        Box::new(std::io::BufWriter::new(std::io::stderr()))
    } else {
        Box::new(std::io::BufWriter::new(std::io::stdout()))
    };
    while let Ok(line) = rx.recv() {
        let mut pending = Some(line);
        while let Some(l) = pending.take() {
            match l {
                Line::Text(s) => {
                    let _ = out.write_all(s.as_bytes());
                }
                Line::Flush(done) => {
                    let _ = out.flush();
                    let _ = done.send(());
                }
            }
            pending = rx.try_recv().ok();
        }
        let _ = out.flush();
    }
}

/// Waits until every queued record is written (shutdown).
pub fn flush() {
    let (tx, rx) = sync_channel(1);
    if state().tx.send(Line::Flush(tx)).is_ok() {
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
    }
}

/// Records dropped because the queue was full.
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Current threshold.
pub fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        10 => Level::Debug,
        20 => Level::Info,
        30 => Level::Warn,
        35 => Level::Security,
        _ => Level::Error,
    }
}

/// Keeps every formatted line in memory as well (tests). Returns the lines captured so far and
/// clears them when called with `false`.
pub fn capture(on: bool) -> Vec<String> {
    CAPTURE_ON.store(on, Ordering::SeqCst);
    let mut c = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
    if on { c.clone() } else { std::mem::take(&mut *c) }
}

/// A named logger. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Logger {
    component: Arc<str>,
}

impl Logger {
    /// The root logger (empty component).
    pub fn root() -> Logger {
        Logger { component: Arc::from("") }
    }

    /// A child logger: component `parent.name`, or `name` under the root.
    pub fn child(&self, name: &str) -> Logger {
        if self.component.is_empty() {
            Logger { component: Arc::from(name) }
        } else {
            Logger { component: Arc::from(format!("{}.{}", self.component, name)) }
        }
    }

    pub fn component(&self) -> &str {
        &self.component
    }

    pub fn enabled(&self, level: Level) -> bool {
        level >= self::level()
    }

    /// Writes one record. Prefer the `log_*!` macros, which skip building the fields when the
    /// level is disabled.
    pub fn emit(&self, level: Level, msg: &str, fields: Option<Value>) {
        if !self.enabled(level) {
            return;
        }
        let st = state();
        let mut line = String::with_capacity(160);
        if st.journald {
            let _ = write!(line, "<{}>", level.syslog_priority());
        }
        let t = iso_time(crate::clock::wall_ms());
        match st.format {
            Format::Json => {
                let mut obj = Map::new();
                obj.insert("t".into(), Value::String(t));
                obj.insert("level".into(), Value::String(level.as_str().into()));
                obj.insert("c".into(), Value::String(self.component.to_string()));
                obj.insert("msg".into(), Value::String(msg.to_string()));
                for (k, v) in &st.base {
                    if !obj.contains_key(k) {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                if let Some(Value::Object(f)) = fields.map(|f| scrub(&f)) {
                    for (k, v) in f {
                        if !matches!(k.as_str(), "t" | "level" | "c" | "msg") {
                            obj.insert(k, v);
                        }
                    }
                }
                line.push_str(&Value::Object(obj).to_string());
            }
            Format::Pretty => {
                if !st.journald {
                    line.push_str(&t);
                    line.push(' ');
                }
                let _ = write!(line, "{:<8} [{}] {}", level.as_str().to_uppercase(), self.component, msg);
                if let Some(f) = fields {
                    line.push(' ');
                    line.push_str(&scrub(&f).to_string());
                }
            }
        }
        line.push('\n');
        if CAPTURE_ON.load(Ordering::Relaxed) {
            CAPTURE.lock().unwrap_or_else(|e| e.into_inner()).push(line.clone());
        }
        match st.tx.try_send(Line::Text(line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// `log_debug!(logger, "msg")` or `log_debug!(logger, "msg", { "key": value, ... })`.
#[macro_export]
macro_rules! log_debug {
    ($l:expr, $msg:expr $(,)?) => { $l.emit($crate::log::Level::Debug, $msg, None) };
    ($l:expr, $msg:expr, { $($f:tt)* } $(,)?) => {
        if $l.enabled($crate::log::Level::Debug) {
            $l.emit($crate::log::Level::Debug, $msg, Some(::serde_json::json!({ $($f)* })))
        }
    };
}

/// See [`log_debug!`].
#[macro_export]
macro_rules! log_info {
    ($l:expr, $msg:expr $(,)?) => { $l.emit($crate::log::Level::Info, $msg, None) };
    ($l:expr, $msg:expr, { $($f:tt)* } $(,)?) => {
        if $l.enabled($crate::log::Level::Info) {
            $l.emit($crate::log::Level::Info, $msg, Some(::serde_json::json!({ $($f)* })))
        }
    };
}

/// See [`log_debug!`].
#[macro_export]
macro_rules! log_warn {
    ($l:expr, $msg:expr $(,)?) => { $l.emit($crate::log::Level::Warn, $msg, None) };
    ($l:expr, $msg:expr, { $($f:tt)* } $(,)?) => {
        if $l.enabled($crate::log::Level::Warn) {
            $l.emit($crate::log::Level::Warn, $msg, Some(::serde_json::json!({ $($f)* })))
        }
    };
}

/// Security events (failed logins, blocks, sanctions). See [`log_debug!`].
#[macro_export]
macro_rules! log_security {
    ($l:expr, $msg:expr $(,)?) => { $l.emit($crate::log::Level::Security, $msg, None) };
    ($l:expr, $msg:expr, { $($f:tt)* } $(,)?) => {
        if $l.enabled($crate::log::Level::Security) {
            $l.emit($crate::log::Level::Security, $msg, Some(::serde_json::json!({ $($f)* })))
        }
    };
}

/// See [`log_debug!`].
#[macro_export]
macro_rules! log_error {
    ($l:expr, $msg:expr $(,)?) => { $l.emit($crate::log::Level::Error, $msg, None) };
    ($l:expr, $msg:expr, { $($f:tt)* } $(,)?) => {
        if $l.enabled($crate::log::Level::Error) {
            $l.emit($crate::log::Level::Error, $msg, Some(::serde_json::json!({ $($f)* })))
        }
    };
}

/// Formats Unix milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn iso_time(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

const SENSITIVE: &[&str] = &[
    "pass", "token", "secret", "otp", "cookie", "authorization", "recovery", "totp", "verifier",
    "private", "credential",
];

fn sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k == "code" || SENSITIVE.iter().any(|s| k.contains(s))
}

/// Masks `sct_...`, `swt_...`, `mfa_...` and `sso_...` tokens inside a string and caps its length.
pub fn scrub_str(s: &str) -> String {
    let cut: String = s.chars().take(4096).collect();
    let mut out = String::with_capacity(cut.len());
    let b = cut.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let word_start = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        if word_start && i + 4 <= b.len() && b[i + 3] == b'_' && matches!(&b[i..i + 3], b"sct" | b"swt" | b"mfa" | b"sso") {
            let mut j = i + 4;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'-') {
                j += 1;
            }
            if j - (i + 4) >= 8 {
                out.push_str(&cut[i..i + 4]);
                out.push_str("[redacted]");
                i = j;
                continue;
            }
        }
        let ch = cut[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    if out.chars().count() > 2000 || s.chars().count() > 4096 {
        let mut c: String = out.chars().take(2000).collect();
        c.push('…');
        return c;
    }
    out
}

/// Redacts a field tree: sensitive keys, token-like strings, depth over 4, arrays over 50 items.
pub fn scrub(v: &Value) -> Value {
    scrub_at(v, 0)
}

fn scrub_at(v: &Value, depth: usize) -> Value {
    match v {
        Value::String(s) => Value::String(scrub_str(s)),
        Value::Array(_) | Value::Object(_) if depth > 4 => Value::String("[depth]".into()),
        Value::Array(a) => Value::Array(a.iter().take(50).map(|x| scrub_at(x, depth + 1)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| {
                    let val = if sensitive_key(k) { Value::String("[redacted]".into()) } else { scrub_at(x, depth + 1) };
                    (k.clone(), val)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso_time_formats_utc_milliseconds() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_time(1_767_225_600_123), "2026-01-01T00:00:00.123Z");
        assert_eq!(iso_time(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn scrub_masks_keys_and_tokens() {
        let v = scrub(&json!({"password": "x", "code": 12, "errorCode": 3, "note": "use sct_ABCDEFGH123 now"}));
        assert_eq!(v["password"], "[redacted]");
        assert_eq!(v["code"], "[redacted]");
        assert_eq!(v["errorCode"], 3);
        assert_eq!(v["note"], "use sct_[redacted] now");
        assert_eq!(scrub_str("sct_short"), "sct_short");
    }
}
