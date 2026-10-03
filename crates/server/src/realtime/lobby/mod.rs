//! The lobby actor (the former control plane and presence of the Node primary): one task owning
//! presence, the matchmaking queues, challenges and private codes, conduct cooldowns, game
//! creation and placement, rematches, the ban cache and the rating refund notices.
//!
//! A ban counts from the moment the anti-cheat decides it: a sanction being written
//! ([`SanctionEvents::sanction_pending`]) holds its player out as a ban does (claims, requests
//! and game creations refused with the ban's answers, the game forfeited, the other connections
//! kicked) until the stored ban replaces the hold ([`SanctionEvents::sanction_applied`]), or for
//! a minute at most. A client that reconnects right after the sanction's close finds the hold:
//! the hold is posted (its send done) before the host or connection task that detected the cheat
//! queues the forfeit, the `Error{CheatDetected}` and the 4302 close, which the writer task takes
//! after them (the outbound queue's lock); the client's next connection posts its claim after it
//! read them, so after the hold's send, and the inbox, one FIFO queue for all its senders, hands
//! the hold over first. The connection the cheat came from is spared by the hold: the lobby may
//! handle the hold before that connection's owner closes it, with `CheatDetected`.
//!
//! The connection tasks post their claims, releases and lobby requests; the game hosts, auth and
//! the anti-cheat post events through [`Lobby`], which implements [`HostEvents`],
//! [`SessionEvents`] and [`SanctionEvents`]. The actor answers each lobby request with exactly one
//! `Ack` or `Error`, written straight to the connection's outbound queue, and sends the other
//! notifications (`QueueStatus`, `ChallengeStatus`, `ChallengeReceived`, `Notice`) the same way.
//!
//! The actor never waits on the database while it holds a decision: the connection tasks read
//! what a request needs (ratings, the stored ban, the conduct cooldown) before posting it, and
//! the work that needs the store or a host afterwards (game creation, rematch checks, conduct
//! records, the refund notices of [`crate::anticheat::notices`]) runs in a spawned task whose
//! result comes back as a message. The one
//! exception is the target's challenge preference, read inline for a direct challenge to an online
//! player (one indexed read, a rare request).
//!
//! The inbox is unbounded; what can be in it is bounded by its sources: one claim and one release
//! per connection, [`MAX_LOBBY_IN_FLIGHT`](super::link::MAX_LOBBY_IN_FLIGHT) requests per
//! connection, one creation, rematch or conduct result per game, and the rare auth and anti-cheat
//! events.

mod actor;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use scacelith_protocol::ErrorCode;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use self::actor::LobbyActor;
pub(crate) use self::actor::LobbyDeps;
#[cfg(test)]
pub(crate) use self::actor::SANCTION_HOLD_MS;
use super::link::ConnLink;
use crate::anticheat::notices::RefundEvent;
use crate::events::{
    GameEnded, HostEvents, IncidentKind, RematchRequest, SanctionApplied, SanctionEvents, SanctionPending,
    SessionEvents,
};
use crate::ids::{ConnId, GameId, UserId};
use crate::matching::ColorPref;
use crate::matching::conduct::Cooldown;

/// What a claim of presence got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimOutcome {
    /// The connection is the account's live one; `active_game` is its game in progress (0: none).
    Admitted { active_game: GameId },
    /// The account is banned until then (wall-clock ms).
    Banned { until: i64 },
    /// `MAX_CONNECTIONS` players are online and this one has no game in progress.
    Full,
}

/// A lobby request of a connection, with what the connection read for it beforehand.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LobbyRequest {
    QueueJoin {
        category: String,
        rated: bool,
        rating: i64,
        provisional: bool,
        /// End of the stored ban, if any.
        ban: Option<i64>,
        /// The stored conduct state (rated joins only).
        cooldown: Option<Cooldown>,
    },
    QueueLeave,
    ChallengeCreate {
        target: String,
        base_sec: u16,
        inc_sec: u8,
        rated: bool,
        color: ColorPref,
        /// The creator's rating in the challenge's category.
        rating: i64,
        provisional: bool,
        ban: Option<i64>,
    },
    ChallengeAccept {
        id: u32,
    },
    ChallengeDecline {
        id: u32,
    },
    ChallengeCancel {
        id: u32,
    },
    ChallengeJoinCode {
        code: String,
    },
}

/// The lobby's timers (driven by its own loop; by messages in the tests).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Timer {
    MatchTick,
    RefreshQueues,
    ExpireChallenges,
    Sweep,
    RefundPoll,
}

/// A message of the lobby's inbox.
pub(crate) enum LobbyMsg {
    /// A connection authenticated: make it the account's live connection. When the reply cannot
    /// be delivered (the connection is gone), the lobby releases the claim itself.
    Claim {
        link: Arc<ConnLink>,
        ban: Option<i64>,
        reply: oneshot::Sender<ClaimOutcome>,
    },
    /// A connection that claimed presence closed.
    Release {
        user: UserId,
        conn: ConnId,
    },
    /// A lobby request (answered with one `Ack` or `Error`).
    Request {
        link: Arc<ConnLink>,
        seq: u32,
        req: LobbyRequest,
    },
    GameEnded(GameEnded),
    GameRecovered {
        game: GameId,
        white: UserId,
        black: UserId,
    },
    Rematch {
        request: RematchRequest,
        reply: oneshot::Sender<Result<GameId, ErrorCode>>,
    },
    Conduct {
        user: UserId,
        kind: IncidentKind,
    },
    SessionsRevoked {
        user: UserId,
        token_hashes: Option<Vec<[u8; 32]>>,
    },
    SanctionPending(SanctionPending),
    SanctionApplied(SanctionApplied),
    RefundsPending,
    /// A result of the refund notices' background work.
    Refund(RefundEvent),
    /// The results of the lobby's own tasks.
    Done(actor::Done),
    /// Stops the periodic work (the drain at shutdown).
    StopTimers,
    /// Answers with the number of the lobby's tasks in flight (watchdog, tests).
    Ping(oneshot::Sender<usize>),
    /// Ends the actor.
    Stop,
    #[cfg(test)]
    Timer(Timer),
}

/// The way into the lobby actor. Cheap to clone; messages posted after the actor ended are
/// dropped.
#[derive(Clone, Debug)]
pub struct Lobby {
    tx: mpsc::UnboundedSender<LobbyMsg>,
}

/// The receiving end of a lobby's inbox, until the actor starts.
pub struct LobbyInbox {
    tx: mpsc::UnboundedSender<LobbyMsg>,
    rx: mpsc::UnboundedReceiver<LobbyMsg>,
}

impl std::fmt::Debug for LobbyInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LobbyInbox").finish_non_exhaustive()
    }
}

impl Lobby {
    /// A lobby handle and its inbox. The handle can be given to the game hosts before the actor
    /// starts (the games they recover are announced into the inbox).
    pub fn channel() -> (Lobby, LobbyInbox) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Lobby { tx: tx.clone() }, LobbyInbox { tx, rx })
    }

    /// A lobby handle whose inbox the test reads itself (no actor).
    #[cfg(test)]
    pub(crate) fn manual() -> (Lobby, mpsc::UnboundedReceiver<LobbyMsg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Lobby { tx }, rx)
    }

    pub(crate) fn post(&self, msg: LobbyMsg) {
        let _ = self.tx.send(msg);
    }

    /// Resolves to true once the actor has handled every message posted before (systemd
    /// watchdog); false when the actor is gone.
    pub async fn ping(&self) -> bool {
        self.pending_tasks().await.is_some()
    }

    /// The number of the lobby's tasks in flight, once the actor has handled every message posted
    /// before (`None` when the actor is gone).
    pub(crate) async fn pending_tasks(&self) -> Option<usize> {
        let (reply, answer) = oneshot::channel();
        self.post(LobbyMsg::Ping(reply));
        answer.await.ok()
    }

    /// Stops the periodic work (matchmaking, queue refreshes, challenge expiry, sweeps, refund
    /// polls): the drain at shutdown.
    pub fn stop_timers(&self) {
        self.post(LobbyMsg::StopTimers);
    }

    /// Ends the actor.
    pub fn stop(&self) {
        self.post(LobbyMsg::Stop);
    }
}

impl LobbyInbox {
    /// Starts the actor on `deps`.
    pub(crate) fn start(self, deps: LobbyDeps) -> JoinHandle<()> {
        let actor = LobbyActor::new(deps, self.tx);
        tokio::spawn(actor.run(self.rx, true))
    }

    /// Starts the actor without its timers (tests drive them with [`LobbyMsg::Timer`]).
    #[cfg(test)]
    pub(crate) fn start_manual(self, deps: LobbyDeps) -> JoinHandle<()> {
        let actor = LobbyActor::new(deps, self.tx);
        tokio::spawn(actor.run(self.rx, false))
    }
}

impl HostEvents for Lobby {
    fn game_ended(&self, ended: GameEnded) {
        self.post(LobbyMsg::GameEnded(ended));
    }

    fn game_recovered(&self, game: GameId, white: UserId, black: UserId) {
        self.post(LobbyMsg::GameRecovered { game, white, black });
    }

    fn rematch(&self, request: RematchRequest, reply: oneshot::Sender<Result<GameId, ErrorCode>>) {
        self.post(LobbyMsg::Rematch { request, reply });
    }

    fn conduct(&self, user: UserId, kind: IncidentKind) {
        self.post(LobbyMsg::Conduct { user, kind });
    }
}

impl SessionEvents for Lobby {
    fn sessions_revoked(&self, user: UserId, token_hashes: Option<Vec<[u8; 32]>>) {
        self.post(LobbyMsg::SessionsRevoked { user, token_hashes });
    }
}

impl SanctionEvents for Lobby {
    fn sanction_pending(&self, pending: SanctionPending) {
        self.post(LobbyMsg::SanctionPending(pending));
    }

    fn sanction_applied(&self, sanction: SanctionApplied) {
        self.post(LobbyMsg::SanctionApplied(sanction));
    }

    fn refunds_pending(&self) {
        self.post(LobbyMsg::RefundsPending);
    }
}
