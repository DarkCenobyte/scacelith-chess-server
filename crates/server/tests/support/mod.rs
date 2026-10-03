//! Shared support of the black-box integration tests: the real server binary in a child process
//! ([`server`]), players over the client SDK ([`mod@players`]) and plain HTTP requests ([`web`]).
//! Each test file uses part of it.
#![allow(dead_code, unused_imports, unused_macros)]

pub mod players;
pub mod server;
pub mod web;

pub use players::{
    Account, Client, PASSWORD, Player, Table, WAIT, account, accounts, challenge_game, close_all, connect,
    join_all, mv, player, players, queue_game, sign_in,
};
pub use server::{TempDir, TestServer, eventually, metric_value};
