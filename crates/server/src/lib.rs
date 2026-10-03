//! Scacelith dedicated server.
//!
//! One process serves the HTTPS account API and the realtime WebSocket protocol on the same port,
//! runs the games (one host actor per shard), stores everything in SQLite and analyses rated games
//! with Stockfish for the anti-cheat. See `docs/DESIGN.md` for the behaviour and
//! `docs/RUST-PORT.md` for the code layout.
//!
//! Copyright (C) 2026 the Scacelith authors. Licensed under the GNU General Public License,
//! version 3 or later.

pub mod clock;
pub mod config;
pub mod events;
pub mod ids;
pub mod log;
pub mod metrics;
pub mod sys;
pub mod systemd;
pub mod util;

pub mod journal;
pub mod store;

pub mod mail;
pub mod security;

pub mod auth;
pub mod http;
pub mod net;

pub mod anticheat;
pub mod game;
pub mod gifsvc;
pub mod matching;
pub mod realtime;

pub mod app;
pub mod cli;
