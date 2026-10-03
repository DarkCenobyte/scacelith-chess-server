//! Command line of the `scacelith-server` binary (docs/RUST-PORT.md sections 9 and 10):
//!
//! ```text
//! scacelith-server [start]                    start the server
//! scacelith-server migrate                    apply the database migrations and exit
//! scacelith-server check-config               validate the configuration and print it (secrets hidden)
//! scacelith-server gen-secret                 print a new random SERVER_SECRET value
//! scacelith-server gen-config-docs [--check]  write .env.example and docs/CONFIG.md
//! scacelith-server admin ...                  administration commands
//! scacelith-server version | help
//! ```
//!
//! Exit codes: 0 success, 1 failure (an invalid configuration included), 2 usage. Configuration:
//! environment variables and `./.env` (or `SCACELITH_ENV_FILE`); see `.env.example` and
//! docs/CONFIG.md.

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::config::{self, Config, LoadOptions};

/// The help text.
pub const USAGE: &str = "Usage: scacelith-server [command]

Commands:
  start            Start the server (default).
  migrate          Apply the database migrations, then exit.
  check-config     Validate the configuration and print it without secrets
                   (warnings about risky settings go to stderr).
  gen-secret       Print a new random value for SERVER_SECRET.
  gen-config-docs  Write .env.example and docs/CONFIG.md from the configuration keys
                   (--check: only report the files that are out of date).
                   Run it in the dedicated-server directory.
  admin            Administration commands (scacelith-server admin --help).
  version          Print the version.
  help             Show this help.
";

/// Runs the command line (`args[0]` is the program name) and returns the process exit code.
pub fn run(args: Vec<String>) -> i32 {
    let rest = args.get(1..).unwrap_or_default();
    let code = catch_unwind(AssertUnwindSafe(|| {
        execute(rest, &LoadOptions::from_process, &mut std::io::stdout(), &mut std::io::stderr())
    }))
    .unwrap_or(1);
    crate::log::flush();
    code
}

/// Runs one command. `sources` gives the configuration sources (the process environment in
/// production); `out` and `err` receive what the command prints.
fn execute(
    args: &[String],
    sources: &dyn Fn() -> LoadOptions,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let command = args.first().map_or("start", String::as_str);
    match command {
        "start" => match load(sources, err) {
            Some(cfg) => crate::app::start(cfg),
            None => 1,
        },
        "migrate" => match load(sources, err) {
            Some(cfg) => crate::app::migrate(cfg),
            None => 1,
        },
        "check-config" => match load(sources, err) {
            Some(cfg) => {
                let _ = writeln!(out, "{}", crate::util::json::to_string_pretty(&cfg.describe()));
                for w in cfg.warnings() {
                    let _ = writeln!(err, "warning: {w}");
                }
                0
            }
            None => 1,
        },
        "gen-secret" => {
            let secret = crate::util::encoding::base64_encode(&crate::util::random::bytes(48));
            let _ = writeln!(out, "{secret}");
            0
        }
        "gen-config-docs" => {
            let mut check = false;
            for option in &args[1..] {
                if option == "--check" {
                    check = true;
                } else {
                    let _ = write!(err, "Unknown option \"{option}\" for gen-config-docs.\n\n{USAGE}");
                    return 2;
                }
            }
            config::gen_config_docs(&sources().cwd, check, out, err)
        }
        "admin" => crate::app::admin(&args[1..]),
        "version" | "--version" | "-V" => {
            let _ = writeln!(out, "scacelith-server {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "help" | "-h" | "--help" => {
            let _ = write!(out, "{USAGE}");
            0
        }
        other => {
            let _ = write!(err, "Unknown command \"{other}\".\n\n{USAGE}");
            2
        }
    }
}

/// The configuration, or `None` after printing every problem to `err`.
fn load(sources: &dyn Fn() -> LoadOptions, err: &mut dyn Write) -> Option<Config> {
    match config::load(&sources()) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            let _ = writeln!(err, "{e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Run {
        code: i32,
        out: String,
        err: String,
    }

    fn run_with(args: &[&str], env: &[(&str, &str)]) -> Run {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let sources = || LoadOptions::from_pairs(env);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = execute(&args, &sources, &mut out, &mut err);
        Run { code, out: String::from_utf8(out).unwrap(), err: String::from_utf8(err).unwrap() }
    }

    fn secret() -> String {
        crate::util::encoding::base64_encode(&[7u8; 48])
    }

    #[test]
    fn version_help_and_usage_errors() {
        let v = run_with(&["version"], &[]);
        assert_eq!(
            (v.code, v.out.as_str()),
            (0, concat!("scacelith-server ", env!("CARGO_PKG_VERSION"), "\n"))
        );
        assert_eq!(run_with(&["--version"], &[]).out, v.out);
        for h in ["help", "-h", "--help"] {
            let r = run_with(&[h], &[]);
            assert_eq!((r.code, r.out.as_str(), r.err.as_str()), (0, USAGE, ""), "{h}");
        }
        let u = run_with(&["nope"], &[]);
        assert_eq!(u.code, 2);
        assert_eq!(u.err, format!("Unknown command \"nope\".\n\n{USAGE}"));
        assert_eq!(run_with(&["gen-config-docs", "--chek"], &[]).code, 2);
    }

    #[test]
    fn gen_secret_prints_48_random_bytes_in_base64() {
        let a = run_with(&["gen-secret"], &[]);
        let b = run_with(&["gen-secret"], &[]);
        assert_eq!(a.code, 0);
        let text = a.out.strip_suffix('\n').unwrap();
        assert_eq!(crate::util::encoding::base64_decode(text).unwrap().len(), 48);
        assert_ne!(a.out, b.out);
    }

    /// Ported from config.load.test.js (check-config prints the Google sign-in origin and warns).
    #[test]
    fn check_config_prints_the_effective_configuration_and_the_warnings() {
        let s = secret();
        let env = [
            ("SCACELITH_ENV_FILE", ""),
            ("SERVER_SECRET", s.as_str()),
            ("TLS_MODE", "off"),
            ("ALLOW_INSECURE_DEV", "1"),
            ("SSO_GOOGLE_ENABLED", "1"),
            ("GOOGLE_CLIENT_ID", "id.apps.googleusercontent.com"),
            ("GOOGLE_CLIENT_SECRET", "GOCSPX-x"),
            ("SERVER_PUBLIC_HOST", "localhost"),
            ("REQUIRE_EMAIL_VERIFICATION", "false"),
            ("GOOGLE_REDIRECT_URI", "https://x/cb"),
        ];
        let r = run_with(&["check-config"], &env);
        assert_eq!(r.code, 0, "{}", r.err);
        let printed: serde_json::Value = serde_json::from_str(&r.out).unwrap();
        assert_eq!(printed["ssoOrigin"], "localhost:443");
        assert_eq!(printed["ssoRedirectTag"], config::sso_origin_tag("localhost:443"));
        assert_eq!(printed["serverSecret"], "<set>");
        assert_eq!(printed["googleClientSecret"], "<set>");
        assert!(r.out.starts_with("{\n  \"serverName\": \"Scacelith Community Server\",\n"));
        for start in [
            "warning: GOOGLE_REDIRECT_URI is no longer used",
            "warning: SSO_GOOGLE_ENABLED with REQUIRE_EMAIL_VERIFICATION=false",
            "warning: SSO_GOOGLE_ENABLED with SERVER_PUBLIC_HOST=localhost",
        ] {
            assert!(r.err.lines().any(|l| l.starts_with(start)), "{start}: {}", r.err);
        }
        assert!(!r.out.contains("GOCSPX") && !r.err.contains("GOCSPX"));
    }

    #[test]
    fn an_invalid_configuration_exits_with_1_and_every_problem() {
        let r = run_with(&["check-config"], &[("SCACELITH_ENV_FILE", ""), ("TLS_MODE", "off")]);
        assert_eq!(r.code, 1);
        assert_eq!(r.out, "");
        assert!(r.err.starts_with("Invalid configuration:\n  - SERVER_SECRET is required"), "{}", r.err);
        assert!(r.err.contains("\n  - TLS_MODE=off is refused unless ALLOW_INSECURE_DEV=1"));
        for command in [&[][..], &["start"], &["migrate"]] {
            let r = run_with(command, &[("SCACELITH_ENV_FILE", "")]);
            assert_eq!(r.code, 1, "{command:?}");
            assert!(r.err.starts_with("Invalid configuration:"), "{command:?}");
        }
    }

    #[test]
    fn gen_config_docs_checks_the_committed_files() {
        let server_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let at = |cwd: &std::path::Path, args: &[&str]| -> Run {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let sources = || LoadOptions { cwd: cwd.to_path_buf(), ..LoadOptions::default() };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = execute(&args, &sources, &mut out, &mut err);
            Run { code, out: String::from_utf8(out).unwrap(), err: String::from_utf8(err).unwrap() }
        };
        let r = at(&server_dir, &["gen-config-docs", "--check"]);
        assert_eq!(
            (r.code, r.err.as_str()),
            (0, ""),
            "run `cargo run -p scacelith-server -- gen-config-docs`"
        );
        let elsewhere = at(&std::env::temp_dir(), &["gen-config-docs", "--check"]);
        assert_eq!(elsewhere.code, 1);
        assert!(elsewhere.err.contains("run it from the dedicated-server directory"));
    }
}
