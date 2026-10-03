//! Realtime connections: the per-connection task (Hello, rate buckets, heartbeats, dispatch),
//! outbound queues with slow-consumer handling, the lobby actor (presence, matchmaking,
//! challenges, conduct, rematches, sanctions and notices) and the drain at shutdown. See
//! docs/RUST-PORT.md.
//!
//! `app::start` wires it: [`Lobby::channel`] first (the game hosts announce the games they
//! recover into its inbox), the lobby actor once the hosts run, then [`Realtime`], whose
//! [`Realtime::on_connection`] is the WebSocket endpoint's hook and whose [`Realtime::drain`]
//! closes every connection at shutdown. [`Admissions`] is the endpoint's admission (per-address
//! and global connection counts) and the server-full signal of the listeners.

pub mod admission;
pub(crate) mod conn;
pub mod deps;
pub mod drain;
#[cfg(test)]
mod e2e;
pub mod endpoint;
pub(crate) mod frames;
pub(crate) mod link;
pub mod lobby;
pub(crate) mod metrics;
pub(crate) mod presence;
pub(crate) mod reads;
#[cfg(test)]
pub(crate) mod testing;

use std::sync::Arc;
use std::time::Duration;

pub use admission::Admissions;
pub use deps::{GameHosts, NoTokens, Session, TokenValidator};
pub use drain::{Drain, DrainPhase};
pub use endpoint::{Endpoint, Outbound};
pub use lobby::{Lobby, LobbyInbox};

use self::conn::{ConnContext, ConnSettings};
use crate::clock::SharedClock;
use crate::config::Config;
use crate::events::AnomalySink;
use crate::log::Logger;
use crate::matching::elo::Categories;
use crate::net::upgrade::OnConnection;
use crate::net::ws::WsConnection;
use crate::store::Store;

/// What the connection tasks need.
pub struct RealtimeDeps {
    pub config: Arc<Config>,
    pub clock: SharedClock,
    /// Ratings, bans and conduct read before lobby requests and at the Hello.
    pub store: Store,
    pub hosts: Arc<dyn GameHosts>,
    pub tokens: Arc<dyn TokenValidator>,
    /// Protocol anomalies and certain cheats (forged message types).
    pub anomalies: Arc<dyn AnomalySink>,
    pub lobby: Lobby,
}

/// The realtime connections of a server: spawns a task per connection and drains them at
/// shutdown.
pub struct Realtime {
    ctx: Arc<ConnContext>,
}

impl std::fmt::Debug for Realtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Realtime").field("connections", &self.connections()).finish()
    }
}

impl Realtime {
    pub fn new(deps: RealtimeDeps) -> Realtime {
        let ctx = ConnContext {
            settings: ConnSettings::from_config(&deps.config),
            categories: Categories::from_config(&deps.config),
            config: deps.config,
            clock: deps.clock,
            store: deps.store,
            hosts: deps.hosts,
            tokens: deps.tokens,
            anomalies: deps.anomalies,
            lobby: deps.lobby,
            drain: Drain::new(),
            log: Logger::root().child("ws"),
        };
        Realtime { ctx: Arc::new(ctx) }
    }

    /// The WebSocket endpoint's hook: serves each new connection in a task of its own.
    pub fn on_connection(&self) -> OnConnection {
        let ctx = self.ctx.clone();
        Arc::new(move |conn: WsConnection| {
            tokio::spawn(conn::serve(conn, ctx.clone()));
        })
    }

    /// Connections open (the Hello included).
    pub fn connections(&self) -> usize {
        self.ctx.drain.live()
    }

    /// Where the drain is.
    pub fn drain_phase(&self) -> DrainPhase {
        self.ctx.drain.phase()
    }

    /// Drains the connections: every player gets `Notice{ServerShutdown, arg = grace}` and new
    /// connections are refused; after `grace` (sooner once every connection has ended) every
    /// connection gets a fatal `Error{ShuttingDown}` and closes with 4008. Returns whether they
    /// all ended (their games detached, their presence released) within twice the close timeout
    /// (the last frames, then the closing handshake) and a second more.
    pub async fn drain(&self, grace: Duration) -> bool {
        let drain = &self.ctx.drain;
        let grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
        drain.set(DrainPhase::Grace { grace_ms });
        drain.wait_idle(grace).await;
        drain.set(DrainPhase::Closing);
        drain.wait_idle(2 * self.ctx.settings.close_timeout + Duration::from_secs(1)).await
    }
}
