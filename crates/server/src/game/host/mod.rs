//! Host actors (DESIGN 5.3; docs/RUST-PORT.md sections 2 and 8.1): one tokio task per game
//! shard owns the rooms of its games, their timers, the shard's journal and the commit of
//! finished games.
//!
//! [`Hosts::start`] opens each shard's journal, replays it on a blocking thread before the shard
//! serves anything, reconciled with the database ([`Shard::recover_reconciled`]: a game the
//! database already holds does not come back), and spawns the actor. The actor processes its inbox
//! one message at a time (a connection's frames and its detach stay ordered: they travel through
//! the same inbox), and a 10 ms beat (`MissedTickBehavior::Delay`) runs the timers, detects
//! stalls, starts commits and compacts the journal. A beat first handles the messages already in
//! the inbox (read before it: each request processes the deadlines due at its own arrival, so a
//! move read before its flag deadline is never overtaken by that deadline's timer), never those
//! that arrive meanwhile, so a busy inbox cannot hold the timers back. A beat that comes more than
//! `GAME_STALL_MIN_MS` late means the actor did not run meanwhile: the actor then yields, handles
//! the messages that reached its inbox meanwhile (they count as arrived when the stall began, up
//! to `GAME_STALL_CREDIT_MAX_MS` earlier), and only then fires the deadlines due by that beat, so
//! that a flag that fell during the stall overtakes no request that waited through it.
//!
//! What the inbox holds is counted and bounded at its door (`inbox`): a host with too much
//! waiting ([`HostHandle::busy`], from [`INBOX_BUSY`] messages) gets no new game
//! ([`Hosts::place`]); gestures beyond [`GESTURE_INBOX_MAX`], and every cosmetic message beyond
//! [`INBOX_MAX`], are dropped before it; a game request beyond [`INBOX_MAX`] (one that ends a game
//! beyond [`INBOX_MAX`] + [`INBOX_RESERVE`]) is answered `Error{RateLimited}` at once; the
//! lifecycle messages (attach, detach, create, cancel, forfeit, stats, shutdown) are always
//! delivered.
//!
//! [`HostHandle`] is the cheap, cloneable way in: every method posts to the inbox and returns
//! (only [`HostHandle::create`] waits for the game id). The realtime connection tasks call
//! [`HostHandle::client`], [`HostHandle::gesture`], [`HostHandle::stance`],
//! [`HostHandle::attach`], [`HostHandle::detach`] and [`HostHandle::rtt`]; the lobby creates
//! games, cancels them, forfeits sanctioned players and closes rematch windows. The host talks
//! back through [`HostEvents`] and [`AnomalySink`].
//!
//! A panic in room code is caught: the request gets `Error{Internal}` and the game goes on (a
//! timer that panicked is retried a second later); a panic anywhere else in a message's handling
//! is logged and the actor goes on with the next message.

mod commit;
mod inbox;
mod metrics;
mod shard;

#[cfg(test)]
mod tests;

use std::ops::{ControlFlow, Range};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use scacelith_chess::ChessGame;
use scacelith_protocol::{ClientMsg, ErrorCode};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

pub use self::commit::{CommitFuture, GameStore, JOURNAL_GATE_TRIES, LookupFuture, MAX_BACKOFF_MS, Stored};
pub use self::inbox::{
    GESTURE_INBOX_MAX, INBOX_BUSY, INBOX_MAX, INBOX_RESERVE, JOURNAL_PENDING_BUSY, Refusal,
};
pub use self::metrics::{Counters, GestureDrop, RecoveryDrop};
pub use self::shard::{Recovery, RulesFactory, SLOT_MS, Shard, ShardDeps, ShardSettings};

use self::inbox::Class;
use self::shard::{Shared, error_frame, panic_message};
use crate::clock::SharedClock;
use crate::config::Config;
use crate::events::{AnomalySink, HostEvents, NewGame};
use crate::ids::{self, ConnId, GameId, MAX_SHARDS, UserId};
use crate::journal::owner::{self, ClaimError, ShardClaim};
use crate::journal::{Journal, JournalError, JournalOptions};
use crate::log::Logger;
use crate::realtime::Endpoint;
use crate::store::{Store, StoreError};
use crate::{log_error, log_info, log_warn};

/// What the host actors need.
#[derive(Clone)]
pub struct HostDeps {
    pub config: Arc<Config>,
    pub clock: SharedClock,
    pub store: Store,
    pub events: Arc<dyn HostEvents>,
    pub anomalies: Arc<dyn AnomalySink>,
}

impl std::fmt::Debug for HostDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostDeps").field("store", &self.store).finish_non_exhaustive()
    }
}

/// Why the hosts could not start.
#[derive(Debug)]
pub enum HostError {
    /// The shard range is empty or goes beyond the 64 shards a game id can address.
    BadShards(Range<u32>),
    /// The database could not be read (largest game id, server id, the games of a journal).
    Store(StoreError),
    /// `JOURNAL_DIR` could not be listed.
    JournalDir(std::io::Error),
    /// A shard's journal could not be opened.
    Journal { shard: u32, error: JournalError },
    /// Another process serves a shard of this instance's range (two servers share `JOURNAL_DIR`).
    ShardInUse(u32),
    /// A shard of this instance's range holds games of another database.
    ForeignJournal { shard: u32, owner: String, server_id: String, games: usize },
    /// A shard's recovery failed (a bug).
    Recovery { shard: u32, message: String },
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::BadShards(r) => write!(f, "bad game shards {}..{} (at most 64)", r.start, r.end),
            HostError::Store(e) => write!(f, "database not readable: {e}"),
            HostError::JournalDir(e) => write!(f, "JOURNAL_DIR not readable: {e}"),
            HostError::Journal { shard, error } => write!(f, "journal of shard {shard}: {error}"),
            HostError::ShardInUse(shard) => write!(
                f,
                "the journal of shard {shard} is in use by another process: every server needs its own JOURNAL_DIR \
                 (docs/DEPLOY.md section 11)"
            ),
            HostError::ForeignJournal { shard, owner, server_id, games } => write!(
                f,
                "the journal of shard {shard} holds {games} games of another database (server id {owner}; this \
                 database is {server_id}): point JOURNAL_DIR at this database's journal, or move \
                 JOURNAL_DIR/shard-{shard} aside (docs/DEPLOY.md section 11)"
            ),
            HostError::Recovery { shard, message } => {
                write!(f, "recovery of shard {shard} failed: {message}")
            }
        }
    }
}

impl std::error::Error for HostError {}

/// The load of a host (placement), read without waiting for the actor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostLoad {
    /// Games hosted, finished ones waiting for their commit or rematch window included.
    pub games: usize,
    /// Players with a running game.
    pub players: usize,
}

/// Numbers of one host (admin view, tests).
#[derive(Clone, Debug, PartialEq)]
pub struct HostStats {
    pub shard: u32,
    /// Games hosted.
    pub games: usize,
    /// Games in progress.
    pub active: usize,
    /// Finished games waiting for their commit.
    pub pending_commits: usize,
    pub commit_in_flight: bool,
    /// Timers scheduled.
    pub timers: usize,
    /// Players with a running game.
    pub players: usize,
    /// What the host counted since it started.
    pub counters: Counters,
}

impl HostStats {
    fn of(shard: &Shard) -> HostStats {
        let mut counters = shard.counters().clone();
        let backlog = &shard.shared().backlog;
        let refused = backlog.refused(Refusal::Gesture);
        if refused > 0 {
            *counters.gesture_drops.entry(GestureDrop::Overload.as_str()).or_default() += refused;
        }
        for kind in Refusal::ALL.into_iter().filter(|&k| k != Refusal::Gesture) {
            let n = backlog.refused(kind);
            if n > 0 {
                counters.inbox_refused.insert(kind.as_str(), n);
            }
        }
        counters.inbox_over_reserve = backlog.over_reserve();
        HostStats {
            shard: shard.shard,
            games: shard.games(),
            active: shard.active(),
            pending_commits: shard.pending_commits(),
            commit_in_flight: shard.commit_in_flight(),
            timers: shard.timers(),
            players: shard.players(),
            counters,
        }
    }
}

/// A message of a host's inbox.
enum Msg {
    Client { user: UserId, msg: ClientMsg, ep: Endpoint, recv_at: f64 },
    Gesture { game: GameId, user: UserId, frame: Bytes },
    Stance { game: GameId, user: UserId, frame: Bytes },
    Attach { game: GameId, user: UserId, ep: Endpoint },
    Detach { game: GameId, user: UserId, conn: ConnId },
    Rtt { game: GameId, user: UserId, rtt_ms: u32 },
    ForfeitUser { user: UserId },
    DeclineRematch { game: GameId, user: UserId },
    Create { game: NewGame, reply: oneshot::Sender<Result<GameId, ErrorCode>> },
    Cancel { game: GameId },
    Stats { reply: oneshot::Sender<HostStats> },
    Shutdown { reply: oneshot::Sender<()> },
}

impl Msg {
    fn is_gesture(&self) -> bool {
        matches!(self, Msg::Gesture { .. })
    }

    /// The budget of the message at the inbox's door (`inbox`).
    fn class(&self) -> Class {
        match self {
            Msg::Gesture { .. } => Class::Cosmetic(Refusal::Gesture),
            Msg::Stance { .. } => Class::Cosmetic(Refusal::Stance),
            Msg::Rtt { .. } => Class::Cosmetic(Refusal::Rtt),
            Msg::DeclineRematch { .. } => Class::Cosmetic(Refusal::RematchDecline),
            Msg::Client { msg, .. } => match msg {
                ClientMsg::Resign(_)
                | ClientMsg::Abort(_)
                | ClientMsg::DrawAnswer(_)
                | ClientMsg::DrawClaim(_) => Class::Ending,
                _ => Class::Request,
            },
            Msg::Attach { .. }
            | Msg::Detach { .. }
            | Msg::ForfeitUser { .. }
            | Msg::Create { .. }
            | Msg::Cancel { .. }
            | Msg::Stats { .. }
            | Msg::Shutdown { .. } => Class::Lifecycle,
        }
    }
}

/// The way into one host actor (see the module documentation). Cheap to clone. Messages posted
/// after the host shut down are dropped.
#[derive(Clone)]
pub struct HostHandle {
    shard: u32,
    /// Outside the instance's shard range: it serves the games of its journal, never a new one.
    draining: bool,
    tx: mpsc::UnboundedSender<Msg>,
    shared: Arc<Shared>,
    clock: SharedClock,
}

impl std::fmt::Debug for HostHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostHandle")
            .field("shard", &self.shard)
            .field("draining", &self.draining)
            .field("load", &self.load())
            .finish()
    }
}

impl HostHandle {
    /// The shard number.
    #[must_use]
    pub fn shard(&self) -> u32 {
        self.shard
    }

    /// Whether the shard is outside the instance's range (`SHARD_BASE` to `SHARD_BASE + WORKERS -
    /// 1`) and only serves the games its journal held at start.
    #[must_use]
    pub fn draining(&self) -> bool {
        self.draining
    }

    /// Posts a message admitted under its class's budget (`inbox`); false when it was refused
    /// at the door (counted; nothing was posted).
    fn post(&self, msg: Msg) -> bool {
        if !self.shared.backlog.admit(msg.class()) {
            return false;
        }
        self.send(msg);
        true
    }

    /// Sends a message counted in the backlog.
    fn send(&self, msg: Msg) {
        // The host is gone only after the shutdown: nothing to do then.
        if let Err(mpsc::error::SendError(msg)) = self.tx.send(msg) {
            self.shared.backlog.taken(msg.is_gesture());
        }
    }

    /// A strictly decoded game request (Move, Resign, DrawOffer, DrawAnswer, DrawClaim, Abort,
    /// Resync, Rematch) with the connection it came from and its read time (monotonic ms). A
    /// connection the player has not attached yet is bound (a player back after a restart may
    /// only send Resync). Refused when the host is that far behind ([`INBOX_MAX`] messages
    /// waiting, [`INBOX_MAX`] + [`INBOX_RESERVE`] for a request that ends a game): the
    /// connection gets `Error{RateLimited}` for it at once, and nothing is played.
    pub fn client(&self, user: UserId, msg: ClientMsg, ep: Endpoint, recv_at: f64) {
        let (seq, game) = (msg.seq(), shard::game_of(&msg));
        let refused = ep.clone();
        if !self.post(Msg::Client { user, msg, ep, recv_at }) {
            metrics::busy_reject(ErrorCode::RateLimited);
            if let Some(frame) = error_frame(ErrorCode::RateLimited, seq, false, game) {
                refused.send(frame);
            }
        }
    }

    /// A raw `C_Gesture` frame, relayed to the opponent (validated here). Dropped at once when
    /// [`GESTURE_INBOX_MAX`] gestures, or [`INBOX_MAX`] messages, already wait for the host.
    pub fn gesture(&self, game: GameId, user: UserId, frame: Bytes) {
        self.post(Msg::Gesture { game, user, frame });
    }

    /// A raw `C_Stance` frame (minor 2), relayed to the opponent (validated here). Not capped
    /// like the gestures (a client sends one when its player's stance changes and, standing, one
    /// per gesture keepalive, within its connection's message rate), but cosmetic: dropped while
    /// [`INBOX_MAX`] messages wait.
    pub fn stance(&self, game: GameId, user: UserId, frame: Bytes) {
        self.post(Msg::Stance { game, user, frame });
    }

    /// Binds the player's connection to the game and sends it a `GameSnapshot`
    /// (`Error{NotInGame}` when the game is not here or the user does not play it).
    pub fn attach(&self, game: GameId, user: UserId, ep: Endpoint) {
        self.post(Msg::Attach { game, user, ep });
    }

    /// The player's connection `conn` closed (ignored when another connection replaced it).
    pub fn detach(&self, game: GameId, user: UserId, conn: ConnId) {
        self.post(Msg::Detach { game, user, conn });
    }

    /// A round-trip measurement of the player (dropped while [`INBOX_MAX`] messages wait: the
    /// next one comes with the next heartbeat).
    pub fn rtt(&self, game: GameId, user: UserId, rtt_ms: u32) {
        self.post(Msg::Rtt { game, user, rtt_ms });
    }

    /// Ends the user's running game on this shard as a forfeit (sanction).
    pub fn forfeit_user(&self, user: UserId) {
        self.post(Msg::ForfeitUser { user });
    }

    /// The player joined a queue: the finished game's rematch window closes (dropped while
    /// [`INBOX_MAX`] messages wait: the window then closes on its timer).
    pub fn decline_rematch(&self, game: GameId, user: UserId) {
        self.post(Msg::DeclineRematch { game, user });
    }

    /// Creates a game on this shard (its timers start at once).
    ///
    /// # Errors
    ///
    /// `ShuttingDown` once the host shut down, `Internal` for a bug.
    pub async fn create(&self, game: NewGame) -> Result<GameId, ErrorCode> {
        let (reply, answer) = oneshot::channel();
        self.post(Msg::Create { game, reply });
        answer.await.unwrap_or(Err(ErrorCode::ShuttingDown))
    }

    /// Ends a game the lobby gave up creating (`ServerAborted`, no conduct incident).
    pub fn cancel(&self, game: GameId) {
        self.post(Msg::Cancel { game });
    }

    /// The load of the host.
    #[must_use]
    pub fn load(&self) -> HostLoad {
        HostLoad {
            games: self.shared.games.load(Ordering::Relaxed),
            players: self.shared.players.load(Ordering::Relaxed),
        }
    }

    /// Whether the host has too much work waiting to take a new game: [`INBOX_BUSY`] messages in
    /// its inbox, or [`JOURNAL_PENDING_BUSY`] bytes of journal records not handed to its I/O
    /// thread.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.shared.backlog.busy()
    }

    /// Messages waiting in the host's inbox, and the gestures among them.
    #[must_use]
    pub fn inbox(&self) -> (usize, usize) {
        (self.shared.backlog.messages(), self.shared.backlog.gestures())
    }

    /// Whether a stall of this host overlaps the time since `since_mono_ms`.
    #[must_use]
    pub fn stall_during(&self, since_mono_ms: f64) -> bool {
        self.shared.stall_during(since_mono_ms, self.clock.now_ms())
    }

    /// The host's numbers (`None` once it shut down).
    pub async fn stats(&self) -> Option<HostStats> {
        let (reply, answer) = oneshot::channel();
        self.post(Msg::Stats { reply });
        answer.await.ok()
    }
}

/// The host actors of the process, one per shard: those of the instance's range, and those of the
/// shards outside it whose journal held games at start (draining: they serve these games until
/// they end and get no new game).
pub struct Hosts {
    /// Every host, in shard order.
    handles: Vec<HostHandle>,
    /// The index in `handles` of each shard's host.
    by_shard: [Option<u8>; MAX_SHARDS as usize],
    /// The shard directories claimed, released once the hosts shut down.
    claims: Mutex<Vec<ShardClaim>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The database, whose write backlog also refuses new games.
    store: Store,
}

impl std::fmt::Debug for Hosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hosts").field("handles", &self.handles).finish_non_exhaustive()
    }
}

/// What starting one shard gave.
enum Started {
    /// A host, with its actor and the claim on its directory.
    Host(HostHandle, JoinHandle<()>, ShardClaim),
    /// Nothing to serve: a directory outside the range without games, or not this instance's.
    Skipped,
}

impl Hosts {
    /// Starts the hosts of the shards of `shards` (`SHARD_BASE` to `SHARD_BASE + WORKERS - 1`)
    /// and of every other shard whose journal under `JOURNAL_DIR` holds games of this database
    /// (draining hosts, see [`HostHandle::draining`]): each one's directory is claimed (locked
    /// and marked with the database's server id: [`ShardClaim`]), its journal opened, its games
    /// recovered and reconciled with the database, and its actor started. Returns once every
    /// shard has recovered (the players can be told their games back).
    ///
    /// # Errors
    ///
    /// A bad shard range, an unreadable database or `JOURNAL_DIR`, a journal that cannot be
    /// opened, a shard of the range that another process serves or whose journal holds the games
    /// of another database.
    pub async fn start(deps: HostDeps, shards: Range<u32>) -> Result<Hosts, HostError> {
        let rules: RulesFactory = Arc::new(|| Box::new(ChessGame::default()));
        let store: Arc<dyn GameStore> = Arc::new(deps.store.clone());
        let config = deps.config.clone();
        let clock = deps.clock.clone();
        let journal = move |shard| {
            let mut options = JournalOptions::from_config(&config, shard);
            options.clock = clock.clone();
            options
        };
        Hosts::start_with(&deps, shards, store, rules, journal).await
    }

    /// [`Hosts::start`] with given rules, store and journal options (tests). The journal options
    /// of every shard name the same directory.
    pub(crate) async fn start_with(
        deps: &HostDeps,
        shards: Range<u32>,
        store: Arc<dyn GameStore>,
        rules: RulesFactory,
        journal_options: impl Fn(u32) -> JournalOptions,
    ) -> Result<Hosts, HostError> {
        if shards.is_empty() || shards.end > MAX_SHARDS {
            return Err(HostError::BadShards(shards));
        }
        let last_game_id = deps.store.games().last_id().await.map_err(HostError::Store)?;
        let server_id = deps.store.server_id().await.map_err(HostError::Store)?;
        let root = journal_options(shards.start).dir;
        let mut all: Vec<u32> = shards.clone().collect();
        all.extend(owner::shard_dirs(&root).map_err(HostError::JournalDir)?);
        all.sort_unstable();
        all.dedup();
        let logger = Logger::root().child("game");
        let mut hosts = Hosts {
            handles: Vec::new(),
            by_shard: [None; MAX_SHARDS as usize],
            claims: Mutex::new(Vec::new()),
            tasks: Mutex::new(Vec::new()),
            store: deps.store.clone(),
        };
        for shard in all {
            let at = ShardAt { shard, draining: !shards.contains(&shard), server_id: server_id.as_deref() };
            let started = Hosts::start_shard(
                deps,
                at,
                store.clone(),
                rules.clone(),
                &journal_options,
                &logger,
                last_game_id,
            )
            .await;
            match started {
                Ok(Started::Host(handle, task, claim)) => {
                    hosts.by_shard[shard as usize] = u8::try_from(hosts.handles.len()).ok();
                    hosts.handles.push(handle);
                    hosts.tasks.get_mut().push(task);
                    hosts.claims.get_mut().push(claim);
                }
                Ok(Started::Skipped) => {}
                Err(e) => {
                    hosts.shutdown().await;
                    return Err(e);
                }
            }
        }
        metrics::draining_shards(hosts.handles.iter().filter(|h| h.draining).count());
        Ok(hosts)
    }

    async fn start_shard(
        deps: &HostDeps,
        at: ShardAt<'_>,
        store: Arc<dyn GameStore>,
        rules: RulesFactory,
        journal_options: &impl Fn(u32) -> JournalOptions,
        logger: &Logger,
        last_game_id: GameId,
    ) -> Result<Started, HostError> {
        let ShardAt { shard, draining, server_id } = at;
        let options = journal_options(shard);
        let skip = |why: &str| {
            log_warn!(logger, "game shard outside this instance's range left alone", {
                "shard": shard, "why": why,
            });
            Ok(Started::Skipped)
        };
        if draining && !owner::has_segments(&options.dir, shard).unwrap_or(true) {
            // Nothing to recover: the directory is left as it is.
            return Ok(Started::Skipped);
        }
        let mut claim = match ShardClaim::take(&options.dir, shard) {
            Ok(claim) => claim,
            Err(ClaimError::InUse) if draining => return skip("in use by another process"),
            Err(ClaimError::InUse) => return Err(HostError::ShardInUse(shard)),
            Err(ClaimError::Io(e)) if draining => return skip(&e.to_string()),
            Err(ClaimError::Io(e)) => {
                let error = JournalError::Io { kind: e.kind(), message: format!("owner file: {e}") };
                return Err(HostError::Journal { shard, error });
            }
        };
        if !claim.locked() {
            log_warn!(logger, "journal directory not locked: the file system has no flock", { "shard": shard });
        }
        let foreign = match (claim.owner(), server_id) {
            (Some(owner), Some(id)) if owner != id => Some(owner.to_owned()),
            _ => None,
        };
        if draining && foreign.is_some() {
            return skip("journal of another database");
        }
        let journal = Journal::open(options).await.map_err(|error| HostError::Journal { shard, error })?;
        let games = journal.recover().len();
        if let Some(owner) = foreign
            && games > 0
        {
            let _ = journal.close().await;
            let server_id = server_id.unwrap_or_default().to_owned();
            return Err(HostError::ForeignJournal { shard, owner, server_id, games });
        }
        if draining && games == 0 {
            // A directory a former range left behind, its games all committed.
            let _ = journal.close().await;
            return Ok(Started::Skipped);
        }
        if let Some(id) = server_id {
            claim.set_owner(id).map_err(|e| HostError::Journal {
                shard,
                error: JournalError::Io { kind: e.kind(), message: format!("owner file: {e}") },
            })?;
        }
        let mut core = Shard::new(ShardDeps {
            shard,
            settings: ShardSettings::from_config(&deps.config),
            clock: deps.clock.clone(),
            store,
            journal: Some(journal),
            events: deps.events.clone(),
            anomalies: deps.anomalies.clone(),
            rules,
            logger: logger.clone(),
            last_game_id,
        });
        // The replay is CPU work: off the runtime threads. Nothing is published before the
        // database has said which of the games it already holds.
        let rebuilt = tokio::task::spawn_blocking(move || {
            let recovery = core.rebuild();
            (core, recovery)
        })
        .await;
        let (mut core, recovery) =
            rebuilt.map_err(|e| HostError::Recovery { shard, message: e.to_string() })?;
        let stored = core.lookup(&recovery).await.map_err(HostError::Store)?;
        let recovered = tokio::task::spawn_blocking(move || {
            let n = core.publish(recovery, &stored);
            core.publish_load();
            (core, n)
        })
        .await;
        let (core, n) = recovered.map_err(|e| HostError::Recovery { shard, message: e.to_string() })?;
        if draining {
            log_warn!(logger, "game shard outside this instance's range: it serves the games of its journal until they end, and takes no new game", {
                "shard": shard, "recovered": n, "games": core.games(),
            });
        } else {
            log_info!(logger, "game host started", { "shard": shard, "recovered": n });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = HostHandle { shard, draining, tx, shared: core.shared(), clock: deps.clock.clone() };
        let task = tokio::spawn(run(core, Inbox { rx, shared: handle.shared.clone() }));
        Ok(Started::Host(handle, task, claim))
    }

    /// The host of a game (by the shard bits of its id).
    #[must_use]
    pub fn get(&self, game: GameId) -> Option<&HostHandle> {
        let i = self.by_shard.get(ids::shard_of(game) as usize).copied().flatten()?;
        self.handles.get(usize::from(i))
    }

    /// Where to create a game: the `preferred` shard when it is one of the instance's range,
    /// else the host of the range with the fewest games (the lowest shard on a tie). A draining
    /// host, or a busy one ([`HostHandle::busy`]), is never chosen: `None` when every host of the
    /// range is busy.
    #[must_use]
    pub fn pick(&self, preferred: Option<u32>) -> Option<&HostHandle> {
        let free = |h: &&HostHandle| !h.draining && !h.busy();
        if let Some(h) = preferred.and_then(|s| self.handles.iter().filter(free).find(|h| h.shard == s)) {
            return Some(h);
        }
        let mut best: Option<&HostHandle> = None;
        for h in self.handles.iter().filter(free) {
            if best.is_none_or(|b| h.load().games < b.load().games) {
                best = Some(h);
            }
        }
        best
    }

    /// The host to create a new game on ([`Hosts::pick`]), or `None` when the server is saturated:
    /// every host of the range is busy, or the database has [`crate::store::WRITE_BACKLOG_BUSY`]
    /// write jobs waiting or more (a new game is refused with `RateLimited` then).
    #[must_use]
    pub fn place(&self, preferred: Option<u32>) -> Option<&HostHandle> {
        if self.store.writes_backlogged() {
            return None;
        }
        self.pick(preferred)
    }

    /// Whether a new game would be refused for now ([`Hosts::place`]).
    #[must_use]
    pub fn saturated(&self) -> bool {
        self.place(None).is_none()
    }

    /// Every host, in shard order (draining ones included).
    #[must_use]
    pub fn handles(&self) -> &[HostHandle] {
        &self.handles
    }

    /// Whether a stall of any host overlaps the time since `since_mono_ms` (a round trip
    /// measured over it includes the stall: the RTT average leaves it out).
    #[must_use]
    pub fn stall_during(&self, since_mono_ms: f64) -> bool {
        self.handles.iter().any(|h| h.stall_during(since_mono_ms))
    }

    /// Shuts every host down: final commits (a few attempts), journals flushed and closed, then
    /// the shard directories released.
    pub async fn shutdown(&self) {
        let answers: Vec<_> = self
            .handles
            .iter()
            .map(|h| {
                let (reply, answer) = oneshot::channel();
                h.post(Msg::Shutdown { reply });
                answer
            })
            .collect();
        for answer in answers {
            let _ = answer.await;
        }
        let tasks = std::mem::take(&mut *self.tasks.lock());
        for task in tasks {
            let _ = task.await;
        }
        self.claims.lock().clear();
    }
}

/// A shard to start, and how.
#[derive(Clone, Copy)]
struct ShardAt<'a> {
    shard: u32,
    /// Outside the instance's range.
    draining: bool,
    /// The database's server id (`None`: unknown, the owner files are neither checked nor written).
    server_id: Option<&'a str>,
}

/// Handles one message (the shutdown request is returned to the caller, which awaits it).
fn handle(shard: &mut Shard, msg: Msg) -> Option<oneshot::Sender<()>> {
    match msg {
        Msg::Client { user, msg, ep, recv_at } => shard.client(user, &msg, Some(ep), recv_at),
        Msg::Gesture { game, user, frame } => {
            shard.gesture(game, user, &frame);
        }
        Msg::Stance { game, user, frame } => {
            shard.stance(game, user, &frame);
        }
        Msg::Attach { game, user, ep } => {
            shard.attach(game, user, ep);
        }
        Msg::Detach { game, user, conn } => {
            shard.detach(game, user, Some(conn));
        }
        Msg::Rtt { game, user, rtt_ms } => shard.rtt(game, user, rtt_ms),
        Msg::ForfeitUser { user } => {
            shard.forfeit_user(user);
        }
        Msg::DeclineRematch { game, user } => shard.decline_rematch(game, user),
        Msg::Create { game, reply } => {
            let _ = reply.send(shard.create(game));
        }
        Msg::Cancel { game } => {
            shard.cancel(game);
        }
        Msg::Stats { reply } => {
            let _ = reply.send(HostStats::of(shard));
        }
        Msg::Shutdown { reply } => return Some(reply),
    }
    None
}

/// Runs `f` on the shard, logging a panic (a bug) instead of losing the actor.
fn shielded<R>(shard: &mut Shard, what: &str, f: impl FnOnce(&mut Shard) -> R) -> Option<R> {
    match catch_unwind(AssertUnwindSafe(|| f(shard))) {
        Ok(r) => Some(r),
        Err(panic) => {
            log_error!(shard.log, what, { "shard": shard.shard, "err": panic_message(panic.as_ref()) });
            None
        }
    }
}

/// Handles one message; `Break` once the host shut down.
async fn step(shard: &mut Shard, msg: Msg) -> ControlFlow<()> {
    let Some(Some(reply)) = shielded(shard, "game host message failed", |s| handle(s, msg)) else {
        return ControlFlow::Continue(());
    };
    shard.shutdown().await;
    let _ = reply.send(());
    ControlFlow::Break(())
}

/// The receiving end of a host's inbox: what it hands out leaves the host's backlog.
struct Inbox {
    rx: mpsc::UnboundedReceiver<Msg>,
    shared: Arc<Shared>,
}

impl Inbox {
    fn taken(&self, msg: Msg) -> Msg {
        self.shared.backlog.taken(msg.is_gesture());
        msg
    }

    async fn recv(&mut self) -> Option<Msg> {
        let msg = self.rx.recv().await?;
        Some(self.taken(msg))
    }

    fn try_recv(&mut self) -> Option<Msg> {
        let msg = self.rx.try_recv().ok()?;
        Some(self.taken(msg))
    }

    fn len(&self) -> usize {
        self.rx.len()
    }
}

/// Handles the messages already in the inbox, not those that arrive meanwhile (a busy inbox
/// cannot hold a beat back); `Break` once the host shut down.
async fn drain(shard: &mut Shard, inbox: &mut Inbox) -> ControlFlow<()> {
    for _ in 0..inbox.len() {
        let Some(msg) = inbox.try_recv() else { break };
        step(shard, msg).await?;
    }
    ControlFlow::Continue(())
}

/// One beat: the requests already in the inbox first (they were read before it, and each one
/// processes the deadlines due at its own arrival: a move read before its flag deadline is not
/// overtaken by the timer of that deadline), then the timers, commits and compaction. After a
/// detected stall, what reached the inbox meanwhile goes first too, then the timers due by that
/// beat. `Break` once the host shut down.
async fn beat(shard: &mut Shard, inbox: &mut Inbox) -> ControlFlow<()> {
    drain(shard, inbox).await?;
    let t = shard.now();
    if shielded(shard, "game host beat failed", |s| s.heartbeat(t)) == Some(true) {
        tokio::task::yield_now().await;
        drain(shard, inbox).await?;
        shielded(shard, "game host beat failed", Shard::after_stall);
    }
    shard.publish_backlog();
    ControlFlow::Continue(())
}

/// The actor of one host (see the module documentation).
async fn run(mut shard: Shard, mut inbox: Inbox) {
    let mut ticks = tokio::time::interval(Duration::from_millis(SLOT_MS as u64));
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        // The beat and the task results come at a bounded rate: first, so that a busy inbox
        // cannot starve the timers.
        tokio::select! {
            biased;
            _ = ticks.tick() => {
                if beat(&mut shard, &mut inbox).await.is_break() {
                    return;
                }
            }
            Some(result) = shard.tasks.join_next_with_id(), if !shard.tasks.is_empty() => {
                shielded(&mut shard, "game host task result failed", |s| s.on_task(result));
            }
            msg = inbox.recv() => {
                let Some(msg) = msg else {
                    // Every handle is gone: nothing more can come.
                    shard.shutdown().await;
                    break;
                };
                if step(&mut shard, msg).await.is_break() {
                    break;
                }
            }
        }
        shard.publish_load();
    }
}
