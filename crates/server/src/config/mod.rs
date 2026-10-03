//! Server configuration: every setting an administrator can change, read from the environment
//! and from an optional `.env` file (`KEY=value` lines), with `*_FILE` indirection for secrets.
//!
//! The key table (`keys.rs`) is the single source of truth: `.env.example`, `docs/CONFIG.md` and
//! `check-config` are generated from it. Key names, types, defaults, bounds, validation messages
//! and warnings are those of the former Node.js server, so an existing `.env` keeps working.
//!
//! Field names are the lower-case key names (`API_PORT` -> `api_port`). Derived values (resolved
//! ports, worker count, categories, SSO origin) are computed once at load time.
//!
//! Owner: foundations. Contract: docs/RUST-PORT.md section 4.1.

use std::fmt;
use std::path::PathBuf;

/// A binary secret (`secret` keys: hex, or Node's lenient base64). Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Secret(bytes)
    }
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A text secret (`secretText` keys: SMTP_PASSWORD, GOOGLE_CLIENT_SECRET, METRICS_TOKEN), raw text.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretText(String);

impl SecretText {
    pub fn new(text: String) -> Self {
        SecretText(text)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretText(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsMode {
    Native,
    Proxy,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsMinVersion {
    Tls12,
    Tls13,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registration {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailTransport {
    Smtp,
    Log,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpSecurity {
    Starttls,
    Tls,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    Json,
    Pretty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogIp {
    Truncated,
    Full,
    Hashed,
}

/// An official time-control category (`RATED_CATEGORIES`), e.g. `3+2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Category {
    pub id: String,
    pub base_ms: i64,
    pub inc_ms: i64,
}

/// The loaded configuration. Immutable after `load`; shared as `Arc<Config>`.
#[derive(Clone, Debug)]
pub struct Config {
    /// SERVER_NAME (string, default "Scacelith Community Server")
    pub server_name: String,
    /// SERVER_PUBLIC_HOST (string, default "localhost")
    pub server_public_host: String,
    /// SERVER_MOTD (string, default "")
    pub server_motd: String,
    /// BIND_ADDRESS (string, default "0.0.0.0")
    pub bind_address: String,
    /// API_PORT (port, default 443)
    pub api_port: u16,
    /// WS_PORT (port, default (no default))
    pub ws_port: u16,
    /// PUBLIC_API_PORT (port, default 0)
    pub public_api_port: u16,
    /// PUBLIC_WS_PORT (port, default 0)
    pub public_ws_port: u16,
    /// WORKERS (string, default "auto")
    pub workers: i64,
    /// SHARD_BASE (int, default 0)
    pub shard_base: i64,
    /// INSTANCE_ID (string, default "")
    pub instance_id: String,
    /// WS_ALLOWED_ORIGINS (list, default "")
    pub ws_allowed_origins: Vec<String>,
    /// SHUTDOWN_GRACE_MS (int, default 3000)
    pub shutdown_grace_ms: i64,
    /// LISTEN_REUSE_PORT (bool, default false)
    pub listen_reuse_port: bool,
    /// LISTEN_BACKLOG (int, default 2048)
    pub listen_backlog: i64,
    /// SHARD_OVERLOAD_LAG_MS (int, default 250)
    pub shard_overload_lag_ms: i64,
    /// TLS_MODE (enum, default "native")
    pub tls_mode: TlsMode,
    /// TLS_CERT_FILE (path, default "")
    pub tls_cert_file: String,
    /// TLS_KEY_FILE (path, default "")
    pub tls_key_file: String,
    /// TLS_MIN_VERSION (enum, default "TLSv1.2")
    pub tls_min_version: TlsMinVersion,
    /// TRUSTED_PROXIES (list, default "127.0.0.1,::1")
    pub trusted_proxies: Vec<String>,
    /// ALLOW_INSECURE_DEV (bool, default false)
    pub allow_insecure_dev: bool,
    /// DATA_DIR (path, default "./data")
    pub data_dir: String,
    /// DB_PATH (path, default "")
    pub db_path: String,
    /// JOURNAL_DIR (path, default "")
    pub journal_dir: String,
    /// JOURNAL_FLUSH_MS (int, default 50)
    pub journal_flush_ms: i64,
    /// JOURNAL_FSYNC (bool, default true)
    pub journal_fsync: bool,
    /// JOURNAL_COMPACT_SEGMENTS (int, default 4)
    pub journal_compact_segments: i64,
    /// DB_COMMIT_MS (int, default 50)
    pub db_commit_ms: i64,
    /// DB_CACHE_MB (int, default 64)
    pub db_cache_mb: i64,
    /// DB_MMAP_MB (int, default 256)
    pub db_mmap_mb: i64,
    /// SERVER_SECRET (secret, default (no default))
    pub server_secret: Secret,
    /// MFA_ENCRYPTION_KEY (secret, default "")
    pub mfa_encryption_key: Option<Secret>,
    /// REGISTRATION (enum, default "open")
    pub registration: Registration,
    /// REQUIRE_EMAIL_VERIFICATION (bool, default true)
    pub require_email_verification: bool,
    /// USERNAME_MIN (int, default 3)
    pub username_min: i64,
    /// USERNAME_MAX (int, default 20)
    pub username_max: i64,
    /// PASSWORD_MIN_LENGTH (int, default 10)
    pub password_min_length: i64,
    /// SESSION_IDLE_DAYS (int, default 30)
    pub session_idle_days: i64,
    /// SESSION_MAX_DAYS (int, default 90)
    pub session_max_days: i64,
    /// MAX_SESSIONS_PER_USER (int, default 10)
    pub max_sessions_per_user: i64,
    /// MAIL_TRANSPORT (enum, default "log")
    pub mail_transport: MailTransport,
    /// MAIL_FROM (string, default "Scacelith <no-reply@localhost>")
    pub mail_from: String,
    /// SMTP_HOST (string, default "")
    pub smtp_host: String,
    /// SMTP_PORT (port, default 587)
    pub smtp_port: u16,
    /// SMTP_SECURITY (enum, default "starttls")
    pub smtp_security: SmtpSecurity,
    /// SMTP_USER (string, default "")
    pub smtp_user: String,
    /// SMTP_PASSWORD (secretText, default "")
    pub smtp_password: Option<SecretText>,
    /// SSO_GOOGLE_ENABLED (bool, default false)
    pub sso_google_enabled: bool,
    /// GOOGLE_CLIENT_ID (string, default "")
    pub google_client_id: String,
    /// GOOGLE_CLIENT_SECRET (secretText, default "")
    pub google_client_secret: Option<SecretText>,
    /// HTTP_RATE_PER_IP (int, default 600)
    pub http_rate_per_ip: i64,
    /// HTTP_RATE_PER_PREFIX (int, default 0)
    pub http_rate_per_prefix: i64,
    /// IP_CONN_RATE (int, default 10)
    pub ip_conn_rate: i64,
    /// IP_MAX_CONNECTIONS (int, default 128)
    pub ip_max_connections: i64,
    /// IP_MAX_INFLIGHT (int, default 32)
    pub ip_max_inflight: i64,
    /// ABUSE_BLOCK_REFUSALS_PER_MIN (int, default 600)
    pub abuse_block_refusals_per_min: i64,
    /// ABUSE_BLOCK_BASE_SEC (int, default 60)
    pub abuse_block_base_sec: i64,
    /// ABUSE_BLOCK_MAX_SEC (int, default 3600)
    pub abuse_block_max_sec: i64,
    /// ABUSE_EXEMPT (list, default "")
    pub abuse_exempt: Vec<String>,
    /// MAX_CONNECTIONS (int, default 200000)
    pub max_connections: i64,
    /// MAX_CONNECTIONS_PER_IP (int, default 64)
    pub max_connections_per_ip: i64,
    /// MAX_PENDING_HANDSHAKES (int, default 128)
    pub max_pending_handshakes: i64,
    /// MAX_PENDING_HANDSHAKES_PER_IP (int, default (no default))
    pub max_pending_handshakes_per_ip: i64,
    /// WS_MAX_MESSAGE_BYTES (int, default 512)
    pub ws_max_message_bytes: i64,
    /// WS_MSG_RATE (int, default 20)
    pub ws_msg_rate: i64,
    /// WS_MSG_BURST (int, default 40)
    pub ws_msg_burst: i64,
    /// WS_SEND_BUFFER_LIMIT (int, default 262144)
    pub ws_send_buffer_limit: i64,
    /// WS_HELLO_TIMEOUT_MS (int, default 10000)
    pub ws_hello_timeout_ms: i64,
    /// HEARTBEAT_INTERVAL_MS (int, default 10000)
    pub heartbeat_interval_ms: i64,
    /// HEARTBEAT_TIMEOUT_MS (int, default 30000)
    pub heartbeat_timeout_ms: i64,
    /// CLIENT_PING_INTERVAL_MS (int, default 10000)
    pub client_ping_interval_ms: i64,
    /// GESTURE_RATE (int, default 4)
    pub gesture_rate: i64,
    /// GESTURE_BURST (int, default 8)
    pub gesture_burst: i64,
    /// HTTP_BODY_LIMIT (int, default 16384)
    pub http_body_limit: i64,
    /// AUTH_RATE_PER_IP (int, default 20)
    pub auth_rate_per_ip: i64,
    /// AUTH_RATE_PER_PREFIX (int, default 0)
    pub auth_rate_per_prefix: i64,
    /// AUTH_FAILURES_PER_ACCOUNT (int, default 5)
    pub auth_failures_per_account: i64,
    /// AUTH_REGISTER_PER_HOUR (int, default 10)
    pub auth_register_per_hour: i64,
    /// AUTH_MAIL_PER_HOUR (int, default 10)
    pub auth_mail_per_hour: i64,
    /// AUTH_FORGOT_PER_HOUR (int, default 3)
    pub auth_forgot_per_hour: i64,
    /// AUTH_FORGOT_PER_DAY (int, default 10)
    pub auth_forgot_per_day: i64,
    /// AUTH_RESET_PER_HOUR (int, default 10)
    pub auth_reset_per_hour: i64,
    /// AUTH_MFA_PER_ACCOUNT (int, default 10)
    pub auth_mfa_per_account: i64,
    /// AUTH_REAUTH_PER_USER (int, default 10)
    pub auth_reauth_per_user: i64,
    /// USER_RATE_PER_MIN (int, default 120)
    pub user_rate_per_min: i64,
    /// CHALLENGE_UNPLAYED_PER_MIN (int, default 5)
    pub challenge_unplayed_per_min: i64,
    /// PRIVATE_CODE_FAILURES_PER_MIN (int, default 10)
    pub private_code_failures_per_min: i64,
    /// POW_REGISTER_BITS (int, default 18)
    pub pow_register_bits: i64,
    /// POW_LOGIN_BITS (int, default 18)
    pub pow_login_bits: i64,
    /// POW_LOGIN_TRIGGER_PER_MIN (int, default 30)
    pub pow_login_trigger_per_min: i64,
    /// PASSWORD_HASH_CONCURRENCY (int, default 1)
    pub password_hash_concurrency: i64,
    /// PASSWORD_HASH_QUEUE_MAX (int, default 32)
    pub password_hash_queue_max: i64,
    /// PASSWORD_HASH_WAITERS_PER_SOURCE (int, default 2)
    pub password_hash_waiters_per_source: i64,
    /// PASSWORD_HASH_QUEUE_TIMEOUT_MS (int, default 10000)
    pub password_hash_queue_timeout_ms: i64,
    /// RATED_CATEGORIES (list, default "1+0,3+0,3+2,5+0,5+3,10+0,10+5,15+10,30+0,30+20,90+30")
    pub rated_categories: Vec<String>,
    /// ALLOW_CUSTOM_TIME_CONTROLS (bool, default true)
    pub allow_custom_time_controls: bool,
    /// FIRST_MOVE_TIMEOUT_MS (int, default 30000)
    pub first_move_timeout_ms: i64,
    /// RECONNECT_GRACE_MIN_MS (int, default 15000)
    pub reconnect_grace_min_ms: i64,
    /// RECONNECT_GRACE_MAX_MS (int, default 60000)
    pub reconnect_grace_max_ms: i64,
    /// RECOVERY_GRACE_MS (int, default 90000)
    pub recovery_grace_ms: i64,
    /// RECOVERY_CLOCK_HOLD_MS (int, default 20000)
    pub recovery_clock_hold_ms: i64,
    /// LAG_COMP_MAX_MS (int, default 1000)
    pub lag_comp_max_ms: i64,
    /// LAG_QUOTA_INITIAL_MS (int, default 2000)
    pub lag_quota_initial_ms: i64,
    /// LAG_QUOTA_GAIN_MS (int, default 100)
    pub lag_quota_gain_ms: i64,
    /// LAG_QUOTA_MAX_MS (int, default 3000)
    pub lag_quota_max_ms: i64,
    /// GAME_STALL_MIN_MS (int, default 30)
    pub game_stall_min_ms: i64,
    /// GAME_STALL_CREDIT_MAX_MS (int, default 5000)
    pub game_stall_credit_max_ms: i64,
    /// AUTO_PRESS_CLOCK (bool, default true)
    pub auto_press_clock: bool,
    /// DRAW_OFFERS_PER_GAME (int, default 3)
    pub draw_offers_per_game: i64,
    /// CHALLENGE_TTL_MS (int, default 60000)
    pub challenge_ttl_ms: i64,
    /// PRIVATE_GAME_TTL_MS (int, default 900000)
    pub private_game_ttl_ms: i64,
    /// INITIAL_RATING (int, default 1500)
    pub initial_rating: i64,
    /// PROVISIONAL_GAMES (int, default 30)
    pub provisional_games: i64,
    /// MATCH_TICK_MS (int, default 250)
    pub match_tick_ms: i64,
    /// MATCH_WINDOW_START (int, default 100)
    pub match_window_start: i64,
    /// MATCH_WINDOW_STEP (int, default 50)
    pub match_window_step: i64,
    /// MATCH_WINDOW_STEP_MS (int, default 5000)
    pub match_window_step_ms: i64,
    /// MATCH_WINDOW_MAX (int, default 500)
    pub match_window_max: i64,
    /// MATCH_PROVISIONAL_BONUS (int, default 150)
    pub match_provisional_bonus: i64,
    /// MATCH_REPEAT_LIMIT (int, default 3)
    pub match_repeat_limit: i64,
    /// MATCH_REPEAT_WINDOW_MS (int, default 3600000)
    pub match_repeat_window_ms: i64,
    /// CONDUCT_ABANDON_LIMIT (int, default 3)
    pub conduct_abandon_limit: i64,
    /// AUTO_SANCTION_CERTAIN_CHEATS (bool, default true)
    pub auto_sanction_certain_cheats: bool,
    /// BAN_DURATION_HOURS (int, default 24)
    pub ban_duration_hours: i64,
    /// RATING_REFUND_DAYS (int, default 60)
    pub rating_refund_days: i64,
    /// ANALYSIS_ENGINE_PATH (path, default "")
    pub analysis_engine_path: String,
    /// ANALYSIS_WORKERS (int, default 1)
    pub analysis_workers: i64,
    /// ANALYSIS_DEPTH_FAST (int, default 9)
    pub analysis_depth_fast: i64,
    /// ANALYSIS_DEPTH_DEEP (int, default 15)
    pub analysis_depth_deep: i64,
    /// ANALYSIS_MIN_PLIES (int, default 30)
    pub analysis_min_plies: i64,
    /// REPORTS_PER_DAY (int, default 5)
    pub reports_per_day: i64,
    /// ANALYSIS_HASH_MB (int, default 32)
    pub analysis_hash_mb: i64,
    /// ANALYSIS_POSITION_TIMEOUT_MS (int, default 120000)
    pub analysis_position_timeout_ms: i64,
    /// ANALYSIS_POLL_MS (int, default 5000)
    pub analysis_poll_ms: i64,
    /// ANALYSIS_QUEUE_MAX (int, default 5000)
    pub analysis_queue_max: i64,
    /// ANALYSIS_SAMPLE_RATE (number, default 1)
    pub analysis_sample_rate: f64,
    /// GIF_ENABLED (bool, default true)
    pub gif_enabled: bool,
    /// GIF_THREADS (int, default 1)
    pub gif_threads: i64,
    /// GIF_QUEUE_MAX (int, default 4)
    pub gif_queue_max: i64,
    /// GIF_QUEUE_TIMEOUT_MS (int, default 10000)
    pub gif_queue_timeout_ms: i64,
    /// GIF_RENDER_TIMEOUT_MS (int, default 30000)
    pub gif_render_timeout_ms: i64,
    /// GIF_MAX_PLIES (int, default 600)
    pub gif_max_plies: i64,
    /// GIF_CACHE_MB (int, default 32)
    pub gif_cache_mb: i64,
    /// GIF_USER_RENDERS_PER_MIN (int, default 4)
    pub gif_user_renders_per_min: i64,
    /// GIF_USER_RENDERS_PER_HOUR (int, default 30)
    pub gif_user_renders_per_hour: i64,
    /// GIF_IP_RENDERS_PER_MIN (int, default 12)
    pub gif_ip_renders_per_min: i64,
    /// GIF_IP_RENDERS_PER_HOUR (int, default 120)
    pub gif_ip_renders_per_hour: i64,
    /// METRICS_PORT (port, default 9464)
    pub metrics_port: u16,
    /// METRICS_BIND (string, default "127.0.0.1")
    pub metrics_bind: String,
    /// METRICS_TOKEN (secretText, default "")
    pub metrics_token: Option<SecretText>,
    /// LOG_LEVEL (enum, default "info")
    pub log_level: LogLevel,
    /// LOG_FORMAT (enum, default "json")
    pub log_format: LogFormat,
    /// LOG_IP (enum, default "truncated")
    pub log_ip: LogIp,
    /// RETENTION_SECURITY_DAYS (int, default 90)
    pub retention_security_days: i64,
    /// RETENTION_IP_DAYS (int, default 30)
    pub retention_ip_days: i64,
    /// RETENTION_INTERVAL_MS (int, default 3600000)
    pub retention_interval_ms: i64,

    // ---- Derived values (computed at load time) ----
    /// Directory for runtime files (DATA_DIR/run).
    pub run_dir: PathBuf,
    /// `SERVER_PUBLIC_HOST:PUBLIC_API_PORT` as typed by players (Google sign-in origin).
    pub sso_origin: String,
    /// Tag of the Google sign-in loopback redirect path, derived from `sso_origin`.
    pub sso_redirect_tag: String,
    /// The official categories parsed from RATED_CATEGORIES.
    pub categories: Vec<Category>,
}

impl Config {
    /// The configuration of the unit tests (the former `testConfig()` with no override): every
    /// default, TLS off with ALLOW_INSECURE_DEV, one worker, no mail, fixed secret.
    pub fn for_tests() -> Config {
        let mut c = Config {
            server_name: "Scacelith Community Server".to_string(),
            server_public_host: "localhost".to_string(),
            server_motd: "".to_string(),
            bind_address: "0.0.0.0".to_string(),
            api_port: 443,
            ws_port: 443,
            public_api_port: 443,
            public_ws_port: 443,
            workers: 1,
            shard_base: 0,
            instance_id: "vm".to_string(),
            ws_allowed_origins: vec![],
            shutdown_grace_ms: 3000,
            listen_reuse_port: false,
            listen_backlog: 2048,
            shard_overload_lag_ms: 250,
            tls_mode: TlsMode::Off,
            tls_cert_file: "".to_string(),
            tls_key_file: "".to_string(),
            tls_min_version: TlsMinVersion::Tls12,
            trusted_proxies: vec!["127.0.0.1".to_string(), "::1".to_string()],
            allow_insecure_dev: true,
            data_dir: "/home/user/scacelith-chess/dedicated-server/crates/data-test".to_string(),
            db_path: "/home/user/scacelith-chess/dedicated-server/crates/data-test/scacelith.db".to_string(),
            journal_dir: "/home/user/scacelith-chess/dedicated-server/crates/data-test/journal".to_string(),
            journal_flush_ms: 50,
            journal_fsync: true,
            journal_compact_segments: 4,
            db_commit_ms: 50,
            db_cache_mb: 64,
            db_mmap_mb: 256,
            server_secret: Secret::new(vec![7u8; 32]),
            mfa_encryption_key: None,
            registration: Registration::Open,
            require_email_verification: true,
            username_min: 3,
            username_max: 20,
            password_min_length: 10,
            session_idle_days: 30,
            session_max_days: 90,
            max_sessions_per_user: 10,
            mail_transport: MailTransport::None,
            mail_from: "Scacelith <no-reply@localhost>".to_string(),
            smtp_host: "".to_string(),
            smtp_port: 587,
            smtp_security: SmtpSecurity::Starttls,
            smtp_user: "".to_string(),
            smtp_password: None,
            sso_google_enabled: false,
            google_client_id: "".to_string(),
            google_client_secret: None,
            http_rate_per_ip: 600,
            http_rate_per_prefix: 2400,
            ip_conn_rate: 10,
            ip_max_connections: 128,
            ip_max_inflight: 32,
            abuse_block_refusals_per_min: 600,
            abuse_block_base_sec: 60,
            abuse_block_max_sec: 3600,
            abuse_exempt: vec![],
            max_connections: 200000,
            max_connections_per_ip: 64,
            max_pending_handshakes: 128,
            max_pending_handshakes_per_ip: 4,
            ws_max_message_bytes: 512,
            ws_msg_rate: 20,
            ws_msg_burst: 40,
            ws_send_buffer_limit: 262144,
            ws_hello_timeout_ms: 10000,
            heartbeat_interval_ms: 10000,
            heartbeat_timeout_ms: 30000,
            client_ping_interval_ms: 10000,
            gesture_rate: 4,
            gesture_burst: 8,
            http_body_limit: 16384,
            auth_rate_per_ip: 20,
            auth_rate_per_prefix: 100,
            auth_failures_per_account: 5,
            auth_register_per_hour: 10,
            auth_mail_per_hour: 10,
            auth_forgot_per_hour: 3,
            auth_forgot_per_day: 10,
            auth_reset_per_hour: 10,
            auth_mfa_per_account: 10,
            auth_reauth_per_user: 10,
            user_rate_per_min: 120,
            challenge_unplayed_per_min: 5,
            private_code_failures_per_min: 10,
            pow_register_bits: 0,
            pow_login_bits: 0,
            pow_login_trigger_per_min: 30,
            password_hash_concurrency: 1,
            password_hash_queue_max: 32,
            password_hash_waiters_per_source: 2,
            password_hash_queue_timeout_ms: 10000,
            rated_categories: vec!["1+0".to_string(), "3+0".to_string(), "3+2".to_string(), "5+0".to_string(), "5+3".to_string(), "10+0".to_string(), "10+5".to_string(), "15+10".to_string(), "30+0".to_string(), "30+20".to_string(), "90+30".to_string()],
            allow_custom_time_controls: true,
            first_move_timeout_ms: 30000,
            reconnect_grace_min_ms: 15000,
            reconnect_grace_max_ms: 60000,
            recovery_grace_ms: 90000,
            recovery_clock_hold_ms: 20000,
            lag_comp_max_ms: 1000,
            lag_quota_initial_ms: 2000,
            lag_quota_gain_ms: 100,
            lag_quota_max_ms: 3000,
            game_stall_min_ms: 30,
            game_stall_credit_max_ms: 5000,
            auto_press_clock: true,
            draw_offers_per_game: 3,
            challenge_ttl_ms: 60000,
            private_game_ttl_ms: 900000,
            initial_rating: 1500,
            provisional_games: 30,
            match_tick_ms: 250,
            match_window_start: 100,
            match_window_step: 50,
            match_window_step_ms: 5000,
            match_window_max: 500,
            match_provisional_bonus: 150,
            match_repeat_limit: 3,
            match_repeat_window_ms: 3600000,
            conduct_abandon_limit: 3,
            auto_sanction_certain_cheats: true,
            ban_duration_hours: 24,
            rating_refund_days: 60,
            analysis_engine_path: "".to_string(),
            analysis_workers: 1,
            analysis_depth_fast: 9,
            analysis_depth_deep: 15,
            analysis_min_plies: 30,
            reports_per_day: 5,
            analysis_hash_mb: 32,
            analysis_position_timeout_ms: 120000,
            analysis_poll_ms: 5000,
            analysis_queue_max: 5000,
            analysis_sample_rate: 1.0,
            gif_enabled: true,
            gif_threads: 1,
            gif_queue_max: 4,
            gif_queue_timeout_ms: 10000,
            gif_render_timeout_ms: 30000,
            gif_max_plies: 600,
            gif_cache_mb: 32,
            gif_user_renders_per_min: 4,
            gif_user_renders_per_hour: 30,
            gif_ip_renders_per_min: 12,
            gif_ip_renders_per_hour: 120,
            metrics_port: 0,
            metrics_bind: "127.0.0.1".to_string(),
            metrics_token: None,
            log_level: LogLevel::Error,
            log_format: LogFormat::Json,
            log_ip: LogIp::Truncated,
            retention_security_days: 90,
            retention_ip_days: 30,
            retention_interval_ms: 3600000,
            run_dir: PathBuf::from("./data-test/run"),
            sso_origin: "localhost:443".to_string(),
            sso_redirect_tag: "9MiVHYnDoNjFI41fOYxx9j".to_string(),
            categories: Vec::new(),
        };
        c.server_secret = Secret::new(vec![7u8; 48]);
        c.data_dir = "./data-test".to_string();
        c.db_path = ":memory:".to_string();
        c.journal_dir = "./data-test/journal".to_string();
        c.categories = parse_categories_for_tests(&c.rated_categories);
        c
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

fn parse_categories_for_tests(list: &[String]) -> Vec<Category> {
    list.iter()
        .filter_map(|s| {
            let (b, i) = s.split_once('+')?;
            Some(Category {
                id: s.clone(),
                base_ms: b.parse::<i64>().ok()? * 60_000,
                inc_ms: i.parse::<i64>().ok()? * 1000,
            })
        })
        .collect()
}
