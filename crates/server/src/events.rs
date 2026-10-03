//! Events that cross module boundaries, and the traits through which one module notifies another
//! without depending on its implementation.
//!
//! | Trait | Called by | Implemented by |
//! |---|---|---|
//! | [`HostEvents`] | game host actors | the lobby (realtime) |
//! | [`AnomalySink`] | game host actors, connection tasks | the anti-cheat service |
//! | [`SessionEvents`] | auth (sessions revoked) | realtime (closes the connections) |
//! | [`SanctionEvents`] | anti-cheat (ban pending, ban applied, refunds) | the lobby (realtime) |
//!
//! Every method is synchronous and must not block: implementations post a message to their actor
//! or enqueue a store job and return. [`Noop`] implements every trait for tests.

use serde_json::Value;
use tokio::sync::oneshot;

pub use scacelith_protocol::{EndReason, ErrorCode, GameStatus, PlayerInfo};

pub use crate::matching::conduct::IncidentKind;

use crate::ids::{ConnId, GameId, UserId};

/// A game to create on a host actor.
#[derive(Clone, Debug, PartialEq)]
pub struct NewGame {
    /// Official category id (`"3+2"`) or `"custom"`.
    pub category: String,
    pub base_ms: u32,
    pub inc_ms: u32,
    /// Rated (only for an official category).
    pub rated: bool,
    pub white: PlayerInfo,
    pub black: PlayerInfo,
    /// Wall-clock creation time (ms).
    pub created_at: i64,
    /// The finished game this one is a rematch of.
    pub rematch_of: Option<GameId>,
    /// The robots press the clock by themselves (fixed at creation, kept by rematches).
    pub auto_press: bool,
}

/// A game whose result is committed to the database (DESIGN 3 "End of game"). Players stay busy
/// (AlreadyInGame) until this event, not until GameEnd.
#[derive(Clone, Debug, PartialEq)]
pub struct GameEnded {
    pub game: GameId,
    pub white: UserId,
    pub black: UserId,
    pub status: GameStatus,
    pub reason: EndReason,
    pub rated: bool,
    pub category: String,
}

/// Both players of a finished game asked for a rematch. `game.white`/`game.black` are already
/// swapped and `game.rematch_of` is the finished game.
#[derive(Clone, Debug, PartialEq)]
pub struct RematchRequest {
    pub game: NewGame,
}

/// A suspicious or certain protocol or game event of one player (DESIGN 6.6). Stored in the
/// anomalies table; feeds the analysis queue and, for certain cheats, the automatic sanction.
#[derive(Clone, Debug, PartialEq)]
pub struct Anomaly {
    pub user: UserId,
    /// 0 when no game is concerned.
    pub game: GameId,
    /// Stored kind (`malformed`, `flood`, `bad_seq`, `forged_type`, `clock_implausible`,
    /// `foreign_game`, `desync`, `repeated_desync`, ...).
    pub kind: &'static str,
    /// Stored detail object (exact key order matters for the stored text).
    pub detail: Value,
    /// The client's position hash matched the server's.
    pub pos_matched: bool,
}

/// A ban for a certain cheat that the anti-cheat is writing ([`SanctionEvents::sanction_pending`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SanctionPending {
    pub user: UserId,
    /// Wall-clock end of the ban being written (ms).
    pub until: i64,
    /// The connection the cheat came from, which its owner closes with `CheatDetected` (0: none).
    pub conn: ConnId,
}

/// A ban applied by the anti-cheat or an administrator.
#[derive(Clone, Debug, PartialEq)]
pub struct SanctionApplied {
    pub user: UserId,
    /// Wall-clock end of the ban (ms).
    pub until: i64,
    pub reason: String,
    /// Rating refunds granted to the cheater's victims.
    pub refunds: u32,
}

/// What a host actor tells the lobby.
pub trait HostEvents: Send + Sync + 'static {
    /// A game's result is committed (after the database transaction).
    fn game_ended(&self, ended: GameEnded);

    /// A game was recovered from the journal at start.
    fn game_recovered(&self, game: GameId, white: UserId, black: UserId);

    /// Both players want a rematch: the lobby checks them, creates the game on a host and answers
    /// with the new game id or the error to report (`RematchUnavailable`...).
    fn rematch(&self, request: RematchRequest, reply: oneshot::Sender<Result<GameId, ErrorCode>>);

    /// A conduct incident (abandon, abort, no-show) for the cooldowns.
    fn conduct(&self, user: UserId, kind: IncidentKind);
}

/// Where anomalies go (the anti-cheat service).
pub trait AnomalySink: Send + Sync + 'static {
    /// Records an anomaly. The first anomaly of a (user, game, kind) in each second is enqueued on
    /// the store writer before this returns, so it commits before any later write of the caller
    /// (a game batch that queues the analysis); repeats within that second are merged into the
    /// same row and written up to one second later.
    fn record(&self, anomaly: Anomaly);

    /// A certain cheat (forged message type, ...) when `AUTO_SANCTION_CERTAIN_CHEATS` is on:
    /// sanctions the user. `conn` is the connection the cheat came from (0: none): the caller
    /// closes it with `CheatDetected` and forfeits the user's games itself.
    ///
    /// The ban holds from the moment this returns ([`SanctionEvents::sanction_pending`] is
    /// called before), so the caller calls it before it queues anything that tells the client
    /// (the forfeit, the `Error`, the close): a client that reconnects then is refused.
    fn sanction_certain(&self, user: UserId, game: GameId, kind: &'static str, conn: ConnId);
}

/// Session revocations (password change, sign-out everywhere, deletion, ban).
pub trait SessionEvents: Send + Sync + 'static {
    /// `token_hashes`: SHA-256 of the revoked tokens, or `None` for every session of the user.
    /// Connections authenticated with a revoked token get `Notice{SessionRevoked}` then
    /// `Error{Unauthorized}` and are closed.
    fn sessions_revoked(&self, user: UserId, token_hashes: Option<Vec<[u8; 32]>>);
}

/// Sanctions and refunds, for the lobby (kick, forfeit, refund notices).
pub trait SanctionEvents: Send + Sync + 'static {
    /// A ban for a certain cheat is being written: the lobby enforces it at once, as a hold that
    /// [`SanctionEvents::sanction_applied`] replaces with the stored ban, or that ends by itself
    /// a minute later when no new ban comes (the write failed, or a ban stood already).
    fn sanction_pending(&self, pending: SanctionPending);

    fn sanction_applied(&self, sanction: SanctionApplied);

    /// New rating refunds may be waiting to be announced (`Notice{RatingRestored}`).
    fn refunds_pending(&self);
}

/// Does nothing; for tests and for components started before their peers.
#[derive(Clone, Copy, Debug, Default)]
pub struct Noop;

impl HostEvents for Noop {
    fn game_ended(&self, _: GameEnded) {}
    fn game_recovered(&self, _: GameId, _: UserId, _: UserId) {}
    fn rematch(&self, _: RematchRequest, reply: oneshot::Sender<Result<GameId, ErrorCode>>) {
        let _ = reply.send(Err(ErrorCode::RematchUnavailable));
    }
    fn conduct(&self, _: UserId, _: IncidentKind) {}
}

impl AnomalySink for Noop {
    fn record(&self, _: Anomaly) {}
    fn sanction_certain(&self, _: UserId, _: GameId, _: &'static str, _: ConnId) {}
}

impl SessionEvents for Noop {
    fn sessions_revoked(&self, _: UserId, _: Option<Vec<[u8; 32]>>) {}
}

impl SanctionEvents for Noop {
    fn sanction_pending(&self, _: SanctionPending) {}
    fn sanction_applied(&self, _: SanctionApplied) {}
    fn refunds_pending(&self) {}
}
