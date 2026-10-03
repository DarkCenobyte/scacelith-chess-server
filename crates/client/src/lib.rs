//! Rust client SDK of the Scacelith server: the HTTPS account API and the realtime protocol v1
//! over a TLS WebSocket. It is the reference client for tools, the server's integration tests
//! and the `scacelith-bench` load generator. GPL-3.0-or-later.
//!
//! # Layers
//!
//! * [`Endpoint`]: a server address, plain TCP or TLS ([`TlsConfig`]: public roots, a pinned
//!   certificate, or no check for tests).
//! * [`ApiClient`]: `GET /info`, registration, sign-in (with a second factor), sign-out, and any
//!   other JSON endpoint, over keep-alive HTTP/1.1 ([`http`]).
//! * [`Connection`]: the realtime protocol v1 (docs/PROTOCOL.md): Hello/Welcome, typed
//!   [`ClientMsg`](scacelith_protocol::ClientMsg) out with automatic `seq` numbering, typed
//!   [`ServerMsg`](scacelith_protocol::ServerMsg) in, automatic answers to the server's `Ping`,
//!   the close code once the connection ends ([`CloseInfo`]).
//! * [`ws::Session`]: the WebSocket client under it (RFC 6455: masking, fragmentation, ping,
//!   pong, close), usable for raw frames.
//! * [`bot`]: players that find games and play random legal moves with gestures.
//!
//! # Example
//!
//! ```no_run
//! # async fn demo() -> scacelith_client::Result<()> {
//! use scacelith_client::bot::{Bot, BotConfig};
//! use scacelith_client::{ApiClient, ConnectOptions, Connection, Endpoint, TlsConfig};
//!
//! let endpoint = Endpoint::tls("127.0.0.1:8443".parse().unwrap(), "localhost",
//!                              TlsConfig::with_root_file("cert.pem")?);
//! let api = ApiClient::new(endpoint.clone());
//! let session = api.register_and_login("alice", "alice@example.org", "correct horse battery").await?;
//! let conn = Connection::connect(&endpoint, &session.token, &ConnectOptions::default()).await?;
//! let mut bot = Bot::new(conn, BotConfig::default());
//! let game = bot.join_queue("3+2", false).await?;
//! let result = bot.play(game).await?;
//! println!("{:?} after {} plies", result.end.reason, result.plies);
//! # Ok(()) }
//! ```

pub mod api;
pub mod bot;
mod error;
pub mod http;
mod net;
mod realtime;
mod tls;
pub mod ws;

#[cfg(test)]
mod tests;

pub use api::{ApiClient, AuthSession, Login};
pub use error::{ApiError, ClientError, CloseInfo, Closer, Result};
pub use net::{ConnectTimings, DEFAULT_CONNECT_TIMEOUT, Endpoint, Stream};
pub use realtime::{ConnectOptions, Connection, pong_reply};
pub use tls::TlsConfig;
