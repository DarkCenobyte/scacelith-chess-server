//! Server bootstrap and lifecycle: builds every service from the configuration, applies the
//! migrations, recovers the journaled games, starts the host actors, the lobby, the listeners and
//! the background jobs, notifies systemd, and runs the graceful shutdown on SIGTERM/SIGINT
//! (SIGHUP reloads the certificates). Owner: realtime (wave 2). See docs/RUST-PORT.md.
