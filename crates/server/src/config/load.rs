//! Loading the configuration: the sources (environment over `.env`, `<KEY>_FILE` secrets), the
//! parsing of every key, the derived values and the checks between keys. Every error is
//! collected before the load fails, with the former server's messages.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use super::keys::{KEYS, KeySpec, Kind, OBSOLETE};
use super::{Category, Config, ConfigError, Derived, sso_origin_tag};
use crate::util::ip::IpMatcher;
use crate::util::{encoding, errno, js, path};

/// Largest integer a setting may hold (JavaScript's `Number.MAX_SAFE_INTEGER`).
const MAX_SAFE: i64 = 9_007_199_254_740_991;

/// Where the configuration comes from.
#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// Environment variables; they win over the file, even when empty.
    pub env: HashMap<String, String>,
    /// The `.env` file: `None` = the file named by `SCACELITH_ENV_FILE`, else `<cwd>/.env` when it
    /// exists; `Some("")` = no file; `Some(path)` = this file, which must be readable.
    pub env_file: Option<String>,
    /// Directory that relative paths are resolved against.
    pub cwd: PathBuf,
}

impl LoadOptions {
    /// The process environment and working directory.
    pub fn from_process() -> LoadOptions {
        let env = std::env::vars_os()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .collect();
        LoadOptions { env, env_file: None, cwd: current_dir() }
    }

    /// These variables only, no file, the process working directory (tests).
    pub fn from_pairs(pairs: &[(&str, &str)]) -> LoadOptions {
        let env = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        LoadOptions { env, env_file: Some(String::new()), cwd: current_dir() }
    }
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// A parsed value. A key whose text was refused has no value at all (JavaScript's `undefined`),
/// which the checks between keys skip, as the former loader's comparisons did.
#[derive(Clone, Debug)]
pub(super) enum Val {
    Text(String),
    Int(i64),
    Num(f64),
    Bool(bool),
    List(Vec<String>),
    Bytes(Vec<u8>),
    Null,
}

/// The values of the keys during the load.
#[derive(Debug, Default)]
pub(super) struct Values(HashMap<&'static str, Val>);

impl Values {
    fn set(&mut self, name: &'static str, v: Val) {
        self.0.insert(name, v);
    }

    fn int(&self, name: &str) -> Option<i64> {
        match self.0.get(name) {
            Some(Val::Int(n)) => Some(*n),
            _ => None,
        }
    }

    fn text(&self, name: &str) -> Option<&str> {
        match self.0.get(name) {
            Some(Val::Text(s)) => Some(s),
            _ => None,
        }
    }

    fn bool(&self, name: &str) -> Option<bool> {
        match self.0.get(name) {
            Some(Val::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    fn list(&self, name: &str) -> &[String] {
        match self.0.get(name) {
            Some(Val::List(l)) => l,
            _ => &[],
        }
    }

    fn is_null(&self, name: &str) -> bool {
        matches!(self.0.get(name), Some(Val::Null))
    }

    /// Whether the key holds a non-empty text, a secret or `true` (JavaScript truthiness of the
    /// values the checks look at).
    fn truthy(&self, name: &str) -> bool {
        match self.0.get(name) {
            Some(Val::Text(s)) => !s.is_empty(),
            Some(Val::Bytes(_)) => true,
            Some(Val::Bool(b)) => *b,
            Some(Val::Int(n)) => *n != 0,
            _ => false,
        }
    }

    /// Removes a value to build the configuration. Every key has a value once the load found no
    /// error.
    pub(super) fn take(&mut self, name: &str) -> Val {
        self.0
            .remove(name)
            .unwrap_or_else(|| panic!("configuration key {name} has no value after a clean load"))
    }
}

/// Loads and checks the configuration.
pub fn load(opts: &LoadOptions) -> Result<Config, ConfigError> {
    let mut errors = Vec::new();
    let mut notes = Vec::new();
    let file_vars = read_env_file(opts, &mut errors, &mut notes);
    let get = |name: &str| -> Option<&str> {
        opts.env.get(name).or_else(|| file_vars.get(name)).map(String::as_str)
    };

    // The key is read from TLS_KEY_FILE already; its text must never become a path (or a log line).
    if get("TLS_KEY_FILE_FILE").is_some_and(|v| !v.is_empty()) {
        errors
            .push("TLS_KEY_FILE already names the key file; TLS_KEY_FILE_FILE is not supported.".to_string());
    }
    let mut values = Values::default();
    for spec in KEYS {
        if let Some(v) = parse_key(spec, &get, &opts.cwd, &mut errors, &mut notes) {
            values.set(spec.name, v);
        }
    }
    let derived = derive(&mut values, &get, &opts.cwd, &mut errors, &mut notes);
    if errors.is_empty() { Ok(Config::from_values(values, derived)) } else { Err(ConfigError { errors }) }
}

/// The variables of the `.env` file, if any.
fn read_env_file(
    opts: &LoadOptions,
    errors: &mut Vec<String>,
    notes: &mut Vec<String>,
) -> IndexMap<String, String> {
    let named = opts.env_file.clone().or_else(|| opts.env.get("SCACELITH_ENV_FILE").cloned());
    match named {
        // A file named explicitly must be there: a typo would start the server on the defaults.
        Some(name) if !name.is_empty() => match fs::read(&name) {
            Ok(bytes) => super::dotenv::parse_env_file(&String::from_utf8_lossy(&bytes), notes),
            Err(e) => {
                errors
                    .push(format!("SCACELITH_ENV_FILE: cannot read {name} ({}).", errno::io_error_code(&e)));
                IndexMap::new()
            }
        },
        Some(_) => IndexMap::new(),
        None => {
            let file = opts.cwd.join(".env");
            if !file.exists() {
                return IndexMap::new();
            }
            match fs::read(&file) {
                Ok(bytes) => super::dotenv::parse_env_file(&String::from_utf8_lossy(&bytes), notes),
                Err(e) => {
                    errors.push(format!("cannot read {} ({}).", file.display(), errno::io_error_code(&e)));
                    IndexMap::new()
                }
            }
        }
    }
}

/// Parses one key; `None` when its text is refused (the error is recorded).
fn parse_key<'a>(
    k: &KeySpec,
    get: &impl Fn(&str) -> Option<&'a str>,
    cwd: &Path,
    errors: &mut Vec<String>,
    notes: &mut Vec<String>,
) -> Option<Val> {
    let name = k.name;
    let mut raw = get(name).map(str::to_string);
    let secret = k.kind.is_secret();
    let file_ref = get(&format!("{name}_FILE")).filter(|f| !f.is_empty());
    if let (true, Some(_)) = (secret, file_ref) {
        // An empty KEY= (the .env.example line of a required secret, an unset docker-compose
        // variable) does not hide KEY_FILE for a required secret; an optional one stays unset.
        if raw.as_deref() == Some("") && !k.required {
            notes.push(format!(
                "{name} is set but empty, so {name}_FILE is not read and {name} is unset. Remove the empty {name}= \
                 (or {name}_FILE) to say which one you mean."
            ));
        }
    }
    if let (true, Some(file)) = (secret, file_ref)
        && (raw.is_none() || (raw.as_deref() == Some("") && k.required))
    {
        match fs::read(path::resolve(cwd, file)) {
            Ok(bytes) => raw = Some(js::trim(&String::from_utf8_lossy(&bytes)).to_string()),
            Err(e) => {
                errors.push(format!("{name}_FILE: cannot read {file} ({})", errno::io_error_code(&e)));
                return None;
            }
        }
    }
    let text = match raw {
        Some(r) if !r.is_empty() => r,
        _ => {
            if k.required {
                let what = k.desc.split('.').next().unwrap_or(k.desc);
                errors.push(format!("{name} is required ({what})."));
                return None;
            }
            let default = if k.kind == Kind::List { Some(k.default.unwrap_or("")) } else { k.default };
            match default {
                Some(d) if !secret => d.to_string(),
                _ => return Some(Val::Null),
            }
        }
    };
    match k.kind {
        Kind::Text | Kind::Path => {
            if let Some(max) = k.max_chars.filter(|m| js::utf16_len(&text) > *m) {
                errors.push(format!("{name}: at most {max} characters."));
            } else if let Some(max) = k.max_bytes.filter(|m| text.len() > *m) {
                errors.push(format!(
                    "{name}: at most {max} bytes in UTF-8 (fewer characters with accents or other scripts)."
                ));
            }
            if k.max_bytes.is_some() && text.contains('\0') {
                errors.push(format!("{name}: no NUL character."));
            }
            // SQLite's in-memory database is a name, not a file.
            let keep = text.is_empty() || (name == "DB_PATH" && text == ":memory:");
            if k.kind == Kind::Path && !keep {
                return Some(Val::Text(path::to_text(&path::resolve(cwd, &text))));
            }
            Some(Val::Text(text))
        }
        Kind::Int | Kind::Port => {
            let Some(v) = parse_int(js::trim(&text)) else {
                errors.push(format!("{name}: integer expected, got \"{text}\"."));
                return None;
            };
            let (min, max) = if k.kind == Kind::Port {
                (0, 65_535)
            } else {
                (k.min.unwrap_or(-MAX_SAFE), k.max.unwrap_or(MAX_SAFE))
            };
            if v < min {
                errors.push(format!("{name}: at least {min}."));
            }
            if v > max {
                errors.push(format!("{name}: at most {max}."));
            }
            Some(Val::Int(v))
        }
        Kind::Number => {
            let t = js::trim(&text);
            let Some(v) = is_number_text(t).then(|| t.parse::<f64>().ok()).flatten() else {
                errors.push(format!("{name}: number expected, got \"{text}\"."));
                return None;
            };
            if let Some(min) = k.min.filter(|m| v < *m as f64) {
                errors.push(format!("{name}: at least {min}."));
            }
            if let Some(max) = k.max.filter(|m| v > *m as f64) {
                errors.push(format!("{name}: at most {max}."));
            }
            Some(Val::Num(v))
        }
        Kind::Bool => match js::trim(&text).to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(Val::Bool(true)),
            "0" | "false" | "no" | "off" => Some(Val::Bool(false)),
            _ => {
                errors.push(format!("{name}: true or false expected."));
                None
            }
        },
        Kind::Enum(allowed) => {
            let v = js::trim(&text);
            if !allowed.contains(&v) {
                errors.push(format!("{name}: one of {}.", allowed.join(", ")));
            }
            Some(Val::Text(v.to_string()))
        }
        Kind::List => Some(Val::List(split_list(&text))),
        Kind::SecretText => Some(Val::Text(text)),
        Kind::Secret => {
            let bytes = decode_secret(&text);
            if let Some(min) = k.min_bytes.filter(|m| bytes.len() < *m) {
                errors.push(format!("{name}: at least {min} bytes of entropy (hex or base64)."));
            }
            Some(Val::Bytes(bytes))
        }
    }
}

/// `^-?\d+$`, read with saturation (the bounds then refuse what does not fit).
fn parse_int(t: &str) -> Option<i64> {
    let (negative, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v = digits.bytes().fold(0i64, |acc, b| acc.saturating_mul(10).saturating_add(i64::from(b - b'0')));
    Some(if negative { -v } else { v })
}

/// `^-?(\d+\.?\d*|\.\d+)$`.
fn is_number_text(t: &str) -> bool {
    let t = t.strip_prefix('-').unwrap_or(t);
    let (int, frac) = match t.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (t, None),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    match frac {
        None => !int.is_empty() && digits(int),
        Some(f) => digits(int) && digits(f) && (!int.is_empty() || !f.is_empty()),
    }
}

/// Comma-separated values, each trimmed, empty ones dropped.
fn split_list(text: &str) -> Vec<String> {
    text.split(',').map(js::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// A binary secret: hexadecimal when the whole trimmed text is hexadecimal of even length, else
/// base64 decoded leniently (both alphabets, other characters skipped, stop at `=`), as the former
/// server decoded it: every key derived from the secret depends on these exact bytes.
pub fn decode_secret(text: &str) -> Vec<u8> {
    let s = js::trim(text);
    if !s.is_empty() && s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return hex::decode(s).expect("checked hexadecimal text of even length");
    }
    encoding::base64_decode_lenient(s)
}

/// Default `MAX_PENDING_HANDSHAKES_PER_IP`: the total / 32 with a floor of 2, but below the total
/// when the total is at least 2 (4 for 128, 1 for 2).
pub fn default_pending_per_group(max_pending: i64) -> i64 {
    1.max((max_pending - 1).min(2.max(max_pending / 32)))
}

/// Derived values and the checks between keys, in the former loader's order.
fn derive<'a>(
    v: &mut Values,
    get: &impl Fn(&str) -> Option<&'a str>,
    cwd: &Path,
    errors: &mut Vec<String>,
    notes: &mut Vec<String>,
) -> Derived {
    let data_dir = match v.text("DATA_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => path::resolve(cwd, "data"),
    };
    for (key, file) in [("DB_PATH", "scacelith.db"), ("JOURNAL_DIR", "journal")] {
        if v.text(key).is_none_or(str::is_empty) {
            v.set(key, Val::Text(path::to_text(&path::join(&data_dir, file))));
        }
    }
    v.set("DATA_DIR", Val::Text(path::to_text(&data_dir)));
    let run_dir = path::join(&data_dir, "run");

    let api_port = v.int("API_PORT");
    if let (None, Some(p)) = (v.int("WS_PORT"), api_port) {
        v.set("WS_PORT", Val::Int(p));
    }
    for (public, own) in [("PUBLIC_API_PORT", "API_PORT"), ("PUBLIC_WS_PORT", "WS_PORT")] {
        if let (false, Some(p)) = (v.truthy(public), v.int(own)) {
            v.set(public, Val::Int(p));
        }
    }
    if !v.truthy("INSTANCE_ID") {
        v.set("INSTANCE_ID", Val::Text(crate::sys::hostname()));
    }

    // Per-address limits: the /48 budgets default to several /64s' worth.
    if let (false, Some(ip)) = (v.truthy("AUTH_RATE_PER_PREFIX"), v.int("AUTH_RATE_PER_IP")) {
        v.set("AUTH_RATE_PER_PREFIX", Val::Int(5 * ip));
    }
    if let Some(ip) = v.int("HTTP_RATE_PER_IP") {
        match v.int("HTTP_RATE_PER_PREFIX") {
            Some(p) if p != 0 && p < ip => errors.push(
                "HTTP_RATE_PER_PREFIX must be 0 or at least HTTP_RATE_PER_IP (a /48 holds many /64 networks)."
                    .to_string(),
            ),
            Some(p) if p != 0 => {}
            _ => v.set("HTTP_RATE_PER_PREFIX", Val::Int(4 * ip)),
        }
    }
    if let (Some(base), Some(max)) = (v.int("ABUSE_BLOCK_BASE_SEC"), v.int("ABUSE_BLOCK_MAX_SEC"))
        && base > max
    {
        errors.push("ABUSE_BLOCK_BASE_SEC must not exceed ABUSE_BLOCK_MAX_SEC.".to_string());
    }
    if let Err(e) = IpMatcher::parse(v.list("ABUSE_EXEMPT")) {
        errors.push(format!("ABUSE_EXEMPT: {e}."));
    }
    if v.text("TLS_MODE") == Some("proxy")
        && let Err(e) = IpMatcher::parse(v.list("TRUSTED_PROXIES"))
    {
        errors.push(format!("TRUSTED_PROXIES: {e}."));
    }

    // WORKERS: game shards and runtime threads.
    let workers = v.text("WORKERS").and_then(|w| {
        let w = js::trim(w).to_ascii_lowercase();
        if w == "auto" {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            return Some(cores.clamp(1, 16) as i64);
        }
        parse_int(&w).filter(|n| !w.starts_with('-') && (1..=64).contains(n))
    });
    match workers {
        Some(n) => v.set("WORKERS", Val::Int(n)),
        None => errors.push("WORKERS: \"auto\" or a number from 1 to 64.".to_string()),
    }
    if let (Some(base), Some(w)) = (v.int("SHARD_BASE"), workers)
        && base + w > 64
    {
        errors.push("SHARD_BASE + WORKERS must not exceed 64 (game ids hold 6 bits of shard).".to_string());
    }
    // Whole-server capacities that the former server applied per worker process: the defaults
    // keep the capacity of WORKERS such processes.
    if let Some(w) = workers {
        for (key, value) in [
            ("MAX_PENDING_HANDSHAKES", 128 * w),
            ("IP_MAX_INFLIGHT", 32 * w),
            ("PASSWORD_HASH_CONCURRENCY", w),
            ("PASSWORD_HASH_QUEUE_MAX", 32 * w),
            ("PASSWORD_HASH_WAITERS_PER_SOURCE", 2 * w),
            ("GIF_THREADS", w),
            ("GIF_QUEUE_MAX", 4 * w),
            ("GIF_CACHE_MB", 32 * w),
            ("DB_CACHE_MB", 64 * (w + 1)),
        ] {
            if v.is_null(key) {
                v.set(key, Val::Int(value));
            }
        }
    }

    if v.text("TLS_MODE") == Some("native") && !(v.truthy("TLS_CERT_FILE") && v.truthy("TLS_KEY_FILE")) {
        errors.push("TLS_MODE=native needs TLS_CERT_FILE and TLS_KEY_FILE.".to_string());
    }
    if v.text("TLS_MODE") == Some("off") && v.bool("ALLOW_INSECURE_DEV") != Some(true) {
        errors.push(
            "TLS_MODE=off is refused unless ALLOW_INSECURE_DEV=1 (never on a public server).".to_string(),
        );
    }
    if let (Some(min), Some(max)) = (v.int("USERNAME_MIN"), v.int("USERNAME_MAX"))
        && min > max
    {
        errors.push("USERNAME_MIN must not exceed USERNAME_MAX.".to_string());
    }
    if v.bool("SSO_GOOGLE_ENABLED") == Some(true)
        && !(v.truthy("GOOGLE_CLIENT_ID") && v.truthy("GOOGLE_CLIENT_SECRET"))
    {
        errors.push("SSO_GOOGLE_ENABLED needs GOOGLE_CLIENT_ID and GOOGLE_CLIENT_SECRET.".to_string());
    }
    // Keys that no longer exist: without a note they would be ignored silently.
    for (key, note) in OBSOLETE {
        if get(key).is_some_and(|x| !x.is_empty()) {
            notes.push(note.to_string());
        }
    }
    if v.text("MAIL_TRANSPORT") == Some("smtp") && !v.truthy("SMTP_HOST") {
        errors.push("MAIL_TRANSPORT=smtp needs SMTP_HOST.".to_string());
    }
    // (lower key, upper key, whether they may be equal, message when the order is wrong)
    let ordered = [
        (
            "ANALYSIS_DEPTH_FAST",
            "ANALYSIS_DEPTH_DEEP",
            false,
            "ANALYSIS_DEPTH_FAST must be lower than ANALYSIS_DEPTH_DEEP.",
        ),
        (
            "AUTH_FORGOT_PER_HOUR",
            "AUTH_FORGOT_PER_DAY",
            true,
            "AUTH_FORGOT_PER_DAY must be at least AUTH_FORGOT_PER_HOUR.",
        ),
        (
            "GIF_USER_RENDERS_PER_MIN",
            "GIF_USER_RENDERS_PER_HOUR",
            true,
            "GIF_USER_RENDERS_PER_HOUR must be at least GIF_USER_RENDERS_PER_MIN.",
        ),
        (
            "GIF_IP_RENDERS_PER_MIN",
            "GIF_IP_RENDERS_PER_HOUR",
            true,
            "GIF_IP_RENDERS_PER_HOUR must be at least GIF_IP_RENDERS_PER_MIN.",
        ),
    ];
    for (lower, upper, may_equal, message) in ordered {
        if let (Some(low), Some(high)) = (v.int(lower), v.int(upper))
            && (low > high || (low == high && !may_equal))
        {
            errors.push(message.to_string());
        }
    }
    let max_pending = v.int("MAX_PENDING_HANDSHAKES");
    match (v.int("MAX_PENDING_HANDSHAKES_PER_IP"), max_pending) {
        (Some(per_group), Some(total)) if per_group >= total => errors.push(
            "MAX_PENDING_HANDSHAKES_PER_IP must be lower than MAX_PENDING_HANDSHAKES (one address group could \
             otherwise hold every handshake slot)."
                .to_string(),
        ),
        // Empty: the effective default, so that check-config shows it and the TLS gate uses it.
        (None, Some(total)) => v.set("MAX_PENDING_HANDSHAKES_PER_IP", Val::Int(default_pending_per_group(total))),
        _ => {}
    }
    // The hold must end before the grace. Only a value the operator set is refused; the default
    // follows a short RECOVERY_GRACE_MS down.
    if let (Some(hold), Some(grace)) = (v.int("RECOVERY_CLOCK_HOLD_MS"), v.int("RECOVERY_GRACE_MS")) {
        if get("RECOVERY_CLOCK_HOLD_MS").is_some_and(|x| !x.is_empty()) {
            if hold >= grace {
                errors.push("RECOVERY_CLOCK_HOLD_MS must be lower than RECOVERY_GRACE_MS.".to_string());
            }
        } else {
            v.set("RECOVERY_CLOCK_HOLD_MS", Val::Int(hold.min(grace - 1)));
        }
    }

    // The origin players reach the API at, and its tag in the Google sign-in redirect.
    let mut sso_host = v.text("SERVER_PUBLIC_HOST").unwrap_or("").to_lowercase();
    if sso_host.contains(':') && !sso_host.starts_with('[') {
        sso_host = format!("[{sso_host}]");
    }
    let sso_origin = match v.int("PUBLIC_API_PORT") {
        Some(port) => format!("{sso_host}:{port}"),
        None => format!("{sso_host}:undefined"),
    };
    let sso_redirect_tag = sso_origin_tag(&sso_origin);
    let mut categories = Vec::new();
    for c in v.list("RATED_CATEGORIES") {
        match parse_category(c) {
            Some(cat) => categories.push(cat),
            None => errors.push(format!("RATED_CATEGORIES: \"{c}\" is not minutes+seconds (e.g. 3+2).")),
        }
    }
    Derived { run_dir, sso_origin, sso_redirect_tag, categories, load_notes: std::mem::take(notes) }
}

/// `m+s` with 1 to 3 digits each, minutes 1 to 180, increment 0 to 180 seconds.
fn parse_category(text: &str) -> Option<Category> {
    let (m, s) = text.split_once('+')?;
    let field = |f: &str| -> Option<i64> {
        if (1..=3).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_digit()) {
            f.parse().ok()
        } else {
            None
        }
    };
    let (minutes, seconds) = (field(m)?, field(s)?);
    if !(1..=180).contains(&minutes) || seconds > 180 {
        return None;
    }
    Some(Category { id: text.to_string(), base_ms: minutes * 60_000, inc_ms: seconds * 1000 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_and_numbers_follow_the_former_patterns() {
        assert_eq!(parse_int("42"), Some(42));
        assert_eq!(parse_int("-7"), Some(-7));
        assert_eq!(parse_int("007"), Some(7));
        assert_eq!(parse_int("99999999999999999999999"), Some(i64::MAX));
        for bad in ["", "-", "+1", "1.0", "1e3", "0x10", "1 2", "١٢"] {
            assert_eq!(parse_int(bad), None, "{bad}");
        }
        for good in ["1", "0.5", "1.", ".5", "-.5", "-0", "10.25"] {
            assert!(is_number_text(good), "{good}");
        }
        for bad in ["", ".", "-", "1e3", "1.2.3", "+1", " 1", "NaN", "Infinity"] {
            assert!(!is_number_text(bad), "{bad}");
        }
    }

    #[test]
    fn secrets_decode_like_the_former_server() {
        assert_eq!(decode_secret(" 0aFf "), vec![0x0a, 0xff]);
        assert_eq!(decode_secret("abc"), encoding::base64_decode_lenient("abc"), "odd length is base64");
        assert_eq!(hex::encode(decode_secret("\"QUJD\"")), "414243");
        assert_eq!(hex::encode(decode_secret("QU=JD")), "41");
        assert_eq!(hex::encode(decode_secret("QUJD!!RUY")), "4142434546");
        assert_eq!(hex::encode(decode_secret("-_-_")), "fbffbf");
    }

    #[test]
    fn pending_handshakes_per_group_default() {
        assert_eq!(default_pending_per_group(128), 4);
        assert_eq!(default_pending_per_group(2), 1);
        assert_eq!(default_pending_per_group(3), 2);
        assert_eq!(default_pending_per_group(512), 16);
        assert_eq!(default_pending_per_group(100_000), 3125);
    }

    #[test]
    fn categories() {
        let c = parse_category("3+2").unwrap();
        assert_eq!((c.base_ms, c.inc_ms), (180_000, 2000));
        assert!(parse_category("180+180").is_some());
        for bad in ["0+5", "181+0", "3+181", "3+", "+2", "3-2", "1000+1", "3+2+1", " 3+2"] {
            assert!(parse_category(bad).is_none(), "{bad}");
        }
    }
}
