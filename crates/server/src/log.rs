//! Structured logging for systemd/journald and log collectors (DESIGN 8, privacy rules).
//!
//! One record per line on stdout (stderr for administration commands). The default format is a
//! JSON object whose first keys are always `t` (UTC ISO-8601 with milliseconds), `level`, `c`
//! (component) and `msg`, followed by the process base fields (`{"inst": ..., "proc": "server"}`)
//! and the record's own fields; a field never replaces `t`, `level`, `c` or `msg`, and a field
//! named like a base field replaces its value in place. `LOG_FORMAT=pretty` prints
//! `<t> <LEVEL padded to 8> [<component>] <msg> <fields as JSON>` (no base fields). Under systemd
//! (`JOURNAL_STREAM` naming the output stream) each line starts with its sd-daemon priority
//! (`<6>` for info, `<5>` for security...) and pretty lines drop the timestamp journald adds.
//!
//! Lines are written by a dedicated thread through a bounded queue: logging never blocks a
//! runtime thread; when the queue is full the record is dropped and counted ([`dropped`]).
//!
//! ```ignore
//! let log = Logger::root().child("auth");
//! log_security!(log, "login_failed", { "userId": id, "ip": log::ip(addr) });
//! log_error!(log, "e-mail not sent", { "template": name, "err": log::error(&e) });
//! ```
//!
//! Privacy, enforced here so that no call site can forget it (the former server's rules, bit for
//! bit):
//! * a field whose name contains `pass`, `token`, `secret`, `otp`, `cookie`, `authorization`,
//!   `recovery`, `verifier`, `private` or `credential` (ASCII case-insensitive), or is exactly
//!   `code`, is replaced by `"[redacted]"` at any depth. The match is a substring match on
//!   purpose (it also hides `authForgotPerHour` or `privateGameTtlMs`): name fields accordingly
//!   (`errorCode`, not `code`);
//! * `sct_`, `swt_`, `mfa_` and `sso_` tokens inside strings are masked (`sct_[redacted]`); only
//!   the first 4096 UTF-16 units of a string are kept, then at most 2000 followed by `…`;
//! * nesting deeper than 4 levels prints `"[depth]"`, arrays keep their first 50 items;
//! * client addresses go through [`ip`] (`LOG_IP`: truncated to the /24 or /48, full, or hashed
//!   with a daily key derived from `SERVER_SECRET`).
//!
//! A `null` field value is left out of the record, like a JavaScript `undefined`, so `Option`
//! fields (`"ip": log::ip(addr)`) simply vanish when empty. Errors go through [`error`], which
//! prints `{name, message, code, stack}` as the Node server printed its `Error` objects (the
//! `code` of an error is not redacted).

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, LazyLock, OnceLock};

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::config::{self, Config};
use crate::util::{encoding, errno, hash, js, json};

/// Severity of a record. `Security` sits between warn and error and is kept unless the threshold
/// is `Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Diagnosis detail (the access log of every request).
    Debug = 10,
    /// Normal operation.
    Info = 20,
    /// Something to look at.
    Warn = 30,
    /// Security-relevant events: failed logins, blocks, anomalies, sanctions.
    Security = 35,
    /// Failures.
    Error = 40,
}

impl Level {
    /// The name printed in records (`debug`, `info`, `warn`, `security`, `error`).
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Security => "security",
            Level::Error => "error",
        }
    }

    /// Parses a level name as printed by [`Level::as_str`].
    pub fn parse(name: &str) -> Option<Level> {
        Some(match name {
            "debug" => Level::Debug,
            "info" => Level::Info,
            "warn" => Level::Warn,
            "security" => Level::Security,
            "error" => Level::Error,
            _ => return None,
        })
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

    fn from_u8(v: u8) -> Level {
        match v {
            10 => Level::Debug,
            20 => Level::Info,
            30 => Level::Warn,
            35 => Level::Security,
            _ => Level::Error,
        }
    }
}

/// Output format (`LOG_FORMAT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// One JSON object per line.
    Json,
    /// Readable text.
    Pretty,
}

/// How client addresses appear in the logs (`LOG_IP`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpMode {
    /// IPv4 /24, IPv6 /48 (`203.0.113.0/24`, `2001:db8:1::/48`).
    Truncated,
    /// The address as given.
    Full,
    /// `ip:` and 12 characters of a keyed HMAC, the key derived from `SERVER_SECRET` and the UTC
    /// day (the same address gives the same text for a day).
    Hashed,
}

/// Logging setup, applied by [`init`].
#[derive(Clone, Debug)]
pub struct Options {
    /// Records below this level are dropped.
    pub level: Level,
    /// Output format.
    pub format: Format,
    /// Prefix lines with `<N>` priorities (journald). `None` = detect `JOURNAL_STREAM`.
    pub journald: Option<bool>,
    /// Fields added to every JSON record after `msg` (`inst`, `proc`).
    pub base: Map<String, Value>,
    /// Write to stderr instead of stdout (administration commands).
    pub stderr: bool,
    /// How [`ip`] prints client addresses.
    pub ip_mode: IpMode,
    /// Key material of [`IpMode::Hashed`] (`SERVER_SECRET`); `None` uses a fixed key.
    pub ip_secret: Option<Vec<u8>>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            level: Level::Info,
            format: Format::Json,
            journald: None,
            base: Map::new(),
            stderr: false,
            ip_mode: IpMode::Truncated,
            ip_secret: None,
        }
    }
}

impl Options {
    /// The server's logging: `LOG_LEVEL`, `LOG_FORMAT`, `LOG_IP`, base fields
    /// `{"inst": INSTANCE_ID, "proc": "server"}`, hashed addresses keyed by `SERVER_SECRET`.
    pub fn from_config(cfg: &Config) -> Options {
        let mut base = Map::new();
        base.insert("inst".into(), Value::String(cfg.instance_id.clone()));
        base.insert("proc".into(), Value::String("server".into()));
        Options {
            level: match cfg.log_level {
                config::LogLevel::Debug => Level::Debug,
                config::LogLevel::Info => Level::Info,
                config::LogLevel::Warn => Level::Warn,
                config::LogLevel::Error => Level::Error,
            },
            format: match cfg.log_format {
                config::LogFormat::Json => Format::Json,
                config::LogFormat::Pretty => Format::Pretty,
            },
            journald: None,
            base,
            stderr: false,
            ip_mode: match cfg.log_ip {
                config::LogIp::Truncated => IpMode::Truncated,
                config::LogIp::Full => IpMode::Full,
                config::LogIp::Hashed => IpMode::Hashed,
            },
            ip_secret: Some(cfg.server_secret.bytes().to_vec()),
        }
    }

    /// Administration commands: warnings and errors only, readable text on stderr,
    /// `{"proc": "admin"}`.
    pub fn admin() -> Options {
        let mut base = Map::new();
        base.insert("proc".into(), Value::String("admin".into()));
        Options { level: Level::Warn, format: Format::Pretty, base, stderr: true, ..Options::default() }
    }
}

struct Settings {
    format: Format,
    journald: bool,
    base: Map<String, Value>,
    stderr: bool,
}

enum Line {
    Text { text: String, stderr: bool },
    Flush(SyncSender<()>),
}

/// Unit tests of this crate log errors only unless a test asks for more (as the former test
/// configuration did with `LOG_LEVEL=error`).
const DEFAULT_LEVEL: Level = if cfg!(test) { Level::Error } else { Level::Info };
const QUEUE: usize = 16_384;
const MARKER_ERROR: &str = "\u{0}scacelith.error";

static LEVEL: AtomicU8 = AtomicU8::new(DEFAULT_LEVEL as u8);
static SETTINGS: LazyLock<ArcSwap<Settings>> = LazyLock::new(|| {
    ArcSwap::from_pointee(Settings {
        format: Format::Json,
        journald: crate::systemd::journald(),
        base: Map::new(),
        stderr: false,
    })
});
static WRITER: OnceLock<SyncSender<Line>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
static CAPTURE_ON: AtomicBool = AtomicBool::new(false);
static CAPTURE: Mutex<Vec<String>> = Mutex::new(Vec::new());
static CAPTURE_SERIAL: Mutex<()> = Mutex::new(());
static IP_MODE: AtomicU8 = AtomicU8::new(0);
static IP_KEY: Mutex<IpKey> = Mutex::new(IpKey { secret: None, day: i64::MIN, key: [0; 32] });

struct IpKey {
    secret: Option<Vec<u8>>,
    day: i64,
    key: [u8; 32],
}

/// Configures logging. Every call replaces the previous setup (the writer thread stays).
pub fn init(opts: Options) {
    LEVEL.store(opts.level as u8, Ordering::Relaxed);
    let journald = opts.journald.unwrap_or_else(|| {
        if opts.stderr { crate::systemd::journald_stderr() } else { crate::systemd::journald() }
    });
    SETTINGS.store(Arc::new(Settings {
        format: opts.format,
        journald,
        base: opts.base,
        stderr: opts.stderr,
    }));
    IP_MODE.store(
        match opts.ip_mode {
            IpMode::Truncated => 0,
            IpMode::Full => 1,
            IpMode::Hashed => 2,
        },
        Ordering::Relaxed,
    );
    let mut k = IP_KEY.lock();
    k.secret = opts.ip_secret;
    k.day = i64::MIN;
}

/// Logs panics as error records (priority 3 under journald) instead of the default plain text on
/// stderr, which journald stores at priority info. The default hook still runs afterwards when
/// `RUST_BACKTRACE` asks for a backtrace.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        crate::log_error!(Logger::root().child("panic"), "panic", {
            "thread": thread.name().unwrap_or("unnamed"),
            "location": info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
            "message": info.payload_as_str().unwrap_or("(no message)"),
        });
        flush();
        if std::env::var_os("RUST_BACKTRACE").is_some_and(|v| v != "0") {
            default(info);
        }
    }));
}

/// Changes the threshold only.
pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Current threshold.
pub fn level() -> Level {
    Level::from_u8(LEVEL.load(Ordering::Relaxed))
}

fn writer() -> &'static SyncSender<Line> {
    WRITER.get_or_init(|| {
        let (tx, rx) = sync_channel(QUEUE);
        std::thread::Builder::new()
            .name("log-writer".into())
            .spawn(move || run_writer(rx))
            .expect("cannot start the log writer thread");
        tx
    })
}

fn run_writer(rx: Receiver<Line>) {
    let mut out = std::io::BufWriter::new(std::io::stdout());
    let mut err = std::io::BufWriter::new(std::io::stderr());
    while let Ok(first) = rx.recv() {
        let mut next = Some(first);
        while let Some(line) = next.take() {
            match line {
                Line::Text { text, stderr } => {
                    let w: &mut dyn std::io::Write = if stderr { &mut err } else { &mut out };
                    let _ = w.write_all(text.as_bytes());
                }
                Line::Flush(done) => {
                    let _ = out.flush();
                    let _ = err.flush();
                    let _ = done.send(());
                }
            }
            next = rx.try_recv().ok();
        }
        let _ = out.flush();
        let _ = err.flush();
    }
}

/// Waits (up to 2 s) until every queued record is written (shutdown, end of a command).
pub fn flush() {
    let Some(tx) = WRITER.get() else { return };
    let (done_tx, done_rx) = sync_channel(1);
    if tx.send(Line::Flush(done_tx)).is_ok() {
        let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
    }
}

/// Records dropped because the queue was full.
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// A capture of the log lines for a test, from [`capture_logs`]. Restores the previous level and
/// format when dropped. Records of other tests running at the same time are captured too: filter
/// by component.
pub struct Capture {
    _serial: parking_lot::MutexGuard<'static, ()>,
    level: Level,
    settings: Arc<Settings>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture").field("level", &self.level).finish()
    }
}

/// Starts capturing log lines (JSON, no journald prefix) at `level`. One capture at a time: a
/// second call waits until the first [`Capture`] is dropped.
pub fn capture_logs(level: Level) -> Capture {
    let serial = CAPTURE_SERIAL.lock();
    let previous = SETTINGS.load_full();
    SETTINGS.store(Arc::new(Settings {
        format: Format::Json,
        journald: false,
        base: previous.base.clone(),
        stderr: previous.stderr,
    }));
    let capture = Capture { _serial: serial, level: self::level(), settings: previous };
    CAPTURE.lock().clear();
    CAPTURE_ON.store(true, Ordering::SeqCst);
    set_level(level);
    capture
}

impl Capture {
    /// The lines captured so far (each ends with a line feed).
    pub fn lines(&self) -> Vec<String> {
        CAPTURE.lock().clone()
    }

    /// The records captured so far, parsed.
    pub fn records(&self) -> Vec<Value> {
        self.lines().iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    /// The records of one component.
    pub fn records_of(&self, component: &str) -> Vec<Value> {
        self.records().into_iter().filter(|r| r["c"] == component).collect()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURE_ON.store(false, Ordering::SeqCst);
        CAPTURE.lock().clear();
        set_level(self.level);
        SETTINGS.store(self.settings.clone());
    }
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

    /// The component path (`""` for the root).
    pub fn component(&self) -> &str {
        &self.component
    }

    /// Whether a record of `level` would be written.
    pub fn enabled(&self, level: Level) -> bool {
        level >= self::level()
    }

    /// Whether debug records are written (to skip building costly debug fields).
    pub fn debug_enabled(&self) -> bool {
        self.enabled(Level::Debug)
    }

    /// Writes one record. Prefer the `log_*!` macros, which skip building the fields when the
    /// level is disabled.
    pub fn emit(&self, level: Level, msg: &str, fields: Option<Value>) {
        if !self.enabled(level) {
            return;
        }
        let st = SETTINGS.load();
        let mut line = String::with_capacity(192);
        if st.journald {
            let _ = write!(line, "<{}>", level.syslog_priority());
        }
        let t = iso_time(crate::clock::wall_ms());
        let fields = fields.map(|f| scrub(&f));
        match st.format {
            Format::Json => {
                let mut rec = Map::with_capacity(4 + st.base.len());
                rec.insert("t".into(), Value::String(t));
                rec.insert("level".into(), Value::String(level.as_str().into()));
                rec.insert("c".into(), Value::String(self.component.to_string()));
                rec.insert("msg".into(), Value::String(msg.to_string()));
                for (k, v) in &st.base {
                    if !is_record_key(k) {
                        rec.insert(k.clone(), v.clone());
                    }
                }
                if let Some(Value::Object(f)) = fields {
                    for (k, v) in f {
                        if !is_record_key(&k) {
                            rec.insert(k, v);
                        }
                    }
                }
                json::write(&mut line, &Value::Object(rec));
            }
            Format::Pretty => {
                if !st.journald {
                    line.push_str(&t);
                    line.push(' ');
                }
                let _ = write!(line, "{:<8} [{}] {}", level.as_str().to_uppercase(), self.component, msg);
                if let Some(f) = fields {
                    line.push(' ');
                    json::write(&mut line, &f);
                }
            }
        }
        line.push('\n');
        if CAPTURE_ON.load(Ordering::Relaxed) {
            CAPTURE.lock().push(line);
            return;
        }
        match writer().try_send(Line::Text { text: line, stderr: st.stderr }) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn is_record_key(k: &str) -> bool {
    matches!(k, "t" | "level" | "c" | "msg")
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

/// Formats Unix milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ` (`Date.prototype.toISOString`).
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

// ---- Client addresses --------------------------------------------------------------------------

/// A client address as it may appear in the logs (`LOG_IP`); `None` for an empty address, so the
/// field is left out. Accepts anything printable: an `IpAddr`, a `SocketAddr`'s IP, a string.
pub fn ip(addr: impl std::fmt::Display) -> Option<String> {
    let text = addr.to_string();
    if text.is_empty() {
        return None;
    }
    Some(match IP_MODE.load(Ordering::Relaxed) {
        1 => text,
        2 => hashed_ip(&text, crate::clock::wall_ms()),
        _ => truncate_ip(&text),
    })
}

fn hashed_ip(text: &str, now_ms: i64) -> String {
    let day = now_ms.div_euclid(86_400_000);
    let mut k = IP_KEY.lock();
    if k.day != day {
        let secret: &[u8] = k.secret.as_deref().unwrap_or(b"scacelith");
        k.key = hash::hmac_sha256(secret, format!("log-ip:{day}").as_bytes());
        k.day = day;
    }
    let mac = hash::hmac_sha256(&k.key, text.as_bytes());
    let mut out = String::from("ip:");
    out.push_str(&encoding::base64url_encode(&mac)[..12]);
    out
}

/// IPv4 /24 and IPv6 /48 of an address written as text (an IPv4-mapped IPv6 address counts as
/// IPv4); text that is not an address comes back unchanged.
pub fn truncate_ip(text: &str) -> String {
    let mut a = text;
    if a.len() >= 7 && a.is_char_boundary(7) && a[..7].eq_ignore_ascii_case("::ffff:") && a.contains('.') {
        a = &a[7..];
    }
    if !a.contains(':') && a.contains('.') {
        let parts: Vec<&str> = a.split('.').collect();
        return if parts.len() == 4 {
            format!("{}.{}.{}.0/24", parts[0], parts[1], parts[2])
        } else {
            a.to_string()
        };
    }
    match expand_ipv6(a) {
        Some(groups) => format!("{}::/48", groups[..3].join(":")),
        None => a.to_string(),
    }
}

/// The eight groups of an IPv6 address written as text, each as lowercase hexadecimal without
/// leading zeros (a zone is dropped, a dotted IPv4 tail becomes two groups); `None` when the text
/// does not hold eight groups. Lenient like the Node helper: a group that is not hexadecimal
/// counts as `0`.
pub fn expand_ipv6(text: &str) -> Option<Vec<String>> {
    let s = text.split('%').next().unwrap_or("");
    let halves: Vec<&str> = s.split("::").collect();
    if halves.len() > 2 {
        return None;
    }
    let split = |h: &str| -> Vec<String> {
        if h.is_empty() { Vec::new() } else { h.split(':').map(str::to_string).collect() }
    };
    let mut head = split(halves[0]);
    let mut tail = if halves.len() == 2 { split(halves[1]) } else { Vec::new() };
    let last = if halves.len() == 2 { &mut tail } else { &mut head };
    if last.last().is_some_and(|g| g.contains('.')) {
        let v4: std::net::Ipv4Addr = last.pop().and_then(|g| g.parse().ok())?;
        let b = v4.octets();
        last.push(format!("{:x}", u32::from(b[0]) * 256 + u32::from(b[1])));
        last.push(format!("{:x}", u32::from(b[2]) * 256 + u32::from(b[3])));
    }
    let fill = if halves.len() == 2 { 8usize.saturating_sub(head.len() + tail.len()) } else { 0 };
    let mut all = head;
    all.extend(std::iter::repeat_n("0".to_string(), fill));
    all.extend(tail);
    if all.len() != 8 {
        return None;
    }
    Some(all.iter().map(|g| group_hex(g)).collect())
}

/// `(parseInt(text, 16) || 0).toString(16)` for the values an address group can hold (values
/// beyond 64 bits saturate).
fn group_hex(text: &str) -> String {
    let t = js::trim_start(text);
    let (negative, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    let v = t
        .chars()
        .map_while(|c| c.to_digit(16))
        .fold(0u64, |acc, d| acc.saturating_mul(16).saturating_add(u64::from(d)));
    if negative && v != 0 { format!("-{v:x}") } else { format!("{v:x}") }
}

// ---- Redaction ---------------------------------------------------------------------------------

const SENSITIVE: [&str; 10] = [
    "pass",
    "token",
    "secret",
    "otp",
    "cookie",
    "authorization",
    "recovery",
    "verifier",
    "private",
    "credential",
];

/// Whether a field of this name is replaced by `[redacted]` (the former server's regular
/// expression `pass(word)?|token|secret|^code$|otp|cookie|authorization|recovery|mfa_?secret|
/// totp|verifier|nonce_?secret|private|credential|smtp_?pass`, case-insensitive).
pub fn is_sensitive_key(key: &str) -> bool {
    if key.eq_ignore_ascii_case("code") {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    SENSITIVE.iter().any(|s| lower.contains(s))
}

/// Masks `sct_...`, `swt_...`, `mfa_...` and `sso_...` tokens inside a string and caps its length:
/// the first 4096 UTF-16 units are considered, the result is cut to 2000 units and marked with `…`
/// when longer, or when the input was cut.
pub fn scrub_str(s: &str) -> String {
    let long = s.len() > 4096 && js::utf16_len(s) > 4096;
    let window = if long { js::truncate_utf16(s, 4096) } else { s };
    let mut masked = mask_tokens(window);
    if long || (masked.len() > 2000 && js::utf16_len(&masked) > 2000) {
        let keep = js::truncate_utf16(&masked, 2000).len();
        masked.truncate(keep);
        masked.push('…');
    }
    masked
}

fn mask_tokens(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i + 4 <= b.len() {
        let boundary = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        if boundary && b[i + 3] == b'_' && matches!(&b[i..i + 3], b"sct" | b"swt" | b"mfa" | b"sso") {
            let mut j = i + 4;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'-') {
                j += 1;
            }
            if j - (i + 4) >= 8 {
                out.push_str(&s[copied..i + 4]);
                out.push_str("[redacted]");
                copied = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[copied..]);
    out
}

/// Redacts a field tree: sensitive keys, token-like strings, depth over 4, arrays over 50 items,
/// `null` values left out, errors from [`error`] printed as `{name, message, code, stack}`.
pub fn scrub(v: &Value) -> Value {
    scrub_at(v, 0)
}

fn scrub_at(v: &Value, depth: usize) -> Value {
    match v {
        Value::String(s) => Value::String(scrub_str(s)),
        Value::Array(_) | Value::Object(_) if depth > 4 => Value::String("[depth]".into()),
        Value::Array(a) => Value::Array(a.iter().take(50).map(|x| scrub_at(x, depth + 1)).collect()),
        Value::Object(o) => {
            if let (1, Some(Value::Object(err))) = (o.len(), o.get(MARKER_ERROR)) {
                return scrub_error(err);
            }
            let mut out = Map::with_capacity(o.len());
            for (k, x) in o {
                if is_sensitive_key(k) {
                    out.insert(k.clone(), Value::String("[redacted]".into()));
                } else if !x.is_null() {
                    out.insert(k.clone(), scrub_at(x, depth + 1));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn scrub_error(err: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for key in ["name", "message", "code", "stack"] {
        match (key, err.get(key)) {
            (_, None | Some(Value::Null)) => {}
            ("message", Some(Value::String(s))) => {
                out.insert(key.into(), Value::String(scrub_str(s)));
            }
            ("stack", Some(Value::String(s))) => {
                let first: Vec<&str> = s.split('\n').take(6).collect();
                out.insert(key.into(), Value::String(scrub_str(&first.join("\n"))));
            }
            (_, Some(v)) => {
                out.insert(key.into(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// An error field with explicit parts, printed as `{name, message, code, stack}` (the message and
/// the first 6 lines of the stack are scrubbed; the code is not redacted).
pub fn error_value(name: &str, message: &str, code: Option<&str>, stack: Option<&str>) -> Value {
    let mut inner = Map::new();
    inner.insert("name".into(), Value::String(name.into()));
    inner.insert("message".into(), Value::String(message.into()));
    if let Some(c) = code {
        inner.insert("code".into(), Value::String(c.into()));
    }
    if let Some(s) = stack {
        inner.insert("stack".into(), Value::String(s.into()));
    }
    let mut outer = Map::new();
    outer.insert(MARKER_ERROR.into(), Value::Object(inner));
    Value::Object(outer)
}

/// An error as a log field: `name` (the error type), `message` (its `Display`), `code` (the errno
/// name of an I/O error) and, when it has causes, `stack` (`caused by: ...` lines).
pub fn error<E: std::error::Error + 'static>(e: &E) -> Value {
    let full = std::any::type_name::<E>();
    let path = full.split('<').next().unwrap_or(full);
    let name = path.rsplit("::").next().unwrap_or(path);
    error_with_name(name, e)
}

/// [`error`] for a type-erased error (`Box<dyn Error>`), named `Error`.
pub fn error_dyn(e: &(dyn std::error::Error + 'static)) -> Value {
    error_with_name("Error", e)
}

fn error_with_name(name: &str, e: &(dyn std::error::Error + 'static)) -> Value {
    let message = e.to_string();
    let code =
        e.downcast_ref::<std::io::Error>().and_then(|io| io.raw_os_error()).and_then(errno::errno_name);
    let stack = e.source().map(|_| {
        let mut lines = vec![format!("{name}: {message}")];
        let mut cause = e.source();
        while let Some(c) = cause {
            if lines.len() == 6 {
                break;
            }
            lines.push(format!("caused by: {c}"));
            cause = c.source();
        }
        lines.join("\n")
    });
    error_value(name, &message, code, stack.as_deref())
}

/// `[N bytes]`, how binary data appears in the logs.
pub fn byte_count(len: usize) -> Value {
    Value::String(format!("[{len} bytes]"))
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
        assert_eq!(iso_time(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn sensitive_keys_follow_the_former_regular_expression() {
        for k in [
            "password",
            "Password",
            "newPassword",
            "token",
            "sessionToken",
            "secret",
            "mfa_secret",
            "nonceSecret",
            "code",
            "Code",
            "CODE",
            "otp",
            "totp",
            "cookie",
            "authorization",
            "recoveryCode",
            "verifier",
            "privateKey",
            "credential",
            "smtpPass",
            "authForgotPerHour",
            "privateCodeFailuresPerMin",
            "passwordMinLength",
            "xPASSy",
        ] {
            assert!(is_sensitive_key(k), "{k}");
        }
        for k in ["errorCode", "code ", "userId", "ip", "pa\u{17f}s", "c0de", "msg"] {
            assert!(!is_sensitive_key(k), "{k}");
        }
    }

    #[test]
    fn scrub_masks_keys_and_tokens() {
        let v =
            scrub(&json!({"password": "x", "code": 12, "errorCode": 3, "note": "use sct_ABCDEFGH123 now",
            "nested": {"token": null, "list": ["mfa_12345678", "sso_1234567"]}, "gone": null}));
        assert_eq!(v["password"], "[redacted]");
        assert_eq!(v["code"], "[redacted]");
        assert_eq!(v["errorCode"], 3);
        assert_eq!(v["note"], "use sct_[redacted] now");
        assert_eq!(v["nested"]["token"], "[redacted]");
        assert_eq!(v["nested"]["list"], json!(["mfa_[redacted]", "sso_1234567"]));
        assert!(v.get("gone").is_none(), "null fields are left out like undefined");
        assert_eq!(scrub_str("sct_short"), "sct_short");
        assert_eq!(scrub_str("xsct_ABCDEFGHIJ é-sct_ABCDEFGH-sct_X"), "xsct_ABCDEFGHIJ é-sct_[redacted]");
        assert_eq!(scrub_str("SCT_ABCDEFGHIJ"), "SCT_ABCDEFGHIJ", "the prefixes are case-sensitive");
    }

    #[test]
    fn scrub_depth_and_arrays() {
        let deep = json!({"a": {"b": {"c": {"d": {"e": {"f": 1}}}}}});
        assert_eq!(scrub(&deep), json!({"a": {"b": {"c": {"d": {"e": "[depth]"}}}}}));
        let long: Vec<i32> = (0..80).collect();
        assert_eq!(scrub(&json!({ "l": long }))["l"].as_array().unwrap().len(), 50);
    }

    /// Ported from auth.logs.test.js: long strings are masked before they are cut.
    #[test]
    fn long_strings_are_masked_before_they_are_cut() {
        let tok = format!("sct_{}", "Q".repeat(43));
        let a = scrub_str(&format!("x {tok} {}", "y".repeat(2100)));
        assert!(a.contains("sct_[redacted]") && a.ends_with('…') && js::utf16_len(&a) == 2001);
        let edge = scrub_str(&format!("{}{tok}", "w ".repeat(995)));
        assert!(edge.ends_with("sct_[redac…"), "the token straddles the cut: {}", &edge[1980..]);
        let shrunk = scrub_str(&format!("{tok} ").repeat(100));
        assert!(js::utf16_len(&shrunk) < 2000 && shrunk.ends_with('…'), "a cut string is always marked");
        assert!(!shrunk.contains("sct_QQQQ"));
        let err = scrub(&error_value("Error", &format!("failed for {tok} {}", "z".repeat(2100)), None, None));
        assert!(err["message"].as_str().unwrap().contains("sct_[redacted]"));
        let wide = scrub_str(&"😀".repeat(3000));
        assert_eq!(js::utf16_len(&wide), 2001, "UTF-16 units, as JavaScript counts them");
    }

    #[test]
    fn errors_print_like_node_error_objects() {
        let io = std::fs::read("/nonexistent/scacelith/file").unwrap_err();
        let v = scrub(&json!({ "err": error(&io) }));
        assert_eq!(v["err"]["name"], "Error");
        assert_eq!(v["err"]["code"], "ENOENT", "the code of an error is not redacted");
        assert!(v["err"]["message"].as_str().unwrap().contains("No such file"));
        let custom = scrub(
            &json!({ "err": error_value("SmtpError", "failed for sct_ABCDEFGHIJKL", Some("tls"), None) }),
        );
        assert_eq!(
            custom,
            json!({"err": {"name": "SmtpError", "message": "failed for sct_[redacted]", "code": "tls"}})
        );
        // A plain object with a code is still redacted.
        assert_eq!(scrub(&json!({"err": {"code": "x"}})), json!({"err": {"code": "[redacted]"}}));

        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("journal write failed")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let v = scrub(&error(&Outer(std::io::Error::other("disk full"))));
        assert_eq!(v["name"], "Outer");
        assert_eq!(v["stack"], "Outer: journal write failed\ncaused by: disk full");
        assert_eq!(byte_count(12), json!("[12 bytes]"));
    }

    /// Vectors of the former server's truncateIp (and net.ipguard.test.js).
    #[test]
    fn truncated_addresses() {
        let cases = [
            ("::ffff:1.2.3.4", "1.2.3.0/24"),
            ("::FFFF:192.0.2.7", "192.0.2.0/24"),
            ("192.0.2.7", "192.0.2.0/24"),
            ("2001:db8:1:2::5", "2001:db8:1::/48"),
            ("2001:db8::1.2.3.4", "2001:db8:0::/48"),
            ("fe80::1%eth0", "fe80:0:0::/48"),
            ("2001:db8:1:2:3:4:5.6.7.8", "2001:db8:1::/48"),
            ("2001:db8::1:2:3:4.5.6.7", "2001:db8:0::/48"),
            ("2001:0DB8:0001::", "2001:db8:1::/48"),
            ("bogus", "bogus"),
            ("1.2.3", "1.2.3"),
            ("1::2::3", "1::2::3"),
            ("2001:db8::300.1.2.3", "2001:db8::300.1.2.3"),
            ("-1::", "-1:0:0::/48"),
            (" 0x1F::", "1f:0:0::/48"),
        ];
        for (input, want) in cases {
            assert_eq!(truncate_ip(input), want, "{input}");
        }
    }

    #[test]
    fn hashed_addresses_use_the_daily_key() {
        // Node: HMAC(HMAC(secret, 'log-ip:' + day), ip), base64url, 12 characters.
        {
            let mut k = IP_KEY.lock();
            k.secret = Some(vec![7u8; 48]);
            k.day = i64::MIN;
        }
        assert_eq!(hashed_ip("203.0.113.9", 20_000 * 86_400_000 + 5), "ip:4rpXxA3xewGW");
        {
            let mut k = IP_KEY.lock();
            k.secret = None;
            k.day = i64::MIN;
        }
        assert_eq!(hashed_ip("2001:db8::1", 20_000 * 86_400_000), "ip:fseWCQ4_KAgP");
    }

    #[test]
    fn records_keep_their_keys_first_and_fields_never_replace_them() {
        let cap = capture_logs(Level::Debug);
        let log = Logger::root().child("logtest");
        log_warn!(log, "x", { "level": 3, "msg": "y", "c": "other", "t": 0, "n": 1, "ip": ip("") });
        log_debug!(log.child("sub"), "plain");
        let recs = cap.records_of("logtest");
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(
            (r["level"].as_str(), r["msg"].as_str(), r["c"].as_str(), r["n"].as_i64()),
            (Some("warn"), Some("x"), Some("logtest"), Some(1))
        );
        assert!(r["t"].is_string() && r.get("ip").is_none());
        let keys: Vec<&String> = r.as_object().unwrap().keys().take(4).collect();
        assert_eq!(keys, ["t", "level", "c", "msg"]);
        assert_eq!(cap.records_of("logtest.sub").len(), 1);
        drop(cap);
    }
}
