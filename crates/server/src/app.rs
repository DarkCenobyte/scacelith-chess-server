//! Server bootstrap and lifecycle: builds every service from the configuration, applies the
//! migrations, recovers the journaled games, starts the host actors, the lobby, the listeners and
//! the background jobs, notifies systemd, and runs the graceful shutdown on SIGTERM/SIGINT
//! (SIGHUP reloads the certificates). See docs/RUST-PORT.md.
//!
//! The command line ([`crate::cli`]) loads and checks the configuration, then calls these entry
//! points; each returns the process exit code (0 success, 1 failure, 2 usage). Until the services
//! are assembled (wave 2), they report that the command is not available yet.

use crate::config::Config;

/// `scacelith-server start`: runs the server until it is stopped.
pub fn start(config: Config) -> i32 {
    not_available("start", &config)
}

/// `scacelith-server migrate`: applies the database migrations, prints
/// `{"ok": true, "applied": ..., "serverId": "..."}` and exits.
pub fn migrate(config: Config) -> i32 {
    not_available("migrate", &config)
}

/// `scacelith-server admin ...`: the administration commands (`args` follow `admin`). Loads the
/// configuration itself, after the help, as the former `scacelith-admin` did.
pub fn admin(args: &[String]) -> i32 {
    crate::anticheat::admin::main(args)
}

fn not_available(command: &str, config: &Config) -> i32 {
    let _ = config;
    eprintln!("scacelith-server: {command} is not available yet in this build.");
    1
}
