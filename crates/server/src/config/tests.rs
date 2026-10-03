//! Configuration tests, ported from config.load, config.quotas, config.shield, config.docs,
//! auth.hashcap (warnings), game.recovery (the hold) and net.tls-gate (handshake slots), plus the
//! changes of the port (obsolete keys, scaled capacities).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::util::encoding;

/// A temporary directory removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("scacelith-config-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, text: &str) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, text).unwrap();
        p.to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn secret() -> String {
    encoding::base64_encode(&[7u8; 48])
}

const BASE: [(&str, &str); 6] = [
    ("TLS_MODE", "off"),
    ("ALLOW_INSECURE_DEV", "1"),
    ("WORKERS", "1"),
    ("MAIL_TRANSPORT", "none"),
    ("LOG_LEVEL", "error"),
    ("METRICS_PORT", "0"),
];

/// `loadConfig({ env: { ...BASE, ...extra }, envFile, cwd })`.
fn load_with(extra: &[(&str, &str)], env_file: Option<&str>, cwd: &Path) -> Result<Config, ConfigError> {
    let mut env: HashMap<String, String> = BASE.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    load(&LoadOptions { env, env_file: env_file.map(str::to_string), cwd: cwd.to_path_buf() })
}

fn err(overrides: &[(&str, &str)]) -> String {
    test_config(overrides).expect_err("the configuration is refused").to_string()
}

fn ok(overrides: &[(&str, &str)]) -> Config {
    test_config(overrides).unwrap_or_else(|e| panic!("{e}"))
}

fn random_bytes(n: usize) -> Vec<u8> {
    crate::util::random::bytes(n)
}

// ---- config.load.test.js ------------------------------------------------------------------------

#[test]
fn an_unreadable_scacelith_env_file_stops_the_start_a_missing_dot_env_does_not() {
    let dir = TempDir::new();
    let missing = dir.path().join("typo.env").to_string_lossy().into_owned();
    let s = secret();
    let e =
        load_with(&[("SERVER_SECRET", &s), ("SCACELITH_ENV_FILE", &missing)], None, dir.path()).unwrap_err();
    assert!(e.to_string().contains(&format!("SCACELITH_ENV_FILE: cannot read {missing} (ENOENT)")), "{e}");
    let e = load_with(&[("SERVER_SECRET", &s)], Some(&missing), dir.path()).unwrap_err();
    assert!(e.to_string().contains("cannot read"));
    // No ./.env in the directory, and '' meaning 'no file': both load.
    assert_eq!(
        load_with(&[("SERVER_SECRET", &s)], None, dir.path()).unwrap().registration,
        Registration::Open
    );
    let none = load_with(&[("SERVER_SECRET", &s), ("SCACELITH_ENV_FILE", "")], None, dir.path()).unwrap();
    assert_eq!(none.registration, Registration::Open);
    let file = dir.write("server.env", "REGISTRATION=closed\n");
    let named = load_with(&[("SERVER_SECRET", &s), ("SCACELITH_ENV_FILE", &file)], None, dir.path()).unwrap();
    assert_eq!(named.registration, Registration::Closed);
}

#[test]
fn tls_key_file_file_is_refused_and_never_read() {
    let dir = TempDir::new();
    let key = dir.write(
        "key.pem",
        "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg\n-----END PRIVATE KEY-----\n",
    );
    let e = err(&[("TLS_KEY_FILE_FILE", &key)]);
    assert!(e.contains("TLS_KEY_FILE already names the key file; TLS_KEY_FILE_FILE is not supported"));
    assert!(!e.contains("BEGIN"), "the key text is not in the message");
    let cfg = ok(&[("TLS_KEY_FILE", &key), ("TLS_KEY_FILE_FILE", "")]);
    assert_eq!(cfg.tls_key_file, key);
    assert!(!crate::util::json::to_string(&cfg.describe()).contains("BEGIN"));
}

#[test]
fn an_empty_key_does_not_hide_the_file_of_a_required_secret_an_optional_one_is_reported() {
    let dir = TempDir::new();
    let secret_text = encoding::base64_encode(&random_bytes(48));
    let mfa_text = hex::encode(random_bytes(32));
    let secret_file = dir.write("secret", &format!("{secret_text}\n"));
    let mfa_file = dir.write("mfa", &format!("{mfa_text}\n"));
    // The .env.example line 'SERVER_SECRET=' next to SERVER_SECRET_FILE.
    dir.write(".env", &format!("SERVER_SECRET=\nSERVER_SECRET_FILE={secret_file}\n"));
    let c = load_with(&[], None, dir.path()).unwrap();
    assert_eq!(c.server_secret.bytes(), encoding::base64_decode_lenient(&secret_text));
    assert_eq!(c.warnings_for_cores(64), Vec::<String>::new());
    // A required secret empty without a file is still refused.
    let e = load_with(&[("SERVER_SECRET", "")], Some(""), dir.path()).unwrap_err();
    assert!(
        e.to_string()
            .contains("SERVER_SECRET is required (Master secret (at least 32 random bytes, hex or base64)).")
    );
    // An optional secret: the empty value wins, and check-config says so.
    let s = secret();
    let m = load_with(
        &[("SERVER_SECRET", &s), ("MFA_ENCRYPTION_KEY", ""), ("MFA_ENCRYPTION_KEY_FILE", &mfa_file)],
        Some(""),
        dir.path(),
    )
    .unwrap();
    assert!(m.mfa_encryption_key.is_none());
    let w = m.warnings_for_cores(64);
    assert_eq!(w.len(), 1);
    assert!(w[0].starts_with("MFA_ENCRYPTION_KEY is set but empty, so MFA_ENCRYPTION_KEY_FILE is not read"));
    // KEY_FILE alone is read.
    let f = load_with(&[("SERVER_SECRET", &s), ("MFA_ENCRYPTION_KEY_FILE", &mfa_file)], Some(""), dir.path())
        .unwrap();
    assert_eq!(f.mfa_encryption_key.unwrap().bytes(), hex::decode(&mfa_text).unwrap());
    // A relative KEY_FILE is resolved against the working directory; an unreadable one is named.
    let r = load_with(&[("SERVER_SECRET_FILE", "secret")], Some(""), dir.path()).unwrap();
    assert_eq!(r.server_secret.bytes(), encoding::base64_decode_lenient(&secret_text));
    let e = load_with(&[("SERVER_SECRET_FILE", "nope")], Some(""), dir.path()).unwrap_err();
    assert_eq!(e.errors, ["SERVER_SECRET_FILE: cannot read nope (ENOENT)"]);
}

#[test]
fn server_name_fits_welcome_at_most_64_bytes_in_utf8() {
    assert_eq!(ok(&[("SERVER_NAME", &"x".repeat(64))]).server_name, "x".repeat(64));
    let accented = format!("Sunday club {}", "é".repeat(30));
    assert!(crate::util::utf16_len(&accented) <= 64 && accented.len() > 64);
    assert!(err(&[("SERVER_NAME", &accented)]).contains("SERVER_NAME: at most 64 bytes in UTF-8"));
    assert!(err(&[("SERVER_NAME", &"€".repeat(24))]).contains("SERVER_NAME: at most 64 bytes in UTF-8"));
    assert!(err(&[("SERVER_NAME", &"x".repeat(65))]).contains("SERVER_NAME: at most 64 characters"));
    assert!(err(&[("SERVER_NAME", "a\0b")]).contains("SERVER_NAME: no NUL character"));
    assert_eq!(ok(&[("SERVER_NAME", "Club é")]).server_name, "Club é");
}

#[test]
fn trusted_proxies_is_checked_at_load_with_tls_mode_proxy_only() {
    assert!(
        err(&[("TLS_MODE", "proxy"), ("TRUSTED_PROXIES", "127.0.0.1,localhost")])
            .contains("TRUSTED_PROXIES: ")
    );
    assert!(err(&[("TLS_MODE", "proxy"), ("TRUSTED_PROXIES", "10.0.0.0/33")]).contains("TRUSTED_PROXIES: "));
    let c = ok(&[("TLS_MODE", "proxy"), ("TRUSTED_PROXIES", "127.0.0.1, 10.0.0.0/8, ::1")]);
    assert_eq!(c.trusted_proxies, ["127.0.0.1", "10.0.0.0/8", "::1"]);
    // Unused in the other modes: a stale value does not stop a server that starts today.
    assert_eq!(ok(&[("TRUSTED_PROXIES", "localhost")]).trusted_proxies, ["localhost"]);
}

#[test]
fn check_config_warns_when_heartbeat_timeout_would_close_healthy_idle_connections() {
    assert!(ok(&[]).warnings_for_cores(64).is_empty());
    let fine = ok(&[("HEARTBEAT_INTERVAL_MS", "10000"), ("HEARTBEAT_TIMEOUT_MS", "12250")]);
    assert!(fine.warnings_for_cores(64).is_empty());
    for timeout in ["5000", "10000", "12000"] {
        let w = ok(&[("HEARTBEAT_INTERVAL_MS", "10000"), ("HEARTBEAT_TIMEOUT_MS", timeout)])
            .warnings_for_cores(64);
        assert_eq!(w.len(), 1, "{timeout}");
        assert!(w[0].starts_with(&format!(
            "HEARTBEAT_TIMEOUT_MS ({timeout}) is less than HEARTBEAT_INTERVAL_MS (10000) + 2250"
        )));
    }
}

#[test]
fn a_quoted_env_value_followed_by_a_comment_keeps_its_quotes() {
    let dir = TempDir::new();
    let hex_secret = hex::encode(random_bytes(48));
    dir.write(".env", &format!("SERVER_SECRET=\"{hex_secret}\" # rotated\n"));
    let c = load_with(&[], None, dir.path()).unwrap();
    assert_eq!(
        c.server_secret.bytes(),
        encoding::base64_decode_lenient(&format!("\"{hex_secret}\"")),
        "the effective secret does not change"
    );
    let w = c.warnings_for_cores(64);
    assert_eq!(w.len(), 1);
    assert!(w[0].starts_with("SERVER_SECRET: the value is quoted and followed by a comment"));
    assert!(!w[0].contains(&hex_secret), "the warning never shows the value");
}

#[test]
fn google_sign_in_origin_tag_and_warnings() {
    let sso: [(&str, &str); 4] = [
        ("SSO_GOOGLE_ENABLED", "1"),
        ("GOOGLE_CLIENT_ID", "id.apps.googleusercontent.com"),
        ("GOOGLE_CLIENT_SECRET", "GOCSPX-x"),
        ("SERVER_PUBLIC_HOST", "chess.example.org"),
    ];
    let with = |extra: &[(&str, &str)]| -> Config {
        let mut all = sso.to_vec();
        all.extend_from_slice(extra);
        ok(&all)
    };
    let official = ok(&[
        ("SERVER_PUBLIC_HOST", "Play.Scacelith.Example"),
        ("PUBLIC_API_PORT", "443"),
        ("API_PORT", "8443"),
    ]);
    assert_eq!(
        (official.sso_origin.as_str(), official.sso_redirect_tag.as_str()),
        ("play.scacelith.example:443", "IhcScoV7eDOzTEcSnqPUPt")
    );
    assert_eq!(ok(&[("SERVER_PUBLIC_HOST", "::1"), ("API_PORT", "8443")]).sso_origin, "[::1]:8443");
    assert_eq!(
        ok(&[("SERVER_PUBLIC_HOST", "[::1]"), ("API_PORT", "8443")]).sso_redirect_tag,
        "XToJm0DG5PjciEVmZa9Cho"
    );
    assert_eq!(ok(&[]).sso_redirect_tag, "9MiVHYnDoNjFI41fOYxx9j");

    assert!(with(&[]).warnings_for_cores(64).is_empty());
    let off = ok(&[("REQUIRE_EMAIL_VERIFICATION", "0"), ("SERVER_PUBLIC_HOST", "localhost")]);
    assert!(off.warnings_for_cores(64).is_empty(), "Google sign-in off");
    let w = with(&[("REQUIRE_EMAIL_VERIFICATION", "0")]).warnings_for_cores(64);
    assert_eq!(w.len(), 1);
    assert!(w[0].starts_with(
        "SSO_GOOGLE_ENABLED with REQUIRE_EMAIL_VERIFICATION=false: anyone can register a password account with \
         someone else's e-mail address."
    ));
    let w = with(&[("SERVER_PUBLIC_HOST", "localhost"), ("API_PORT", "8443")]).warnings_for_cores(64);
    assert_eq!(
        w,
        [
            "SSO_GOOGLE_ENABLED with SERVER_PUBLIC_HOST=localhost: Google sign-in only works for players who add this \
          server as localhost:8443."
        ]
    );
    // GOOGLE_REDIRECT_URI is no key any more: in the environment or the .env file, it is reported.
    let old = "GOOGLE_REDIRECT_URI is no longer used: Google sign-in now returns to the game on 127.0.0.1. Remove it \
               and use a \"Desktop app\" OAuth client.";
    let uri = "https://chess.example.org/auth/sso/google/callback";
    assert_eq!(ok(&[("GOOGLE_REDIRECT_URI", uri)]).warnings_for_cores(64), [old]);
    assert!(ok(&[("GOOGLE_REDIRECT_URI", "")]).warnings_for_cores(64).is_empty());
    let dir = TempDir::new();
    dir.write(".env", &format!("GOOGLE_REDIRECT_URI={uri}\n"));
    let s = secret();
    assert_eq!(load_with(&[("SERVER_SECRET", &s)], None, dir.path()).unwrap().warnings_for_cores(64), [old]);
    assert!(
        err(&[("SSO_GOOGLE_ENABLED", "1")])
            .contains("SSO_GOOGLE_ENABLED needs GOOGLE_CLIENT_ID and GOOGLE_CLIENT_SECRET.")
    );
}

// ---- config.quotas.test.js ----------------------------------------------------------------------

#[test]
fn defaults_of_the_auth_family_limits_the_account_budget_and_the_gif_settings() {
    let c = ok(&[]);
    assert_eq!(
        [
            c.auth_register_per_hour,
            c.auth_mail_per_hour,
            c.auth_forgot_per_hour,
            c.auth_forgot_per_day,
            c.auth_reset_per_hour,
            c.auth_mfa_per_account,
            c.auth_reauth_per_user,
            c.user_rate_per_min,
        ],
        [10, 10, 3, 10, 10, 10, 10, 120]
    );
    assert!(c.gif_enabled);
    assert_eq!(
        [
            c.gif_threads,
            c.gif_queue_max,
            c.gif_queue_timeout_ms,
            c.gif_render_timeout_ms,
            c.gif_max_plies,
            c.gif_cache_mb,
            c.gif_user_renders_per_min,
            c.gif_user_renders_per_hour,
            c.gif_ip_renders_per_min,
            c.gif_ip_renders_per_hour,
        ],
        [1, 4, 10000, 30000, 600, 32, 4, 30, 12, 120]
    );
}

#[test]
fn the_auth_keys_live_in_the_limits_section_the_gif_keys_in_their_own() {
    for n in [
        "AUTH_REGISTER_PER_HOUR",
        "AUTH_MAIL_PER_HOUR",
        "AUTH_FORGOT_PER_HOUR",
        "AUTH_FORGOT_PER_DAY",
        "AUTH_RESET_PER_HOUR",
        "AUTH_MFA_PER_ACCOUNT",
        "AUTH_REAUTH_PER_USER",
        "USER_RATE_PER_MIN",
    ] {
        assert_eq!(spec(n).unwrap().section, Section::Limits, "{n}");
    }
    for n in [
        "GIF_ENABLED",
        "GIF_THREADS",
        "GIF_QUEUE_MAX",
        "GIF_QUEUE_TIMEOUT_MS",
        "GIF_RENDER_TIMEOUT_MS",
        "GIF_MAX_PLIES",
        "GIF_CACHE_MB",
        "GIF_USER_RENDERS_PER_MIN",
        "GIF_USER_RENDERS_PER_HOUR",
        "GIF_IP_RENDERS_PER_MIN",
        "GIF_IP_RENDERS_PER_HOUR",
    ] {
        assert_eq!(spec(n).unwrap().section, Section::Gif, "{n}");
    }
}

#[test]
fn bounds_of_the_limits_and_gif_keys() {
    for n in [
        "AUTH_REGISTER_PER_HOUR",
        "AUTH_MAIL_PER_HOUR",
        "AUTH_FORGOT_PER_HOUR",
        "AUTH_RESET_PER_HOUR",
        "AUTH_MFA_PER_ACCOUNT",
        "AUTH_REAUTH_PER_USER",
        "USER_RATE_PER_MIN",
        "GIF_USER_RENDERS_PER_MIN",
        "GIF_IP_RENDERS_PER_MIN",
        "GIF_THREADS",
    ] {
        assert!(err(&[(n, "0")]).contains(&format!("{n}: at least 1")), "{n}");
    }
    // Whole-server values now (the former per-process bounds were 8 and 64).
    assert!(err(&[("GIF_THREADS", "65")]).contains("GIF_THREADS: at most 64"));
    assert!(err(&[("GIF_QUEUE_MAX", "1025")]).contains("GIF_QUEUE_MAX: at most 1024"));
    assert!(err(&[("GIF_QUEUE_TIMEOUT_MS", "99")]).contains("GIF_QUEUE_TIMEOUT_MS: at least 100"));
    assert!(err(&[("GIF_RENDER_TIMEOUT_MS", "120001")]).contains("GIF_RENDER_TIMEOUT_MS: at most 120000"));
    assert!(
        err(&[("GIF_MAX_PLIES", "1201")]).contains("GIF_MAX_PLIES: at most 1200"),
        "the renderer draws 1200 plies at most"
    );
    assert!(err(&[("GIF_CACHE_MB", "-1")]).contains("GIF_CACHE_MB: at least 0"));
    assert!(err(&[("GIF_ENABLED", "maybe")]).contains("GIF_ENABLED: true or false expected"));
    let off = ok(&[("GIF_QUEUE_MAX", "0"), ("GIF_CACHE_MB", "0"), ("GIF_ENABLED", "false")]);
    assert!(!off.gif_enabled, "no queue, no cache, off");
}

#[test]
fn a_longer_window_may_not_allow_fewer_than_a_shorter_one() {
    assert!(
        err(&[("AUTH_FORGOT_PER_HOUR", "5"), ("AUTH_FORGOT_PER_DAY", "4")])
            .contains("AUTH_FORGOT_PER_DAY must be at least AUTH_FORGOT_PER_HOUR")
    );
    assert!(
        err(&[("GIF_USER_RENDERS_PER_MIN", "31")])
            .contains("GIF_USER_RENDERS_PER_HOUR must be at least GIF_USER_RENDERS_PER_MIN")
    );
    assert!(
        err(&[("GIF_IP_RENDERS_PER_HOUR", "11")])
            .contains("GIF_IP_RENDERS_PER_HOUR must be at least GIF_IP_RENDERS_PER_MIN")
    );
    let c = ok(&[
        ("AUTH_FORGOT_PER_HOUR", "10"),
        ("AUTH_FORGOT_PER_DAY", "10"),
        ("GIF_USER_RENDERS_PER_MIN", "30"),
        ("GIF_IP_RENDERS_PER_MIN", "120"),
    ]);
    assert_eq!(c.auth_forgot_per_day, 10, "equal is allowed");
}

// ---- config.shield.test.js ----------------------------------------------------------------------

const SHIELD_KEYS: [&str; 9] = [
    "HTTP_RATE_PER_IP",
    "HTTP_RATE_PER_PREFIX",
    "IP_CONN_RATE",
    "IP_MAX_CONNECTIONS",
    "IP_MAX_INFLIGHT",
    "ABUSE_BLOCK_REFUSALS_PER_MIN",
    "ABUSE_BLOCK_BASE_SEC",
    "ABUSE_BLOCK_MAX_SEC",
    "ABUSE_EXEMPT",
];

#[test]
fn protection_per_address_defaults() {
    let c = ok(&[]);
    assert_eq!(
        [c.http_rate_per_ip, c.http_rate_per_prefix, c.ip_conn_rate, c.ip_max_connections, c.ip_max_inflight],
        [600, 2400, 10, 128, 32]
    );
    assert_eq!(
        [c.abuse_block_refusals_per_min, c.abuse_block_base_sec, c.abuse_block_max_sec],
        [600, 60, 3600]
    );
    assert!(c.abuse_exempt.is_empty());
    assert_eq!(c.max_connections_per_ip, 64, "WebSockets per address: 64");
    for name in SHIELD_KEYS {
        let k = spec(name).unwrap();
        assert_eq!(k.section, Section::Abuse, "{name} is in the per-address section");
        assert!(k.desc.len() > 40, "{name} has a description");
    }
}

#[test]
fn gesture_keepalive_defaults_to_one_second_within_one_to_ten() {
    assert_eq!(ok(&[]).gesture_idle_ms, 1000);
    assert_eq!(ok(&[("GESTURE_IDLE_MS", "10000")]).gesture_idle_ms, 10_000);
    assert!(err(&[("GESTURE_IDLE_MS", "999")]).contains("GESTURE_IDLE_MS: at least 1000"));
    assert!(err(&[("GESTURE_IDLE_MS", "10001")]).contains("GESTURE_IDLE_MS: at most 10000"));
    let off = ok(&[("GESTURE_RATE", "0"), ("GESTURE_IDLE_MS", "4000")]);
    assert_eq!(off.gesture_idle_ms, 4000, "accepted with the relay off (Welcome then says 0)");
    assert_eq!(spec("GESTURE_IDLE_MS").unwrap().section, Section::Limits);
}

#[test]
fn http_rate_per_prefix_zero_means_four_times_http_rate_per_ip() {
    assert_eq!(ok(&[("HTTP_RATE_PER_IP", "100")]).http_rate_per_prefix, 400);
    assert_eq!(ok(&[("HTTP_RATE_PER_IP", "100"), ("HTTP_RATE_PER_PREFIX", "100")]).http_rate_per_prefix, 100);
    assert_eq!(
        ok(&[("HTTP_RATE_PER_IP", "100"), ("HTTP_RATE_PER_PREFIX", "5000")]).http_rate_per_prefix,
        5000
    );
    assert!(
        err(&[("HTTP_RATE_PER_IP", "100"), ("HTTP_RATE_PER_PREFIX", "99")])
            .contains("HTTP_RATE_PER_PREFIX must be 0 or at least HTTP_RATE_PER_IP")
    );
    assert_eq!(ok(&[("AUTH_RATE_PER_IP", "7")]).auth_rate_per_prefix, 35);
    assert_eq!(ok(&[("AUTH_RATE_PER_PREFIX", "3")]).auth_rate_per_prefix, 3);
}

#[test]
fn protection_bounds() {
    for name in [
        "HTTP_RATE_PER_IP",
        "IP_CONN_RATE",
        "IP_MAX_CONNECTIONS",
        "IP_MAX_INFLIGHT",
        "ABUSE_BLOCK_BASE_SEC",
        "ABUSE_BLOCK_MAX_SEC",
    ] {
        assert!(err(&[(name, "0")]).contains(&format!("{name}: at least 1")), "{name}");
        assert!(err(&[(name, "many")]).contains(name), "{name}");
    }
    assert!(err(&[("HTTP_RATE_PER_PREFIX", "-1")]).contains("HTTP_RATE_PER_PREFIX"));
    assert!(err(&[("IP_CONN_RATE", "100001")]).contains("IP_CONN_RATE: at most 100000"));
    assert!(err(&[("ABUSE_BLOCK_MAX_SEC", "604801")]).contains("ABUSE_BLOCK_MAX_SEC: at most 604800"));
    assert_eq!(ok(&[("ABUSE_BLOCK_REFUSALS_PER_MIN", "0")]).abuse_block_refusals_per_min, 0);
}

#[test]
fn abuse_block_base_must_not_exceed_max() {
    assert_eq!(
        ok(&[("ABUSE_BLOCK_BASE_SEC", "600"), ("ABUSE_BLOCK_MAX_SEC", "600")]).abuse_block_base_sec,
        600
    );
    assert!(
        err(&[("ABUSE_BLOCK_BASE_SEC", "601"), ("ABUSE_BLOCK_MAX_SEC", "600")])
            .contains("ABUSE_BLOCK_BASE_SEC must not exceed ABUSE_BLOCK_MAX_SEC")
    );
    assert!(
        err(&[("ABUSE_BLOCK_BASE_SEC", "7200")]).contains("ABUSE_BLOCK_BASE_SEC must not exceed"),
        "the default maximum"
    );
}

#[test]
fn abuse_exempt_is_parsed_at_load() {
    let c = ok(&[("ABUSE_EXEMPT", " 203.0.113.7 , 2001:db8:12::/48,10.0.0.0/8 ")]);
    assert_eq!(c.abuse_exempt, ["203.0.113.7", "2001:db8:12::/48", "10.0.0.0/8"]);
    for bad in ["school.example.org", "10.0.0.0/33", "2001:db8::/129", "300.1.2.3"] {
        assert!(err(&[("ABUSE_EXEMPT", &format!("203.0.113.7,{bad}"))]).contains("ABUSE_EXEMPT"), "{bad}");
    }
    assert!(
        err(&[("ABUSE_EXEMPT", "10.0.0.0/33")]).contains("ABUSE_EXEMPT: bad prefix length: \"10.0.0.0/33\".")
    );
}

#[test]
fn check_config_warns_when_ip_max_connections_is_below_twice_max_connections_per_ip() {
    let warns = |o: &[(&str, &str)]| -> Vec<String> {
        ok(o).warnings_for_cores(64).into_iter().filter(|w| w.starts_with("IP_MAX_CONNECTIONS")).collect()
    };
    assert!(warns(&[]).is_empty(), "the defaults: 128 and 64");
    assert!(warns(&[("IP_MAX_CONNECTIONS", "200"), ("MAX_CONNECTIONS_PER_IP", "100")]).is_empty());
    let w = warns(&[("IP_MAX_CONNECTIONS", "100"), ("MAX_CONNECTIONS_PER_IP", "64")]);
    assert!(w[0].contains("IP_MAX_CONNECTIONS (100) is below twice MAX_CONNECTIONS_PER_IP (64)"));
    assert!(w[0].contains("ABUSE_EXEMPT"), "names the way out for a known shared address");
}

// ---- Generated files (config.quotas, config.shield) ---------------------------------------------

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn env_example_and_config_md_are_generated_from_the_current_keys() {
    assert_eq!(
        stale_files(&server_dir()),
        Vec::<&str>::new(),
        "run `cargo run -p scacelith-server -- gen-config-docs` in dedicated-server/"
    );
    let env = render_env_example();
    let doc = render_config_doc();
    assert!(env.contains("Protection per address (background layer)"));
    assert!(env.lines().any(|l| l == "# HTTP_RATE_PER_IP=600"));
    assert!(env.lines().any(|l| l == "# MAX_CONNECTIONS_PER_IP=64"));
    assert!(env.lines().any(|l| l == "SERVER_SECRET="), "a required key is uncommented and empty");
    assert!(env.lines().any(|l| l == "# MFA_ENCRYPTION_KEY="), "a secret never carries a value");
    assert!(env.lines().any(|l| l == "# MAX_PENDING_HANDSHAKES="), "a derived default is empty");
    assert!(env.lines().all(|l| crate::util::utf16_len(l) <= 100 || !l.contains(' ')));
    for name in SHIELD_KEYS {
        assert!(env.contains(name) && doc.contains(name), "{name}");
    }
    assert!(doc.contains("| `SERVER_SECRET`<br>`SERVER_SECRET_FILE` | secret (at least 32 bytes, hex or base64) | **required** |"));
    assert!(
        doc.contains(
            "- [Protection per address (background layer)](#protection-per-address-background-layer)"
        )
    );
    for (obsolete, _) in OBSOLETE {
        assert!(!env.lines().any(|l| l.starts_with(&format!("# {obsolete}="))), "{obsolete}");
        assert!(doc.contains(&format!("`{obsolete}`")), "the obsolete keys are listed: {obsolete}");
    }
}

// ---- config.docs.test.js --------------------------------------------------------------------

#[test]
fn heartbeat_interval_gives_the_game_clients_probe_and_dead_connection_bounds() {
    let client = server_dir().join("../src/net/online_client.cpp");
    let Ok(src) = std::fs::read_to_string(&client) else {
        return; // client sources not present
    };
    let flat: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
    let pattern = "silence = std::max<uint32_t>(";
    let Some(at) = flat.find(pattern) else {
        panic!("the liveness limit of online_client.cpp changed: update HEARTBEAT_INTERVAL_MS and this test");
    };
    // uint32_t silence = std::max<uint32_t>(10000, std::min<uint32_t>(rt.heartbeatMs, 60000) * 2);
    let rest = &flat[at + pattern.len()..];
    let number = |s: &str| -> f64 { s.trim().parse().expect("a number in the liveness limit") };
    let floor = number(rest.split(',').next().unwrap());
    let (_, after) =
        rest.split_once("std::min<uint32_t>(rt.heartbeatMs,").expect("the limit follows the interval");
    let (cap, tail) = after.split_once(')').unwrap();
    assert!(tail.trim_start().starts_with("* 2"), "twice the interval");
    assert!(
        flat.contains("quiet > std::chrono::milliseconds(silence / 4 * 3)"),
        "the probe comes at 3/4 of the limit"
    );
    let (floor_s, cap_s) = (floor / 1000.0, number(cap) * 2.0 / 1000.0);
    let desc = spec("HEARTBEAT_INTERVAL_MS").unwrap().desc;
    let n = crate::util::js_number;
    assert!(
        desc.contains(&format!("dead after twice this (at least {} s, at most {} s)", n(floor_s), n(cap_s))),
        "{desc}"
    );
    assert!(
        desc.contains(&format!(
            "after 1.5 times this with nothing received (at least {} s, at most {} s)",
            n(floor_s * 0.75),
            n(cap_s * 0.75)
        )),
        "{desc}"
    );
}

#[test]
fn user_rate_and_body_limit_descriptions() {
    // One process: the account budget is exact, there is no share per worker any more.
    let desc = spec("USER_RATE_PER_MIN").unwrap().desc;
    assert!(desc.contains("whole server") && !desc.contains("WORKERS"), "{desc}");
    let desc = spec("HTTP_BODY_LIMIT").unwrap().desc;
    assert!(
        desc.contains("except POST /api/v1/gif, which has its own fixed limit of 135,168 bytes"),
        "{desc}"
    );
}

// ---- auth.hashcap.test.js (warning), game.recovery.test.js, net.tls-gate.test.js -------------------

#[test]
fn check_config_warns_when_password_hashes_could_take_every_core() {
    assert!(ok(&[]).warnings_for_cores(1).is_empty());
    assert!(ok(&[("PASSWORD_HASH_CONCURRENCY", "4")]).warnings_for_cores(4).is_empty());
    let w = ok(&[("PASSWORD_HASH_CONCURRENCY", "5")]).warnings_for_cores(4);
    assert_eq!(w.len(), 1);
    assert!(w[0].starts_with("PASSWORD_HASH_CONCURRENCY (5) is above the number of CPU cores (4)"));
}

#[test]
fn recovery_clock_hold_follows_a_short_grace_down_and_only_a_set_value_is_refused() {
    assert_eq!(ok(&[]).recovery_clock_hold_ms, 20000);
    assert_eq!(ok(&[("RECOVERY_CLOCK_HOLD_MS", "0")]).recovery_clock_hold_ms, 0);
    assert!(err(&[("RECOVERY_CLOCK_HOLD_MS", "-1")]).contains("RECOVERY_CLOCK_HOLD_MS: at least 0."));
    assert!(test_config(&[("RECOVERY_GRACE_MS", "5000")]).is_err());
    assert_eq!(
        ok(&[("RECOVERY_GRACE_MS", "20000"), ("RECOVERY_CLOCK_HOLD_MS", "19999")]).recovery_clock_hold_ms,
        19999
    );
    for (grace, hold) in [(15000, 14999), (19999, 19998), (20000, 19999), (20001, 20000), (3_600_000, 20000)]
    {
        assert_eq!(ok(&[("RECOVERY_GRACE_MS", &grace.to_string())]).recovery_clock_hold_ms, hold, "{grace}");
    }
    assert_eq!(
        ok(&[("RECOVERY_GRACE_MS", "15000"), ("RECOVERY_CLOCK_HOLD_MS", "")]).recovery_clock_hold_ms,
        14999
    );
    for (grace, hold) in [("15000", "20000"), ("20000", "20000")] {
        assert!(
            err(&[("RECOVERY_GRACE_MS", grace), ("RECOVERY_CLOCK_HOLD_MS", hold)])
                .contains("RECOVERY_CLOCK_HOLD_MS must be lower than RECOVERY_GRACE_MS")
        );
    }
    assert_eq!(
        ok(&[("RECOVERY_GRACE_MS", "15000"), ("RECOVERY_CLOCK_HOLD_MS", "5000")]).recovery_clock_hold_ms,
        5000
    );
    assert_eq!(
        ok(&[("RECOVERY_GRACE_MS", "90000"), ("RECOVERY_CLOCK_HOLD_MS", "60000")]).recovery_clock_hold_ms,
        60000
    );
}

#[test]
fn handshake_slots_per_address_group() {
    let defaults: Vec<i64> =
        [2, 3, 32, 64, 100, 1000, 100_000].into_iter().map(default_pending_per_group).collect();
    assert_eq!(defaults, [1, 2, 2, 2, 3, 31, 3125]);
    for n in [2, 3, 5, 64, 128, 4096] {
        assert!(default_pending_per_group(n) < n, "below the total ({n})");
    }
    assert_eq!(ok(&[]).max_pending_handshakes_per_ip, 4);
    assert_eq!(ok(&[("MAX_PENDING_HANDSHAKES", "2")]).max_pending_handshakes_per_ip, 1);
    assert_eq!(ok(&[("MAX_PENDING_HANDSHAKES_PER_IP", "9")]).max_pending_handshakes_per_ip, 9);
    assert!(err(&[("MAX_PENDING_HANDSHAKES_PER_IP", "128")]).contains(
        "MAX_PENDING_HANDSHAKES_PER_IP must be lower than MAX_PENDING_HANDSHAKES (one address group could otherwise \
         hold every handshake slot)."
    ));
}

// ---- The port: obsolete keys, WORKERS, scaled capacities, check-config ------------------------------

#[test]
fn obsolete_keys_get_a_warning_never_an_error() {
    let c = ok(&[("SHARD_OVERLOAD_LAG_MS", "250"), ("LISTEN_REUSE_PORT", "1"), ("UV_THREADPOOL_SIZE", "8")]);
    let w = c.warnings_for_cores(64);
    assert_eq!(w.len(), 3);
    assert!(w[0].starts_with("SHARD_OVERLOAD_LAG_MS is no longer used"));
    assert!(w[1].starts_with("LISTEN_REUSE_PORT is no longer used"));
    assert!(w[2].starts_with("UV_THREADPOOL_SIZE is a Node.js setting"));
    // Even a value the former server refused: the key is not read any more.
    assert!(
        ok(&[("SHARD_OVERLOAD_LAG_MS", "x")]).warnings_for_cores(64)[0].starts_with("SHARD_OVERLOAD_LAG_MS")
    );
    let dir = TempDir::new();
    dir.write(".env", "LISTEN_REUSE_PORT=true\nSHARD_OVERLOAD_LAG_MS=\n");
    let s = secret();
    let w = load_with(&[("SERVER_SECRET", &s)], None, dir.path()).unwrap().warnings_for_cores(64);
    assert_eq!(w.len(), 1, "an empty value is no setting");
    assert!(w[0].starts_with("LISTEN_REUSE_PORT is no longer used"));
}

#[test]
fn workers_shards_and_threads() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    assert_eq!(ok(&[("WORKERS", "auto")]).workers, cores.clamp(1, 16) as i64);
    assert_eq!(ok(&[("WORKERS", " AUTO ")]).workers, cores.clamp(1, 16) as i64);
    assert_eq!(ok(&[("WORKERS", "")]).workers, cores.clamp(1, 16) as i64, "empty is the default");
    assert_eq!(ok(&[("WORKERS", "64")]).workers, 64);
    for bad in ["0", "65", "-1", "two", "1.5"] {
        assert!(err(&[("WORKERS", bad)]).contains("WORKERS: \"auto\" or a number from 1 to 64."), "{bad}");
    }
    assert!(
        err(&[("WORKERS", "9"), ("SHARD_BASE", "56")])
            .contains("SHARD_BASE + WORKERS must not exceed 64 (game ids hold 6 bits of shard).")
    );
    assert_eq!(ok(&[("WORKERS", "8"), ("SHARD_BASE", "56")]).shard_base, 56);
}

#[test]
fn per_process_capacities_scale_with_workers() {
    let c = ok(&[("WORKERS", "4")]);
    assert_eq!(
        [
            c.max_pending_handshakes,
            c.max_pending_handshakes_per_ip,
            c.ip_max_inflight,
            c.password_hash_concurrency,
            c.password_hash_queue_max,
            c.password_hash_waiters_per_source,
            c.gif_threads,
            c.gif_queue_max,
            c.gif_cache_mb,
            c.db_cache_mb,
        ],
        [512, 16, 128, 4, 128, 8, 4, 16, 128, 320]
    );
    let one = ok(&[]);
    assert_eq!(
        [
            one.max_pending_handshakes,
            one.password_hash_concurrency,
            one.password_hash_queue_max,
            one.db_cache_mb
        ],
        [128, 1, 32, 128]
    );
    let set = ok(&[
        ("WORKERS", "4"),
        ("MAX_PENDING_HANDSHAKES", "64"),
        ("PASSWORD_HASH_QUEUE_MAX", "0"),
        ("GIF_THREADS", "1"),
    ]);
    assert_eq!(
        [
            set.max_pending_handshakes,
            set.max_pending_handshakes_per_ip,
            set.password_hash_queue_max,
            set.gif_threads
        ],
        [64, 2, 0, 1]
    );
}

#[test]
fn check_config_prints_every_key_in_table_order_then_the_derived_values() {
    assert_eq!(
        FIELD_KEYS,
        KEYS.iter().map(|k| k.name).collect::<Vec<_>>().as_slice(),
        "one field per key, in table order"
    );
    let c = ok(&[("SMTP_PASSWORD", "hunter2")]);
    let d = c.describe();
    let obj = d.as_object().unwrap();
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    let mut want: Vec<String> = KEYS.iter().map(|k| camel_case(k.name)).collect();
    want.extend(["ssoOrigin", "ssoRedirectTag", "categories"].map(String::from));
    assert_eq!(keys, want);
    assert_eq!(keys.len(), 158);
    assert_eq!(d["serverSecret"], "<set>");
    assert_eq!(d["mfaEncryptionKey"], "<unset>");
    assert_eq!(d["smtpPassword"], "<set>");
    assert_eq!(d["metricsToken"], "<unset>");
    assert_eq!(d["wsPort"], 443);
    assert_eq!(d["publicApiPort"], 443);
    assert_eq!(d["workers"], 1);
    assert_eq!(d["maxPendingHandshakesPerIp"], 4);
    assert_eq!(d["recoveryClockHoldMs"], 20000);
    assert_eq!(d["httpRatePerPrefix"], 2400);
    assert_eq!(d["authRatePerPrefix"], 100);
    assert_eq!(d["tlsMinVersion"], "TLSv1.2");
    assert_eq!(d["analysisSampleRate"], 1.0);
    assert_eq!(d["trustedProxies"], serde_json::json!(["127.0.0.1", "::1"]));
    assert_eq!(d["dbPath"], ":memory:");
    assert_eq!(d["categories"][2], serde_json::json!({"id": "3+2", "baseMs": 180000, "incMs": 2000}));
    let text = crate::util::json::to_string_pretty(&d);
    assert!(text.contains("\n  \"analysisSampleRate\": 1,\n"), "numbers print as JavaScript does");
    assert!(!text.contains("hunter2"));
}

#[test]
fn derived_paths_instance_and_errors() {
    let dir = TempDir::new();
    let s = secret();
    let c =
        load_with(&[("SERVER_SECRET", &s), ("DATA_DIR", "./var/../state")], Some(""), dir.path()).unwrap();
    let root = dir.path().to_string_lossy().into_owned();
    assert_eq!(c.data_dir, format!("{root}/state"));
    assert_eq!(c.db_path, format!("{root}/state/scacelith.db"));
    assert_eq!(c.journal_dir, format!("{root}/state/journal"));
    assert_eq!(c.instance_id, crate::sys::hostname(), "INSTANCE_ID defaults to the host name");
    let d = load_with(
        &[("SERVER_SECRET", &s), ("DB_PATH", "/srv/db.sqlite"), ("TLS_CERT_FILE", "tls/c.pem")],
        Some(""),
        dir.path(),
    )
    .unwrap();
    assert_eq!(d.data_dir, format!("{root}/data"));
    assert_eq!(d.db_path, "/srv/db.sqlite");
    assert_eq!(d.tls_cert_file, format!("{root}/tls/c.pem"));
    assert_eq!(d.tls_key_file, "", "an empty path stays empty");

    // Every problem at once, in order, in the former format.
    let e = test_config(&[
        ("API_PORT", "70000"),
        ("USERNAME_MIN", "30"),
        ("JOURNAL_FSYNC", "maybe"),
        ("TLS_MODE", "Off"),
    ])
    .unwrap_err();
    assert_eq!(
        e.errors,
        [
            "API_PORT: at most 65535.",
            "TLS_MODE: one of native, proxy, off.",
            "JOURNAL_FSYNC: true or false expected.",
            "USERNAME_MIN: at most 24.",
            "USERNAME_MIN must not exceed USERNAME_MAX.",
        ]
    );
    assert!(
        e.to_string().starts_with("Invalid configuration:\n  - API_PORT: at most 65535.\n  - TLS_MODE: ")
    );
    assert_eq!(
        err(&[("SHUTDOWN_GRACE_MS", " 12x")]),
        "Invalid configuration:\n  - SHUTDOWN_GRACE_MS: integer expected, got \" 12x\"."
    );
    assert!(
        err(&[("ANALYSIS_SAMPLE_RATE", "1e-1")])
            .contains("ANALYSIS_SAMPLE_RATE: number expected, got \"1e-1\".")
    );
    assert!(err(&[("ANALYSIS_SAMPLE_RATE", "1.5")]).contains("ANALYSIS_SAMPLE_RATE: at most 1."));
    assert_eq!(ok(&[("ANALYSIS_SAMPLE_RATE", " .25 ")]).analysis_sample_rate, 0.25);
    assert!(
        err(&[("MAX_CONNECTIONS", "99999999999999999999")])
            .contains("MAX_CONNECTIONS: at most 9007199254740991.")
    );
    assert_eq!(ok(&[("MAX_CONNECTIONS", " 007 ")]).max_connections, 7);
    assert!(err(&[("TLS_MODE", "native")]).contains("TLS_MODE=native needs TLS_CERT_FILE and TLS_KEY_FILE."));
    assert!(
        err(&[("ALLOW_INSECURE_DEV", "0")]).contains("TLS_MODE=off is refused unless ALLOW_INSECURE_DEV=1")
    );
    assert!(err(&[("MAIL_TRANSPORT", "smtp")]).contains("MAIL_TRANSPORT=smtp needs SMTP_HOST."));
    assert!(
        err(&[("ANALYSIS_DEPTH_FAST", "15")])
            .contains("ANALYSIS_DEPTH_FAST must be lower than ANALYSIS_DEPTH_DEEP.")
    );
    assert!(
        err(&[("RATED_CATEGORIES", "3+2,0+5")])
            .contains("RATED_CATEGORIES: \"0+5\" is not minutes+seconds (e.g. 3+2).")
    );
    assert!(
        err(&[("SERVER_SECRET", "c2hvcnQ=")])
            .contains("SERVER_SECRET: at least 32 bytes of entropy (hex or base64).")
    );
    assert!(!ok(&[("JOURNAL_FSYNC", " OFF ")]).journal_fsync);
    assert_eq!(
        ok(&[("WS_ALLOWED_ORIGINS", " https://a.example, ,https://b.example,")]).ws_allowed_origins,
        ["https://a.example", "https://b.example"]
    );
}

#[test]
fn the_test_configuration() {
    let c = Config::for_tests();
    assert_eq!(c.server_secret.bytes(), [7u8; 48]);
    assert_eq!((c.tls_mode, c.allow_insecure_dev, c.workers), (TlsMode::Off, true, 1));
    assert_eq!((c.mail_transport, c.log_level, c.metrics_port), (MailTransport::None, LogLevel::Error, 0));
    assert_eq!((c.pow_register_bits, c.pow_login_bits), (0, 0));
    assert_eq!((c.instance_id.as_str(), c.db_path.as_str()), ("vm", ":memory:"));
    assert_eq!((c.ws_port, c.public_api_port, c.public_ws_port), (443, 443, 443));
    assert_eq!(c.category("3+2").map(|k| (k.base_ms, k.inc_ms)), Some((180_000, 2000)));
    assert_eq!(c.category_of(5_400_000, 30_000).map(|k| k.id.as_str()), Some("90+30"));
    assert!(c.category_of(60_000, 1000).is_none());
    assert_eq!(c.categories.len(), 11);
    assert!(c.load_notes.is_empty());
}

#[test]
fn camel_case_and_origin_tags() {
    assert_eq!(camel_case("API_PORT"), "apiPort");
    assert_eq!(camel_case("GIF_IP_RENDERS_PER_MIN"), "gifIpRendersPerMin");
    assert_eq!(camel_case("TLS_MIN_VERSION"), "tlsMinVersion");
    assert_eq!(camel_case("A__B_"), "a_B_");
    assert_eq!(camel_case("X_1"), "x1");
    assert_eq!(sso_origin_tag("localhost:443"), "9MiVHYnDoNjFI41fOYxx9j");
    assert_eq!(sso_origin_tag("play.scacelith.example:443"), "IhcScoV7eDOzTEcSnqPUPt");
    assert_eq!(sso_origin_tag("[::1]:8443"), "XToJm0DG5PjciEVmZa9Cho");
}

#[test]
fn enums_round_trip_their_texts() {
    for k in KEYS {
        if let Kind::Enum(values) = k.kind {
            for v in values {
                // With what native TLS and SMTP need.
                let c = ok(&[
                    (k.name, v),
                    ("TLS_CERT_FILE", "c.pem"),
                    ("TLS_KEY_FILE", "k.pem"),
                    ("SMTP_HOST", "mx"),
                ]);
                assert_eq!(c.describe()[camel_case(k.name)], *v, "{}", k.name);
            }
        }
    }
    assert_eq!(TlsMinVersion::parse("TLSv1.3"), Some(TlsMinVersion::Tls13));
    assert_eq!(LogIp::Hashed.as_str(), "hashed");
    assert!(LogLevel::Debug < LogLevel::Error);
}
