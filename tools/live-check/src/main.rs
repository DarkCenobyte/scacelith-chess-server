//! Live interoperability check: the game's C++ online client (`net::OnlineClient`) against this
//! server, over real TLS (or plain HTTP on the loopback for the sso part with `--sso-http`).
//!
//! ```text
//! scacelith-live-check [--only=game,account,sso] [--server=PATH] [--sso-http] [COMMAND...]
//! ```
//!
//! `COMMAND` runs the C++ tests (default `build/scacelith_tests`); the name of the C++ test of a
//! part is appended to it. Three parts, one server each, in this order; see the README.
//!
//! - game: [`game`], `net_live_server_game` and `net_live_account_server_settings`;
//! - account: [`account`], `net_live_account_api`;
//! - sso: [`sso`], `net_live_sso`.
//!
//! Exit code: 0 when every part passed, else the first failing part's (the C++ test's exit code,
//! or 1 when the harness failed).

#[path = "../../../crates/server/tests/support/mod.rs"]
mod support;

mod account;
mod control;
mod game;
mod google;
mod sso;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use scacelith_server::security::totp::{base32_decode, hotp};
use serde_json::{Value, json};

use crate::support::TestServer;
use crate::support::server::ServerOptions;

/// The parts, in the order they run.
const PARTS: [&str; 3] = ["game", "account", "sso"];

const USAGE: &str =
    "usage: scacelith-live-check [--only=game,account,sso] [--server=PATH] [--sso-http] [COMMAND...]
  COMMAND       runs the C++ tests (default build/scacelith_tests), e.g. `wine build-win/scacelith_tests.exe`
  --only=LIST   the parts to run (comma-separated; default all of them)
  --server=PATH the scacelith-server binary of the game and account parts (default: next to this program)
  --sso-http    the sso part in plain HTTP on the loopback (a development client) instead of pinned TLS
environment: LIVE_HOST, the host name the C++ client connects to (default localhost)";

/// What every part needs.
#[derive(Debug)]
pub(crate) struct Ctx {
    /// The host name the C++ client connects to (`LIVE_HOST`).
    pub host: String,
    /// The command that runs the C++ tests.
    pub command: Vec<String>,
    /// The server binary of the game and account parts, when not the default one.
    pub server_bin: Option<PathBuf>,
    /// The sso part in plain HTTP.
    pub sso_http: bool,
}

impl Ctx {
    /// The options of a server of the game or account part.
    pub fn server(&self) -> ServerOptions {
        let options = TestServer::options();
        match &self.server_bin {
            Some(bin) => options.bin(bin.clone()),
            None => options,
        }
    }
}

/// Runs the C++ tests whose names contain `filter` with `env`, the output going to ours; returns
/// the exit code (128 when a signal ended it, 127 when it could not run).
pub(crate) async fn run_cpp(ctx: &Ctx, filter: &str, env: &[(&str, &str)]) -> i32 {
    let mut command = tokio::process::Command::new(&ctx.command[0]);
    command
        .args(&ctx.command[1..])
        .arg(filter)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    match command.status().await {
        Ok(status) => status.code().unwrap_or(128),
        Err(e) => {
            eprintln!("cannot run {}: {e}", ctx.command.join(" "));
            127
        }
    }
}

/// The wall clock in milliseconds.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A TOTP code of `secret` (base32) the server has not seen used: the current step's, or the next
/// step's when the current one was handed out already (the server takes one step ahead, and
/// refuses a step at or before the last one used). `steps`: the last step handed out per secret.
/// Answers `{code, step}`.
pub(crate) fn fresh_totp(steps: &mut HashMap<String, i64>, secret: &str) -> Value {
    let mut step = now_ms() / 30_000;
    let last = steps.get(secret).copied().unwrap_or(-1);
    if step <= last {
        step = last + 1;
    }
    steps.insert(secret.to_owned(), step);
    let code = base32_decode(secret).map(|key| hotp(&key, step.unsigned_abs(), 6)).unwrap_or_default();
    json!({ "code": code, "step": step })
}

/// The command line: the context and the parts to run.
fn parse_args(mut args: Vec<String>) -> Result<(Ctx, Vec<String>), String> {
    let mut parts: Vec<String> = PARTS.iter().map(|p| (*p).to_owned()).collect();
    let mut server_bin = None;
    let mut sso_http = false;
    while args.first().is_some_and(|a| a.starts_with("--")) {
        let arg = args.remove(0);
        if let Some(list) = arg.strip_prefix("--only=") {
            parts = list.split(',').filter(|p| !p.is_empty()).map(str::to_owned).collect();
        } else if let Some(path) = arg.strip_prefix("--server=") {
            server_bin = Some(PathBuf::from(path));
        } else if arg == "--sso-http" {
            sso_http = true;
        } else if arg == "--help" {
            return Err(String::new());
        } else {
            return Err(format!("unknown option {arg}"));
        }
    }
    if let Some(unknown) = parts.iter().find(|p| !PARTS.contains(&p.as_str())) {
        return Err(format!("unknown part {unknown}"));
    }
    let command = if args.is_empty() { vec!["build/scacelith_tests".to_owned()] } else { args };
    let host =
        std::env::var("LIVE_HOST").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "localhost".to_owned());
    Ok((Ctx { host, command, server_bin, sso_http }, parts))
}

/// Runs one part as a task of its own: a panic of the harness is that part's failure (1).
async fn run_part(ctx: Rc<Ctx>, part: String) -> i32 {
    let task = tokio::task::spawn_local(async move {
        match part.as_str() {
            "game" => game::run(&ctx).await,
            "account" => account::run(&ctx).await,
            _ => sso::run(&ctx).await,
        }
    });
    task.await.unwrap_or_else(|e| {
        eprintln!("the harness failed: {e}");
        1
    })
}

fn main() {
    let (ctx, parts) = match parse_args(std::env::args().skip(1).collect()) {
        Ok(parsed) => parsed,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a Tokio runtime");
    // The parts run on this thread (the sso part holds the log capture, which stays on it); the
    // servers' own tasks on the runtime's workers.
    let local = tokio::task::LocalSet::new();
    let ctx = Rc::new(ctx);
    let code = local.block_on(&runtime, async move {
        let mut first_failure = 0;
        for part in parts {
            let code = run_part(ctx.clone(), part.clone()).await;
            if code == 0 {
                println!("== {part}: passed");
            } else {
                println!("== {part}: FAILED (exit code {code})");
                if first_failure == 0 {
                    first_failure = code;
                }
            }
        }
        first_failure
    });
    std::process::exit(code);
}
