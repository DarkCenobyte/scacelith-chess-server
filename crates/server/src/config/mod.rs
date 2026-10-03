//! Server configuration: every setting an administrator can change, read from the environment
//! and from an optional `.env` file (`KEY=value` lines), with `<KEY>_FILE` indirection for secrets
//! (systemd `LoadCredential=`). Modules receive the loaded [`Config`]; they never read the
//! environment.
//!
//! The key table ([`KEYS`], `keys.rs`) is the single source of truth: the loader, `check-config`
//! and the generated `.env.example` and `docs/CONFIG.md` (`scacelith-server gen-config-docs`)
//! come from it. Key names, types, defaults, bounds, validation messages and warnings are those of
//! the former Node.js server, so that an existing `.env` keeps working, with these changes
//! (docs/RUST-PORT.md section 9):
//!
//! * `SHARD_OVERLOAD_LAG_MS`, `LISTEN_REUSE_PORT` (and `UV_THREADPOOL_SIZE`, `GOOGLE_REDIRECT_URI`)
//!   no longer exist: still set, they get a warning, never an error ([`OBSOLETE`]);
//! * `WORKERS` is the number of game shards and of runtime threads (`auto` = one per core, at
//!   most 16), still within `SHARD_BASE + WORKERS <= 64`;
//! * the settings the former server applied per worker process are whole-server values whose
//!   defaults scale with `WORKERS`: `MAX_PENDING_HANDSHAKES` (128 x), `IP_MAX_INFLIGHT` (32 x),
//!   `PASSWORD_HASH_CONCURRENCY` (1 x), `PASSWORD_HASH_QUEUE_MAX` (32 x),
//!   `PASSWORD_HASH_WAITERS_PER_SOURCE` (2 x), `GIF_THREADS` (1 x), `GIF_QUEUE_MAX` (4 x),
//!   `GIF_CACHE_MB` (32 x) and `DB_CACHE_MB` (64 x (WORKERS + 1), shared by the connections);
//! * integers beyond 2^53 - 1 are refused (the former server rounded them), and `DB_PATH=:memory:`
//!   stays a name instead of becoming a file of the working directory.
//!
//! Field names are the lower-case key names (`API_PORT` -> `api_port`) and hold the effective
//! values: derived ones (ports, worker count, per-address budgets, scaled capacities, the SSO
//! origin, the categories) are computed once at load time, so `check-config` prints what the
//! server uses.

mod docs;
mod dotenv;
mod keys;
mod load;

pub use docs::{
    GENERATED_FILES, gen_config_docs, render_config_doc, render_env_example, stale_files, type_text,
};
pub use dotenv::parse_env_file;
pub use keys::{KEYS, KeySpec, Kind, OBSOLETE, Section, spec};
pub use load::{LoadOptions, decode_secret, default_pending_per_group, load};

use std::fmt;
use std::path::PathBuf;

use serde_json::{Map, Value};

use load::Val;

/// A binary secret (`secret` keys: hexadecimal, or the former server's lenient base64). Never
/// printed: `Debug` shows `Secret(<redacted>)` and `check-config` `<set>`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    /// Wraps decoded bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Secret(bytes)
    }

    /// The decoded bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Whether the secret holds no byte.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A text secret (`SMTP_PASSWORD`, `GOOGLE_CLIENT_SECRET`, `METRICS_TOKEN`), used as written.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretText(String);

impl SecretText {
    /// Wraps the text.
    pub fn new(text: String) -> Self {
        SecretText(text)
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretText(<redacted>)")
    }
}

/// A configuration value read from one of a fixed set of texts.
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// The configuration text of this value.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)+
                }
            }

            /// Parses a configuration text (case-sensitive).
            pub fn parse(text: &str) -> Option<$name> {
                match text {
                    $($text => Some($name::$variant),)+
                    _ => None,
                }
            }
        }

        impl FieldValue for $name {
            fn from_val(v: Val, key: &str) -> Self {
                match v {
                    Val::Text(s) => $name::parse(&s).unwrap_or_else(|| mismatch(key)),
                    _ => mismatch(key),
                }
            }

            fn describe(&self) -> Value {
                Value::String(self.as_str().to_string())
            }
        }
    };
}

text_enum! {
    /// `TLS_MODE`.
    TlsMode {
        /// This server terminates TLS.
        Native = "native",
        /// A reverse proxy terminates TLS.
        Proxy = "proxy",
        /// Plain text (development only).
        Off = "off",
    }
}

text_enum! {
    /// `TLS_MIN_VERSION`.
    TlsMinVersion {
        /// TLS 1.2 and 1.3.
        Tls12 = "TLSv1.2",
        /// TLS 1.3 only.
        Tls13 = "TLSv1.3",
    }
}

text_enum! {
    /// `REGISTRATION`.
    Registration {
        /// New accounts can be created from the game.
        Open = "open",
        /// No new account.
        Closed = "closed",
    }
}

text_enum! {
    /// `MAIL_TRANSPORT`.
    MailTransport {
        /// Send with the `SMTP_*` settings.
        Smtp = "smtp",
        /// Write the messages to the log.
        Log = "log",
        /// No mail.
        None = "none",
    }
}

text_enum! {
    /// `SMTP_SECURITY`.
    SmtpSecurity {
        /// STARTTLS, required.
        Starttls = "starttls",
        /// Implicit TLS (port 465).
        Tls = "tls",
        /// Plain text (local relay only).
        None = "none",
    }
}

text_enum! {
    /// `LOG_LEVEL`.
    LogLevel {
        /// Everything, the access log included.
        Debug = "debug",
        /// Normal operation.
        Info = "info",
        /// Warnings, security events and errors.
        Warn = "warn",
        /// Errors only.
        Error = "error",
    }
}

text_enum! {
    /// `LOG_FORMAT`.
    LogFormat {
        /// One JSON object per line.
        Json = "json",
        /// Readable text.
        Pretty = "pretty",
    }
}

text_enum! {
    /// `LOG_IP`.
    LogIp {
        /// IPv4 /24, IPv6 /48.
        Truncated = "truncated",
        /// The whole address.
        Full = "full",
        /// A keyed hash, rotated daily.
        Hashed = "hashed",
    }
}

/// An official time-control category (`RATED_CATEGORIES`), e.g. `3+2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Category {
    /// The category as written (`3+2`).
    pub id: String,
    /// Base time in milliseconds.
    pub base_ms: i64,
    /// Increment per move in milliseconds.
    pub inc_ms: i64,
}

/// An invalid configuration: every problem found, in the order the former server listed them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    /// One sentence per problem.
    pub errors: Vec<String>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid configuration:\n  - {}", self.errors.join("\n  - "))
    }
}

impl std::error::Error for ConfigError {}

/// Conversion of a checked value into a field, and back to `check-config` JSON.
trait FieldValue: Sized {
    fn from_val(v: Val, key: &str) -> Self;
    fn describe(&self) -> Value;
}

fn mismatch(key: &str) -> ! {
    panic!("configuration key {key}: the loader stored a value of another type")
}

impl FieldValue for String {
    fn from_val(v: Val, key: &str) -> Self {
        match v {
            Val::Text(s) => s,
            _ => mismatch(key),
        }
    }

    fn describe(&self) -> Value {
        Value::String(self.clone())
    }
}

impl FieldValue for i64 {
    fn from_val(v: Val, key: &str) -> Self {
        match v {
            Val::Int(n) => n,
            _ => mismatch(key),
        }
    }

    fn describe(&self) -> Value {
        Value::from(*self)
    }
}

impl FieldValue for u16 {
    fn from_val(v: Val, key: &str) -> Self {
        match v {
            Val::Int(n) => u16::try_from(n).unwrap_or_else(|_| mismatch(key)),
            _ => mismatch(key),
        }
    }

    fn describe(&self) -> Value {
        Value::from(*self)
    }
}

impl FieldValue for f64 {
    fn from_val(v: Val, key: &str) -> Self {
        match v {
            Val::Num(n) => n,
            _ => mismatch(key),
        }
    }

    fn describe(&self) -> Value {
        Value::from(*self)
    }
}

impl FieldValue for bool {
    fn from_val(v: Val, key: &str) -> Self {
        match v {
            Val::Bool(b) => b,
            _ => mismatch(key),
        }
    }

    fn describe(&self) -> Value {
        Value::Bool(*self)
    }
}

impl FieldValue for Vec<String> {
    fn from_val(v: Val, key: &str) -> Self {
        match v {
            Val::List(l) => l,
            _ => mismatch(key),
        }
    }

    fn describe(&self) -> Value {
        Value::Array(self.iter().cloned().map(Value::String).collect())
    }
}

impl FieldValue for Secret {
    fn from_val(v: Val, key: &str) -> Self {
        match v {
            Val::Bytes(b) => Secret(b),
            _ => mismatch(key),
        }
    }

    fn describe(&self) -> Value {
        Value::String("<set>".into())
    }
}

impl FieldValue for Option<Secret> {
    fn from_val(v: Val, key: &str) -> Self {
        match v {
            Val::Null => None,
            other => Some(Secret::from_val(other, key)),
        }
    }

    fn describe(&self) -> Value {
        Value::String(if self.is_some() { "<set>" } else { "<unset>" }.into())
    }
}

impl FieldValue for Option<SecretText> {
    fn from_val(v: Val, key: &str) -> Self {
        match v {
            Val::Null => None,
            Val::Text(s) => Some(SecretText(s)),
            _ => mismatch(key),
        }
    }

    fn describe(&self) -> Value {
        Value::String(if self.is_some() { "<set>" } else { "<unset>" }.into())
    }
}

/// Values computed at load time that are not keys.
struct Derived {
    run_dir: PathBuf,
    sso_origin: String,
    sso_redirect_tag: String,
    categories: Vec<Category>,
    load_notes: Vec<String>,
}

/// Declares [`Config`] with one field per key, in the order of [`KEYS`].
macro_rules! config_struct {
    ($($key:literal => $field:ident: $ty:ty,)+) => {
        /// The loaded configuration, immutable after [`load`] and shared as `Arc<Config>`. Every
        /// field holds the effective value of its key (see docs/CONFIG.md); the last fields are
        /// derived.
        #[derive(Clone, Debug)]
        pub struct Config {
            $(
                #[doc = concat!("`", $key, "` (docs/CONFIG.md).")]
                pub $field: $ty,
            )+
            /// Directory for runtime files (`DATA_DIR/run`).
            pub run_dir: PathBuf,
            /// `SERVER_PUBLIC_HOST:PUBLIC_API_PORT` as players type it (lower case, IPv6 in
            /// brackets): the origin of Google sign-in.
            pub sso_origin: String,
            /// Tag of the Google sign-in loopback redirect, derived from `sso_origin`.
            pub sso_redirect_tag: String,
            /// The official categories parsed from `RATED_CATEGORIES`.
            pub categories: Vec<Category>,
            /// Sentences about the sources found while loading (a quoted `.env` value followed by
            /// a comment, an empty secret hiding its `_FILE`, an obsolete key); the first of
            /// [`Config::warnings`].
            pub load_notes: Vec<String>,
        }

        /// The key of every field, in field order.
        #[cfg(test)]
        const FIELD_KEYS: &[&str] = &[$($key,)+];

        impl Config {
            fn from_values(mut v: load::Values, d: Derived) -> Config {
                Config {
                    $($field: FieldValue::from_val(v.take($key), $key),)+
                    run_dir: d.run_dir,
                    sso_origin: d.sso_origin,
                    sso_redirect_tag: d.sso_redirect_tag,
                    categories: d.categories,
                    load_notes: d.load_notes,
                }
            }

            fn describe_keys(&self, out: &mut Map<String, Value>) {
                $(out.insert(camel_case($key), self.$field.describe());)+
            }
        }
    };
}

config_struct! {
    "SERVER_NAME" => server_name: String,
    "SERVER_PUBLIC_HOST" => server_public_host: String,
    "SERVER_MOTD" => server_motd: String,
    "BIND_ADDRESS" => bind_address: String,
    "API_PORT" => api_port: u16,
    "WS_PORT" => ws_port: u16,
    "PUBLIC_API_PORT" => public_api_port: u16,
    "PUBLIC_WS_PORT" => public_ws_port: u16,
    "WORKERS" => workers: i64,
    "SHARD_BASE" => shard_base: i64,
    "INSTANCE_ID" => instance_id: String,
    "WS_ALLOWED_ORIGINS" => ws_allowed_origins: Vec<String>,
    "SHUTDOWN_GRACE_MS" => shutdown_grace_ms: i64,
    "LISTEN_BACKLOG" => listen_backlog: i64,
    "TLS_MODE" => tls_mode: TlsMode,
    "TLS_CERT_FILE" => tls_cert_file: String,
    "TLS_KEY_FILE" => tls_key_file: String,
    "TLS_MIN_VERSION" => tls_min_version: TlsMinVersion,
    "TRUSTED_PROXIES" => trusted_proxies: Vec<String>,
    "ALLOW_INSECURE_DEV" => allow_insecure_dev: bool,
    "DATA_DIR" => data_dir: String,
    "DB_PATH" => db_path: String,
    "JOURNAL_DIR" => journal_dir: String,
    "JOURNAL_FLUSH_MS" => journal_flush_ms: i64,
    "JOURNAL_FSYNC" => journal_fsync: bool,
    "JOURNAL_COMPACT_SEGMENTS" => journal_compact_segments: i64,
    "DB_COMMIT_MS" => db_commit_ms: i64,
    "DB_CACHE_MB" => db_cache_mb: i64,
    "DB_MMAP_MB" => db_mmap_mb: i64,
    "SERVER_SECRET" => server_secret: Secret,
    "MFA_ENCRYPTION_KEY" => mfa_encryption_key: Option<Secret>,
    "REGISTRATION" => registration: Registration,
    "REQUIRE_EMAIL_VERIFICATION" => require_email_verification: bool,
    "USERNAME_MIN" => username_min: i64,
    "USERNAME_MAX" => username_max: i64,
    "PASSWORD_MIN_LENGTH" => password_min_length: i64,
    "SESSION_IDLE_DAYS" => session_idle_days: i64,
    "SESSION_MAX_DAYS" => session_max_days: i64,
    "MAX_SESSIONS_PER_USER" => max_sessions_per_user: i64,
    "MAIL_TRANSPORT" => mail_transport: MailTransport,
    "MAIL_FROM" => mail_from: String,
    "SMTP_HOST" => smtp_host: String,
    "SMTP_PORT" => smtp_port: u16,
    "SMTP_SECURITY" => smtp_security: SmtpSecurity,
    "SMTP_USER" => smtp_user: String,
    "SMTP_PASSWORD" => smtp_password: Option<SecretText>,
    "SSO_GOOGLE_ENABLED" => sso_google_enabled: bool,
    "GOOGLE_CLIENT_ID" => google_client_id: String,
    "GOOGLE_CLIENT_SECRET" => google_client_secret: Option<SecretText>,
    "HTTP_RATE_PER_IP" => http_rate_per_ip: i64,
    "HTTP_RATE_PER_PREFIX" => http_rate_per_prefix: i64,
    "IP_CONN_RATE" => ip_conn_rate: i64,
    "IP_MAX_CONNECTIONS" => ip_max_connections: i64,
    "IP_MAX_INFLIGHT" => ip_max_inflight: i64,
    "ABUSE_BLOCK_REFUSALS_PER_MIN" => abuse_block_refusals_per_min: i64,
    "ABUSE_BLOCK_BASE_SEC" => abuse_block_base_sec: i64,
    "ABUSE_BLOCK_MAX_SEC" => abuse_block_max_sec: i64,
    "ABUSE_EXEMPT" => abuse_exempt: Vec<String>,
    "MAX_CONNECTIONS" => max_connections: i64,
    "MAX_CONNECTIONS_PER_IP" => max_connections_per_ip: i64,
    "MAX_PENDING_HANDSHAKES" => max_pending_handshakes: i64,
    "MAX_PENDING_HANDSHAKES_PER_IP" => max_pending_handshakes_per_ip: i64,
    "WS_MAX_MESSAGE_BYTES" => ws_max_message_bytes: i64,
    "WS_MSG_RATE" => ws_msg_rate: i64,
    "WS_MSG_BURST" => ws_msg_burst: i64,
    "WS_SEND_BUFFER_LIMIT" => ws_send_buffer_limit: i64,
    "WS_HELLO_TIMEOUT_MS" => ws_hello_timeout_ms: i64,
    "HEARTBEAT_INTERVAL_MS" => heartbeat_interval_ms: i64,
    "HEARTBEAT_TIMEOUT_MS" => heartbeat_timeout_ms: i64,
    "CLIENT_PING_INTERVAL_MS" => client_ping_interval_ms: i64,
    "GESTURE_RATE" => gesture_rate: i64,
    "GESTURE_BURST" => gesture_burst: i64,
    "HTTP_BODY_LIMIT" => http_body_limit: i64,
    "AUTH_RATE_PER_IP" => auth_rate_per_ip: i64,
    "AUTH_RATE_PER_PREFIX" => auth_rate_per_prefix: i64,
    "AUTH_FAILURES_PER_ACCOUNT" => auth_failures_per_account: i64,
    "AUTH_REGISTER_PER_HOUR" => auth_register_per_hour: i64,
    "AUTH_MAIL_PER_HOUR" => auth_mail_per_hour: i64,
    "AUTH_FORGOT_PER_HOUR" => auth_forgot_per_hour: i64,
    "AUTH_FORGOT_PER_DAY" => auth_forgot_per_day: i64,
    "AUTH_RESET_PER_HOUR" => auth_reset_per_hour: i64,
    "AUTH_MFA_PER_ACCOUNT" => auth_mfa_per_account: i64,
    "AUTH_REAUTH_PER_USER" => auth_reauth_per_user: i64,
    "USER_RATE_PER_MIN" => user_rate_per_min: i64,
    "CHALLENGE_UNPLAYED_PER_MIN" => challenge_unplayed_per_min: i64,
    "PRIVATE_CODE_FAILURES_PER_MIN" => private_code_failures_per_min: i64,
    "POW_REGISTER_BITS" => pow_register_bits: i64,
    "POW_LOGIN_BITS" => pow_login_bits: i64,
    "POW_LOGIN_TRIGGER_PER_MIN" => pow_login_trigger_per_min: i64,
    "PASSWORD_HASH_CONCURRENCY" => password_hash_concurrency: i64,
    "PASSWORD_HASH_QUEUE_MAX" => password_hash_queue_max: i64,
    "PASSWORD_HASH_WAITERS_PER_SOURCE" => password_hash_waiters_per_source: i64,
    "PASSWORD_HASH_QUEUE_TIMEOUT_MS" => password_hash_queue_timeout_ms: i64,
    "RATED_CATEGORIES" => rated_categories: Vec<String>,
    "ALLOW_CUSTOM_TIME_CONTROLS" => allow_custom_time_controls: bool,
    "FIRST_MOVE_TIMEOUT_MS" => first_move_timeout_ms: i64,
    "RECONNECT_GRACE_MIN_MS" => reconnect_grace_min_ms: i64,
    "RECONNECT_GRACE_MAX_MS" => reconnect_grace_max_ms: i64,
    "RECOVERY_GRACE_MS" => recovery_grace_ms: i64,
    "RECOVERY_CLOCK_HOLD_MS" => recovery_clock_hold_ms: i64,
    "LAG_COMP_MAX_MS" => lag_comp_max_ms: i64,
    "LAG_QUOTA_INITIAL_MS" => lag_quota_initial_ms: i64,
    "LAG_QUOTA_GAIN_MS" => lag_quota_gain_ms: i64,
    "LAG_QUOTA_MAX_MS" => lag_quota_max_ms: i64,
    "GAME_STALL_MIN_MS" => game_stall_min_ms: i64,
    "GAME_STALL_CREDIT_MAX_MS" => game_stall_credit_max_ms: i64,
    "AUTO_PRESS_CLOCK" => auto_press_clock: bool,
    "DRAW_OFFERS_PER_GAME" => draw_offers_per_game: i64,
    "CHALLENGE_TTL_MS" => challenge_ttl_ms: i64,
    "PRIVATE_GAME_TTL_MS" => private_game_ttl_ms: i64,
    "INITIAL_RATING" => initial_rating: i64,
    "PROVISIONAL_GAMES" => provisional_games: i64,
    "MATCH_TICK_MS" => match_tick_ms: i64,
    "MATCH_WINDOW_START" => match_window_start: i64,
    "MATCH_WINDOW_STEP" => match_window_step: i64,
    "MATCH_WINDOW_STEP_MS" => match_window_step_ms: i64,
    "MATCH_WINDOW_MAX" => match_window_max: i64,
    "MATCH_PROVISIONAL_BONUS" => match_provisional_bonus: i64,
    "MATCH_REPEAT_LIMIT" => match_repeat_limit: i64,
    "MATCH_REPEAT_WINDOW_MS" => match_repeat_window_ms: i64,
    "CONDUCT_ABANDON_LIMIT" => conduct_abandon_limit: i64,
    "AUTO_SANCTION_CERTAIN_CHEATS" => auto_sanction_certain_cheats: bool,
    "BAN_DURATION_HOURS" => ban_duration_hours: i64,
    "RATING_REFUND_DAYS" => rating_refund_days: i64,
    "ANALYSIS_ENGINE_PATH" => analysis_engine_path: String,
    "ANALYSIS_WORKERS" => analysis_workers: i64,
    "ANALYSIS_DEPTH_FAST" => analysis_depth_fast: i64,
    "ANALYSIS_DEPTH_DEEP" => analysis_depth_deep: i64,
    "ANALYSIS_MIN_PLIES" => analysis_min_plies: i64,
    "REPORTS_PER_DAY" => reports_per_day: i64,
    "ANALYSIS_HASH_MB" => analysis_hash_mb: i64,
    "ANALYSIS_POSITION_TIMEOUT_MS" => analysis_position_timeout_ms: i64,
    "ANALYSIS_POLL_MS" => analysis_poll_ms: i64,
    "ANALYSIS_QUEUE_MAX" => analysis_queue_max: i64,
    "ANALYSIS_SAMPLE_RATE" => analysis_sample_rate: f64,
    "GIF_ENABLED" => gif_enabled: bool,
    "GIF_THREADS" => gif_threads: i64,
    "GIF_QUEUE_MAX" => gif_queue_max: i64,
    "GIF_QUEUE_TIMEOUT_MS" => gif_queue_timeout_ms: i64,
    "GIF_RENDER_TIMEOUT_MS" => gif_render_timeout_ms: i64,
    "GIF_MAX_PLIES" => gif_max_plies: i64,
    "GIF_CACHE_MB" => gif_cache_mb: i64,
    "GIF_USER_RENDERS_PER_MIN" => gif_user_renders_per_min: i64,
    "GIF_USER_RENDERS_PER_HOUR" => gif_user_renders_per_hour: i64,
    "GIF_IP_RENDERS_PER_MIN" => gif_ip_renders_per_min: i64,
    "GIF_IP_RENDERS_PER_HOUR" => gif_ip_renders_per_hour: i64,
    "METRICS_PORT" => metrics_port: u16,
    "METRICS_BIND" => metrics_bind: String,
    "METRICS_TOKEN" => metrics_token: Option<SecretText>,
    "LOG_LEVEL" => log_level: LogLevel,
    "LOG_FORMAT" => log_format: LogFormat,
    "LOG_IP" => log_ip: LogIp,
    "RETENTION_SECURITY_DAYS" => retention_security_days: i64,
    "RETENTION_IP_DAYS" => retention_ip_days: i64,
    "RETENTION_INTERVAL_MS" => retention_interval_ms: i64,
}

impl Config {
    /// The configuration without secrets, as `check-config` prints it: every key in table order
    /// (camel case, effective values, secrets as `<set>`/`<unset>`), then `runDir`, `ssoOrigin`,
    /// `ssoRedirectTag` and `categories`.
    pub fn describe(&self) -> Value {
        let mut out = Map::new();
        self.describe_keys(&mut out);
        out.insert("runDir".into(), Value::String(crate::util::path::to_text(&self.run_dir)));
        out.insert("ssoOrigin".into(), Value::String(self.sso_origin.clone()));
        out.insert("ssoRedirectTag".into(), Value::String(self.sso_redirect_tag.clone()));
        let categories = self
            .categories
            .iter()
            .map(|c| {
                let mut m = Map::new();
                m.insert("id".into(), Value::String(c.id.clone()));
                m.insert("baseMs".into(), Value::from(c.base_ms));
                m.insert("incMs".into(), Value::from(c.inc_ms));
                Value::Object(m)
            })
            .collect();
        out.insert("categories".into(), Value::Array(categories));
        Value::Object(out)
    }

    /// Settings that are valid but work against the design, as sentences for the operator
    /// (logged once at start, printed by `check-config`).
    pub fn warnings(&self) -> Vec<String> {
        self.warnings_for_cores(std::thread::available_parallelism().map_or(1, |n| n.get()))
    }

    /// [`Config::warnings`] for a machine of `cores` CPU cores.
    pub fn warnings_for_cores(&self, cores: usize) -> Vec<String> {
        let mut out = self.load_notes.clone();
        if usize::try_from(self.password_hash_concurrency).is_ok_and(|n| n > cores) {
            out.push(format!(
                "PASSWORD_HASH_CONCURRENCY ({}) is above the number of CPU cores ({cores}): password hashes can then \
                 take every core, and the games wait behind them. Lower it (the default is WORKERS).",
                self.password_hash_concurrency
            ));
        }
        if self.ip_max_connections < 2 * self.max_connections_per_ip {
            out.push(format!(
                "IP_MAX_CONNECTIONS ({}) is below twice MAX_CONNECTIONS_PER_IP ({}): the players behind one address \
                 (a school, a mobile operator) could be refused before TLS while their WebSockets and API \
                 connections are still within MAX_CONNECTIONS_PER_IP. Raise IP_MAX_CONNECTIONS, or list the address \
                 in ABUSE_EXEMPT.",
                self.ip_max_connections, self.max_connections_per_ip
            ));
        }
        // A connection that only answers the pings is silent for up to an interval plus the
        // sweeper tick (250 ms) when it is checked, plus the round trip and scheduling delays.
        if self.heartbeat_timeout_ms < self.heartbeat_interval_ms + 2250 {
            out.push(format!(
                "HEARTBEAT_TIMEOUT_MS ({}) is less than HEARTBEAT_INTERVAL_MS ({}) + 2250: healthy idle connections, \
                 which only answer the server's pings, would be closed as silent ('timeout') and reconnect. Raise \
                 HEARTBEAT_TIMEOUT_MS (the default is 3 times the interval).",
                self.heartbeat_timeout_ms, self.heartbeat_interval_ms
            ));
        }
        if self.sso_google_enabled && !self.require_email_verification {
            out.push(
                "SSO_GOOGLE_ENABLED with REQUIRE_EMAIL_VERIFICATION=false: anyone can register a password account \
                 with someone else's e-mail address. Google sign-in will not open that account without its \
                 password, but the address owner cannot create a Google account with that address until she takes \
                 it back with \"Forgot password\" (needs MAIL_TRANSPORT=smtp) or you free it."
                    .to_string(),
            );
        }
        if self.sso_google_enabled && self.server_public_host.to_lowercase() == "localhost" {
            out.push(format!(
                "SSO_GOOGLE_ENABLED with SERVER_PUBLIC_HOST=localhost: Google sign-in only works for players who add \
                 this server as localhost:{}.",
                self.public_api_port
            ));
        }
        out
    }

    /// The configuration of the unit tests (the former `testConfig()` with no override): every
    /// default, TLS off with ALLOW_INSECURE_DEV, one worker, no mail, no proof of work, errors-only
    /// logs, no metrics port, a fixed secret (48 bytes of 7), `INSTANCE_ID=vm`,
    /// `DATA_DIR=data-test` and an in-memory database.
    pub fn for_tests() -> Config {
        test_config(&[]).expect("the test configuration is valid")
    }

    /// The official category of a time control, if any.
    pub fn category_of(&self, base_ms: i64, inc_ms: i64) -> Option<&Category> {
        self.categories.iter().find(|c| c.base_ms == base_ms && c.inc_ms == inc_ms)
    }

    /// The official category with this id, if any.
    pub fn category(&self, id: &str) -> Option<&Category> {
        self.categories.iter().find(|c| c.id == id)
    }
}

/// Loads the configuration of the process (environment, `.env` or `SCACELITH_ENV_FILE`, working
/// directory).
pub fn load_process() -> Result<Config, ConfigError> {
    load(&LoadOptions::from_process())
}

/// A test configuration with these keys changed (the former `testConfig(overrides)`): the
/// settings of [`Config::for_tests`], then `overrides`, no `.env` file.
pub fn test_config(overrides: &[(&str, &str)]) -> Result<Config, ConfigError> {
    let secret = crate::util::encoding::base64_encode(&[7u8; 48]);
    let base: [(&str, &str); 13] = [
        ("SERVER_SECRET", secret.as_str()),
        ("TLS_MODE", "off"),
        ("ALLOW_INSECURE_DEV", "1"),
        ("WORKERS", "1"),
        ("MAIL_TRANSPORT", "none"),
        ("POW_REGISTER_BITS", "0"),
        ("POW_LOGIN_BITS", "0"),
        ("LOG_LEVEL", "error"),
        ("METRICS_PORT", "0"),
        ("INSTANCE_ID", "vm"),
        ("DATA_DIR", "data-test"),
        ("DB_PATH", ":memory:"),
        ("SCACELITH_ENV_FILE", ""),
    ];
    let mut opts = LoadOptions::from_pairs(&base);
    for (k, v) in overrides {
        opts.env.insert(k.to_string(), v.to_string());
    }
    load(&opts)
}

/// Tag of the Google sign-in loopback redirect for an origin:
/// base64url(SHA-256("scacelith-sso-origin-v1\n" + origin)), first 22 characters.
pub fn sso_origin_tag(origin: &str) -> String {
    let digest = crate::util::sha256(format!("scacelith-sso-origin-v1\n{origin}").as_bytes());
    let mut tag = crate::util::base64url_encode(&digest);
    tag.truncate(22);
    tag
}

/// `API_PORT` -> `apiPort`: the key names of `check-config` (lower case, then each `_` followed by
/// a letter or digit removed and that character upper-cased).
pub fn camel_case(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut chars = lower.chars().peekable();
    while let Some(c) = chars.next() {
        match chars.peek() {
            Some(&n) if c == '_' && (n.is_ascii_lowercase() || n.is_ascii_digit()) => {
                out.push(n.to_ascii_uppercase());
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests;
