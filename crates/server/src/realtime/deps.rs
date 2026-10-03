//! What the realtime layer needs from the parts `app::start` wires to it: the game hosts
//! (docs/RUST-PORT.md 8.1) and the session tokens (8.3). Each is a trait object, so that the
//! connection tasks and the lobby never name the concrete types and the tests use doubles.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use scacelith_protocol::{ClientMsg, ErrorCode};

use super::endpoint::Endpoint;
use crate::events::NewGame;
use crate::ids::{ConnId, GameId, UserId};

/// A boxed future, for the asynchronous methods of the traits below.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The game host actors, routed by the shard of the game id (`ids::shard_of`). Every method but
/// [`GameHosts::create`] and [`GameHosts::ping`] posts to an actor and returns at once.
pub trait GameHosts: Send + Sync + 'static {
    /// A strictly decoded game request (Move, Resign, DrawOffer, DrawAnswer, DrawClaim, Abort,
    /// Resync, Rematch) for `game`, with the connection it came from and its read time (mono ms).
    /// Returns false when no host of this server hosts that shard.
    fn client(&self, game: GameId, user: UserId, msg: ClientMsg, ep: Endpoint, recv_at: f64) -> bool;

    /// A raw `C_Gesture` frame, already decoded and checked by the connection. Returns false when
    /// no host of this server hosts that shard.
    fn gesture(&self, game: GameId, user: UserId, frame: Bytes) -> bool;

    /// Binds a connection to a game; the host sends a `GameSnapshot`.
    fn attach(&self, game: GameId, user: UserId, ep: Endpoint);

    /// Unbinds a connection from a game, if it is still that connection.
    fn detach(&self, game: GameId, user: UserId, conn: ConnId);

    /// A new round-trip estimate of a player of the game.
    fn rtt(&self, game: GameId, user: UserId, rtt_ms: u32);

    /// Ends the user's running game on the host of `game` as a forfeit (ban, certain cheat).
    fn forfeit_user(&self, game: GameId, user: UserId);

    /// Closes the rematch window of a finished game for the user.
    fn decline_rematch(&self, game: GameId, user: UserId);

    /// Creates a game on the host of shard `preferred` when given and available, else on the
    /// least loaded one; resolves to the new game id.
    fn create(&self, preferred: Option<u32>, game: NewGame) -> BoxFuture<Result<GameId, ErrorCode>>;

    /// Cancels a game nobody will join (created after its creator gave up waiting).
    fn cancel(&self, game: GameId);

    /// Whether a host stalled since `since_mono_ms` (a round trip measured across a stall is not
    /// a network sample).
    fn stall_during(&self, since_mono_ms: f64) -> bool;

    /// Resolves to true once every host actor has answered (systemd watchdog).
    fn ping(&self) -> BoxFuture<bool>;
}

/// An authenticated session (`auth::SessionInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub user_id: UserId,
    pub username: String,
    pub session_id: i64,
    pub email_verified: bool,
    /// SHA-256 of the session token (the key of revocations).
    pub token_hash: [u8; 32],
}

/// A failure to check a token (store failure...): the Hello ends with `Internal`.
pub type ValidateError = Box<dyn Error + Send + Sync>;

/// Checks session tokens (auth).
pub trait TokenValidator: Send + Sync + 'static {
    /// The session of `token`, or `None` when the token is unknown, expired or revoked.
    fn validate(&self, token: String) -> BoxFuture<Result<Option<Session>, ValidateError>>;
}
