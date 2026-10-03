//! Host actors (DESIGN 5.3; docs/RUST-PORT.md sections 2 and 8.1): one tokio task per game
//! shard owns the rooms of its games, their timers, the shard's journal and the commit of
//! finished games.
//!
//! [`Hosts::start`] opens each shard's journal, replays it ([`Shard::recover`], on a blocking
//! thread, before the shard serves anything) and spawns the actor. The actor processes its inbox
//! one message at a time (a connection's frames and its detach stay ordered: they travel through
//! the same inbox), and a 10 ms beat (`MissedTickBehavior::Delay`) runs the timers, detects
//! stalls, starts commits and compacts the journal. A beat that comes more than
//! `GAME_STALL_MIN_MS` late means the actor did not run meanwhile: the actor then yields, handles
//! every message already in its inbox (they count as arrived when the stall began, up to
//! `GAME_STALL_CREDIT_MAX_MS` earlier), and only then fires the deadlines due by that beat, so
//! that a flag that fell during the stall overtakes no request that waited through it.
//!
//! [`HostHandle`] is the cheap, cloneable way in: every method posts to the inbox and returns
//! (only [`HostHandle::create`] waits for the game id). The realtime connection tasks call
//! [`HostHandle::client`], [`HostHandle::gesture`], [`HostHandle::attach`],
//! [`HostHandle::detach`] and [`HostHandle::rtt`]; the lobby creates games, cancels them,
//! forfeits sanctioned players and closes rematch windows. The host talks back through
//! [`HostEvents`] and [`AnomalySink`].
//!
//! A panic in room code is caught: the request gets `Error{Internal}` and the game goes on (a
//! timer that panicked is retried a second later); a panic anywhere else in a message's handling
//! is logged and the actor goes on with the next message.

mod commit;
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

pub use self::commit::{CommitFuture, GameStore, JOURNAL_GATE_TRIES, MAX_BACKOFF_MS};
pub use self::metrics::{Counters, GestureDrop};
pub use self::shard::{RulesFactory, SLOT_MS, Shard, ShardDeps, ShardSettings};

use self::shard::{Shared, panic_message};
use crate::clock::SharedClock;
use crate::config::Config;
use crate::events::{AnomalySink, HostEvents, NewGame};
use crate::ids::{self, ConnId, GameId, MAX_SHARDS, UserId};
use crate::journal::{Journal, JournalError, JournalOptions};
use crate::log::Logger;
use crate::realtime::Endpoint;
use crate::store::{Store, StoreError};
use crate::{log_error, log_info};

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
    /// The database's largest game id could not be read.
    Store(StoreError),
    /// A shard's journal could not be opened.
    Journal { shard: u32, error: JournalError },
    /// A shard's recovery failed (a bug).
    Recovery { shard: u32, message: String },
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::BadShards(r) => write!(f, "bad game shards {}..{} (at most 64)", r.start, r.end),
            HostError::Store(e) => write!(f, "game ids not readable: {e}"),
            HostError::Journal { shard, error } => write!(f, "journal of shard {shard}: {error}"),
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
        HostStats {
            shard: shard.shard,
            games: shard.games(),
            active: shard.active(),
            pending_commits: shard.pending_commits(),
            commit_in_flight: shard.commit_in_flight(),
            timers: shard.timers(),
            players: shard.players(),
            counters: shard.counters().clone(),
        }
    }
}

/// A message of a host's inbox.
enum Msg {
    Client { user: UserId, msg: ClientMsg, ep: Endpoint, recv_at: f64 },
    Gesture { game: GameId, user: UserId, frame: Bytes },
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

/// The way into one host actor (see the module documentation). Cheap to clone. Messages posted
/// after the host shut down are dropped.
#[derive(Clone)]
pub struct HostHandle {
    shard: u32,
    tx: mpsc::UnboundedSender<Msg>,
    shared: Arc<Shared>,
    clock: SharedClock,
}

impl std::fmt::Debug for HostHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostHandle").field("shard", &self.shard).field("load", &self.load()).finish()
    }
}

impl HostHandle {
    /// The shard number.
    #[must_use]
    pub fn shard(&self) -> u32 {
        self.shard
    }

    fn post(&self, msg: Msg) {
        // The host is gone only after the shutdown: nothing to do then.
        let _ = self.tx.send(msg);
    }

    /// A strictly decoded game request (Move, Resign, DrawOffer, DrawAnswer, DrawClaim, Abort,
    /// Resync, Rematch) with the connection it came from and its read time (monotonic ms). A
    /// connection the player has not attached yet is bound (a player back after a restart may
    /// only send Resync).
    pub fn client(&self, user: UserId, msg: ClientMsg, ep: Endpoint, recv_at: f64) {
        self.post(Msg::Client { user, msg, ep, recv_at });
    }

    /// A raw `C_Gesture` frame, relayed to the opponent (validated here).
    pub fn gesture(&self, game: GameId, user: UserId, frame: Bytes) {
        self.post(Msg::Gesture { game, user, frame });
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

    /// A round-trip measurement of the player.
    pub fn rtt(&self, game: GameId, user: UserId, rtt_ms: u32) {
        self.post(Msg::Rtt { game, user, rtt_ms });
    }

    /// Ends the user's running game on this shard as a forfeit (sanction).
    pub fn forfeit_user(&self, user: UserId) {
        self.post(Msg::ForfeitUser { user });
    }

    /// The player joined a queue: the finished game's rematch window closes.
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

/// The host actors of the process, one per shard.
pub struct Hosts {
    /// One per shard, in shard order (`first_shard..`).
    handles: Vec<HostHandle>,
    first_shard: u32,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for Hosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hosts").field("handles", &self.handles).finish_non_exhaustive()
    }
}

impl Hosts {
    /// Opens the journal of each shard of `shards`, recovers its games and starts its actor.
    /// Returns once every shard has recovered (the players can be told their games back).
    ///
    /// # Errors
    ///
    /// A bad shard range, an unreadable database, a journal that cannot be opened.
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

    /// [`Hosts::start`] with given rules, store and journal options (tests).
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
        let logger = Logger::root().child("game");
        let mut hosts =
            Hosts { handles: Vec::new(), first_shard: shards.start, tasks: Mutex::new(Vec::new()) };
        for shard in shards {
            let started = Hosts::start_shard(
                deps,
                shard,
                store.clone(),
                rules.clone(),
                &journal_options,
                &logger,
                last_game_id,
            )
            .await;
            match started {
                Ok((handle, task)) => {
                    hosts.handles.push(handle);
                    hosts.tasks.get_mut().push(task);
                }
                Err(e) => {
                    hosts.shutdown().await;
                    return Err(e);
                }
            }
        }
        Ok(hosts)
    }

    async fn start_shard(
        deps: &HostDeps,
        shard: u32,
        store: Arc<dyn GameStore>,
        rules: RulesFactory,
        journal_options: &impl Fn(u32) -> JournalOptions,
        logger: &Logger,
        last_game_id: GameId,
    ) -> Result<(HostHandle, JoinHandle<()>), HostError> {
        let journal = Journal::open(journal_options(shard))
            .await
            .map_err(|error| HostError::Journal { shard, error })?;
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
        // The replay is CPU work: off the runtime threads.
        let recovered = tokio::task::spawn_blocking(move || {
            let n = core.recover();
            core.publish_load();
            (core, n)
        })
        .await;
        let (core, n) = recovered.map_err(|e| HostError::Recovery { shard, message: e.to_string() })?;
        log_info!(logger, "game host started", { "shard": shard, "recovered": n });
        let (tx, inbox) = mpsc::unbounded_channel();
        let handle = HostHandle { shard, tx, shared: core.shared(), clock: deps.clock.clone() };
        let task = tokio::spawn(run(core, inbox));
        Ok((handle, task))
    }

    /// The host of a game (by the shard bits of its id).
    #[must_use]
    pub fn get(&self, game: GameId) -> Option<&HostHandle> {
        let i = ids::shard_of(game).checked_sub(self.first_shard)?;
        self.handles.get(i as usize)
    }

    /// Where to create a game: the `preferred` shard when it is one of ours, else the host with
    /// the fewest games (the lowest shard on a tie).
    #[must_use]
    pub fn pick(&self, preferred: Option<u32>) -> &HostHandle {
        if let Some(h) = preferred.and_then(|s| self.handles.iter().find(|h| h.shard == s)) {
            return h;
        }
        // `start` never builds an empty set of hosts.
        let mut best = &self.handles[0];
        for h in &self.handles[1..] {
            if h.load().games < best.load().games {
                best = h;
            }
        }
        best
    }

    /// Every host, in shard order.
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

    /// Shuts every host down: final commits (a few attempts), journals flushed and closed.
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
    }
}

/// Handles one message (the shutdown request is returned to the caller, which awaits it).
fn handle(shard: &mut Shard, msg: Msg) -> Option<oneshot::Sender<()>> {
    match msg {
        Msg::Client { user, msg, ep, recv_at } => shard.client(user, &msg, Some(ep), recv_at),
        Msg::Gesture { game, user, frame } => {
            shard.gesture(game, user, &frame);
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

/// The actor of one host (see the module documentation).
async fn run(mut shard: Shard, mut inbox: mpsc::UnboundedReceiver<Msg>) {
    let mut beat = tokio::time::interval(Duration::from_millis(SLOT_MS as u64));
    beat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        // The beat and the task results come at a bounded rate: first, so that a busy inbox
        // cannot starve the timers.
        tokio::select! {
            biased;
            _ = beat.tick() => {
                let t = shard.now();
                if shielded(&mut shard, "game host beat failed", |s| s.heartbeat(t)) == Some(true) {
                    // A stall: what waited in the inbox goes first, then the timers due by now.
                    tokio::task::yield_now().await;
                    while let Ok(msg) = inbox.try_recv() {
                        if step(&mut shard, msg).await.is_break() {
                            return;
                        }
                    }
                    shielded(&mut shard, "game host beat failed", Shard::after_stall);
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
