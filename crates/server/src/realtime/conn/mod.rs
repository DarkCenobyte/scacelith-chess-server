//! The connection tasks (the former router): one task per WebSocket connection runs the Hello
//! state machine, then the session (rate buckets, sequence numbers, pings, dispatch to the game
//! hosts and the lobby); a second task per authenticated connection writes its outbound queue to
//! the socket.
//!
//! * **Hello** ([`hello`]): the first message must be a Hello, checked in the order of
//!   PROTOCOL.md "Connection lifecycle" (17-byte prefix, `proto`, strict decoding, `seq` 1), within
//!   `WS_HELLO_TIMEOUT_MS`. The token is validated, the e-mail verification enforced, the stored
//!   ban read, then the lobby's presence claim decides (banned, server full, admitted with the
//!   game in progress); the token is validated again (a revocation during the claim found no
//!   connection to close), then `Welcome` (negotiated `minor` and `caps`) is written and the game
//!   in progress attached. Up to 8 messages received meanwhile are handled after the `Welcome`.
//! * **Session** ([`session`]): per message, a token bucket (`WS_MSG_RATE`/`WS_MSG_BURST`; over it
//!   the message is dropped with `Error{RateLimited}` at most once a second, more than
//!   max(10, burst) drops in 10 s is a flood), a server type byte is a forgery (certain cheat),
//!   strict decoding (malformed), `seq` = last + 1 (else dropped, anomaly once, and a gap
//!   resynchronises). Gestures have a bucket of their own and are dropped silently. Client pings
//!   are answered once per 950 ms; the heartbeat pings every `HEARTBEAT_INTERVAL_MS` (the first
//!   half an interval after the connection opened), measures the round trip (an average capped
//!   at 2 s, a sample across a host stall left out) and closes a connection silent for
//!   `HEARTBEAT_TIMEOUT_MS` with 1001. Lobby requests go to the lobby actor (answered there with
//!   one `Ack` or `Error`) after the store reads they need, in order; game messages go to the
//!   host of the game's shard.
//! * **Writer** ([`writer`]): drains the [`Outbound`](super::Outbound) queue in batches. Every
//!   frame the host actors and the lobby send goes through it; over `WS_SEND_BUFFER_LIMIT` bytes
//!   waiting, the connection closes with 4303 and no `Error`.
//!
//! Every fatal `Error` is the connection's last message and is followed by its close code
//! (`close_code_for`). Timers run on tokio's clock, timestamps (buckets, round trips,
//! `serverTime`) on the shared monotonic clock.

pub(crate) mod hello;
pub(crate) mod session;
pub(crate) mod writer;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::deps::{GameHosts, TokenValidator};
use super::drain::{Drain, DrainPhase};
use super::lobby::{Lobby, LobbyMsg};
use super::metrics;
use crate::clock::SharedClock;
use crate::config::Config;
use crate::events::AnomalySink;
use crate::ids::{ConnId, UserId};
use crate::log::Logger;
use crate::matching::elo::Categories;
use crate::net::ws::{CLOSE_TIMEOUT, WsConnection, WsEvent, WsReader};
use crate::store::Store;
use scacelith_protocol::ErrorCode;

/// Messages kept between the Hello and the `Welcome`; one more is a flood.
pub(crate) const MAX_PENDING_HELLO: usize = 8;
/// Games a connection is attached to at most (the oldest is detached).
pub(crate) const MAX_GAMES_PER_CONN: usize = 8;
/// Cap of a round-trip sample and of the average (ms).
pub(crate) const RTT_CAP_MS: f64 = 2000.0;
/// Least gap between two answered client pings (ms).
pub(crate) const CLIENT_PING_GAP_MS: f64 = 950.0;
/// Gesture drops in 10 s past which a connection is a flood: at least this.
const GESTURE_FLOOD_MIN: f64 = 50.0;
/// The former heartbeat sweep's tick, kept in the gesture flood threshold (PROTOCOL.md).
const SWEEP_TICK_MS: f64 = 250.0;

/// The connection settings, from the configuration.
#[derive(Clone, Debug)]
pub(crate) struct ConnSettings {
    pub msg_rate: f64,
    pub msg_burst: f64,
    /// Drops in 10 s past which rate-limited messages are a flood.
    pub flood_drops: u32,
    pub gesture_rate: f64,
    pub gesture_burst: f64,
    /// Gesture drops in 10 s past which gestures are a flood.
    pub gesture_flood_drops: u32,
    pub hello_timeout_ms: f64,
    pub heartbeat_interval_ms: f64,
    pub heartbeat_timeout_ms: f64,
    pub client_ping_ms: u32,
    pub server_name: String,
    pub send_buffer_limit: usize,
    pub auto_sanction: bool,
    pub require_email: bool,
    /// How long a closing connection may take to write its last frames.
    pub close_timeout: Duration,
}

impl ConnSettings {
    pub(crate) fn from_config(config: &Config) -> ConnSettings {
        let f = |v: i64| v.max(0) as f64;
        let gesture_rate = f(config.gesture_rate);
        let gesture_burst = f(config.gesture_burst).max(1.0);
        let silence_ms = f(config.heartbeat_timeout_ms) + f(config.heartbeat_interval_ms) + SWEEP_TICK_MS;
        let gesture_flood =
            GESTURE_FLOOD_MIN.max(10.0 * gesture_burst).max((gesture_rate * silence_ms / 1000.0).ceil());
        ConnSettings {
            msg_rate: f(config.ws_msg_rate),
            msg_burst: f(config.ws_msg_burst),
            flood_drops: u32::try_from(config.ws_msg_burst.max(10)).unwrap_or(u32::MAX),
            gesture_rate,
            gesture_burst,
            gesture_flood_drops: gesture_flood.min(f64::from(u32::MAX)) as u32,
            hello_timeout_ms: f(config.ws_hello_timeout_ms),
            heartbeat_interval_ms: f(config.heartbeat_interval_ms),
            heartbeat_timeout_ms: f(config.heartbeat_timeout_ms),
            client_ping_ms: u32::try_from(config.client_ping_interval_ms).unwrap_or(u32::MAX),
            server_name: config.server_name.clone(),
            send_buffer_limit: usize::try_from(config.ws_send_buffer_limit).unwrap_or(usize::MAX),
            auto_sanction: config.auto_sanction_certain_cheats,
            require_email: config.require_email_verification,
            close_timeout: CLOSE_TIMEOUT,
        }
    }
}

/// What every connection task shares.
pub(crate) struct ConnContext {
    pub settings: ConnSettings,
    pub config: Arc<Config>,
    pub categories: Categories,
    pub clock: SharedClock,
    pub store: Store,
    pub hosts: Arc<dyn GameHosts>,
    pub tokens: Arc<dyn TokenValidator>,
    pub anomalies: Arc<dyn AnomalySink>,
    pub lobby: Lobby,
    pub drain: Drain,
    pub log: Logger,
}

impl ConnContext {
    /// The tokio instant of a monotonic time in ms (now when it is past).
    pub(crate) fn instant_at(&self, mono_ms: f64) -> Instant {
        let wait = (mono_ms - self.clock.mono_ms()).max(0.0);
        Instant::now() + Duration::from_secs_f64(wait / 1000.0)
    }
}

impl std::fmt::Debug for ConnContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnContext").field("settings", &self.settings).finish_non_exhaustive()
    }
}

/// Serves one WebSocket connection until it ends.
pub(crate) async fn serve(conn: WsConnection, ctx: Arc<ConnContext>) {
    let WsConnection { mut reader, mut writer, info } = conn;
    let mut drain = ctx.drain.enter();
    let _open = Gauge::up(&metrics::conn().connections);
    if drain.current() != DrainPhase::Running {
        let refusal = hello::Refusal::new("shutting_down", 0, ErrorCode::ShuttingDown);
        hello::refuse(&ctx, &mut writer, refusal).await;
        linger(&mut reader).await;
        return;
    }
    match hello::run(&ctx, &mut reader, &mut writer, &info, &mut drain).await {
        Some(welcomed) => session::run(ctx, reader, writer, info, drain, welcomed).await,
        None => linger(&mut reader).await,
    }
}

/// Reads until the connection ended (its close handshake, or the socket dropped at most 2 s
/// after the close began), dropping what arrives.
pub(crate) async fn linger(reader: &mut WsReader) {
    while let WsEvent::Message(_) = reader.next().await {}
}

/// A connection's claim of presence, taken before the claim is posted: released when dropped (the
/// lobby removes it only while it is still the account's live connection, so releasing a claim it
/// refused or replaced changes nothing).
#[derive(Debug)]
pub(crate) struct ClaimGuard {
    lobby: Lobby,
    user: UserId,
    conn: ConnId,
}

impl ClaimGuard {
    pub(crate) fn new(lobby: Lobby, user: UserId, conn: ConnId) -> ClaimGuard {
        ClaimGuard { lobby, user, conn }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        self.lobby.post(LobbyMsg::Release { user: self.user, conn: self.conn });
    }
}

/// One more on a gauge while alive.
pub(crate) struct Gauge(&'static crate::metrics::Gauge);

impl Gauge {
    pub(crate) fn up(gauge: &'static crate::metrics::Gauge) -> Gauge {
        gauge.inc();
        Gauge(gauge)
    }
}

impl Drop for Gauge {
    fn drop(&mut self) {
        self.0.dec();
    }
}
