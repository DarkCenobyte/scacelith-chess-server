//! The state of one host actor and everything it does between two awaits: the rooms of its
//! shard, their endpoints and timers, the stall credit, the gesture relay, the anomalies, the
//! journal appends, compaction and recovery. The commit pipeline is in `commit.rs`; the actor
//! loop and the public handles in `mod.rs`.
//!
//! Every method runs to completion on the actor's task and never blocks: journal appends are
//! buffered by the journal, notifications post to their actors, and the few things to wait for
//! (a journal flush, a database commit, the lobby's answer to a rematch) run as tasks of
//! [`Shard::tasks`] whose results come back through [`Shard::on_task`].
//!
//! Time is the clock's monotonic milliseconds, floored ([`crate::clock::Clock::now_ms`]); the
//! methods driven by the beat take it as an argument, the others read the clock.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Instant;

use bytes::Bytes;
use indexmap::IndexSet;
use scacelith_protocol::{ClientGesture, ClientMsg, ErrorCode, Message, close};
use serde_json::json;
use tokio::sync::oneshot;
use tokio::task::{Id as TaskId, JoinError, JoinSet};

use super::commit::{CommitError, Done, GameStore, TaskKind};
use super::metrics::{Counters, GestureDrop, Meter};
use crate::clock::SharedClock;
use crate::config::Config;
use crate::events::{Anomaly, AnomalySink, HostEvents, NewGame, RematchRequest};
use crate::game::room::{
    CERTAIN_KINDS, GameRoom, JournalRecord, NEVER, Outcome, RematchSpec, RoomSettings, RoomSpec, Timing,
};
use crate::game::rules::{Rules, Side};
use crate::game::timers::TimerSet;
use crate::ids::{ConnId, GameId, GameIdAllocator, UserId};
use crate::journal::Journal;
use crate::log::{self, Logger};
use crate::realtime::Endpoint;
use crate::{log_error, log_info, log_warn};

/// Period of the actor's beat (timers, stall detection, commits, compaction).
pub const SLOT_MS: i64 = 10;
/// Journal snapshots built per beat at most (compaction stays off the move path).
pub(super) const SNAPSHOTS_PER_TICK: usize = 2;
/// Finished games per database commit at most.
pub(super) const COMMIT_BATCH_MAX: usize = 500;

/// Builds the rules of a new or replayed game.
pub type RulesFactory = Arc<dyn Fn() -> Box<dyn Rules> + Send + Sync>;

/// The settings of a host (from the configuration).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShardSettings {
    /// The rooms' settings.
    pub room: RoomSettings,
    /// `DB_COMMIT_MS`: finished games are committed this long after the first of a batch ended.
    pub commit_ms: i64,
    /// Finished games per commit at most.
    pub commit_batch_max: usize,
    /// `AUTO_SANCTION_CERTAIN_CHEATS`.
    pub auto_sanction: bool,
    /// `GAME_STALL_MIN_MS`: a beat this much later than due reveals a stall.
    pub stall_min_ms: i64,
    /// `GAME_STALL_CREDIT_MAX_MS`: the most a stall gives back to a request.
    pub stall_credit_max_ms: i64,
    /// The official category ids (`RATED_CATEGORIES`): only their games are rated.
    pub categories: Vec<String>,
}

impl ShardSettings {
    /// The settings of the configuration.
    #[must_use]
    pub fn from_config(config: &Config) -> ShardSettings {
        ShardSettings {
            room: RoomSettings::from_config(config),
            commit_ms: config.db_commit_ms.max(0),
            commit_batch_max: COMMIT_BATCH_MAX,
            auto_sanction: config.auto_sanction_certain_cheats,
            stall_min_ms: config.game_stall_min_ms.max(0),
            stall_credit_max_ms: config.game_stall_credit_max_ms.max(0),
            categories: config.categories.iter().map(|c| c.id.clone()).collect(),
        }
    }
}

/// What a host is built from.
pub struct ShardDeps {
    /// Shard number (below 64).
    pub shard: u32,
    pub settings: ShardSettings,
    pub clock: SharedClock,
    pub store: Arc<dyn GameStore>,
    /// The shard's journal (`None`: nothing is journaled, commits do not wait; tests only).
    pub journal: Option<Journal>,
    pub events: Arc<dyn HostEvents>,
    pub anomalies: Arc<dyn AnomalySink>,
    pub rules: RulesFactory,
    pub logger: Logger,
    /// The largest game id of the database: new ids come after it.
    pub last_game_id: GameId,
}

/// Numbers of a host shared with its handles (read from any thread).
#[derive(Debug)]
pub(super) struct Shared {
    pub(super) games: AtomicUsize,
    pub(super) players: AtomicUsize,
    /// Time of the latest beat (`i64::MIN`: none).
    beat: AtomicI64,
    /// Start of the stall the latest beat detected, until its timers ran ([`NEVER`]: none).
    stall_from: AtomicI64,
    /// Time of the beat that detected the latest stall (`i64::MIN`: none).
    last_stall_end: AtomicI64,
    stall_min_ms: i64,
    stall_credit_max_ms: i64,
}

impl Shared {
    fn new(settings: &ShardSettings) -> Shared {
        Shared {
            games: AtomicUsize::new(0),
            players: AtomicUsize::new(0),
            beat: AtomicI64::new(i64::MIN),
            stall_from: AtomicI64::new(NEVER),
            last_stall_end: AtomicI64::new(i64::MIN),
            stall_min_ms: settings.stall_min_ms,
            stall_credit_max_ms: settings.stall_credit_max_ms,
        }
    }

    /// Whether a stall of the host overlaps the time since `t0` (see [`Shard::stall_during`]).
    pub(super) fn stall_during(&self, t0: f64, t: i64) -> bool {
        let last_end = self.last_stall_end.load(Ordering::Relaxed);
        if last_end != i64::MIN && last_end as f64 > t0 {
            return true;
        }
        let beat = self.beat.load(Ordering::Relaxed);
        let beat = (beat != i64::MIN).then_some(beat);
        let from = self.stall_from.load(Ordering::Relaxed);
        let from = (from != NEVER).then_some(from);
        let start = stall_start(beat, from, t, self.stall_min_ms);
        stall_credit(t, start, self.stall_credit_max_ms) > 0
    }
}

/// Start of the stall a request handled at `t` waited through ([`NEVER`]: none): the stall the
/// latest beat detected, until the timers ran after it, or the one that began when the next beat
/// was due, when that beat is already `stall_min_ms` late.
fn stall_start(beat: Option<i64>, stall_from: Option<i64>, t: i64, stall_min_ms: i64) -> i64 {
    if let Some(from) = stall_from {
        return from;
    }
    match beat {
        Some(b) if t - b > SLOT_MS + stall_min_ms => b + SLOT_MS,
        _ => NEVER,
    }
}

/// Time given back to a request handled at `t` that waited through the stall begun at `from`.
fn stall_credit(t: i64, from: i64, max_ms: i64) -> i64 {
    if from == NEVER { 0 } else { (t - from).clamp(0, max_ms) }
}

/// Whether an anomaly is a certain cheat (DESIGN 6.5): out of turn and illegal moves only when
/// the client provably knew the position.
fn is_certain(kind: &str, pos_matched: bool) -> bool {
    CERTAIN_KINDS.contains(&kind) && (pos_matched || !matches!(kind, "out_of_turn" | "illegal_move"))
}

/// One game hosted here.
pub(super) struct Entry {
    pub(super) room: GameRoom,
    /// The players' connections (White, Black).
    pub(super) ep: [Option<Endpoint>; 2],
    /// Waiting for its commit.
    pub(super) queued: bool,
    /// In the database (the room stays until its rematch window closes).
    pub(super) committed: bool,
    /// A failed journal write may have lost its `ended` record: journaled again before its commit.
    pub(super) rejournal: bool,
}

/// The sender of a request: where its reply goes and when it arrived.
#[derive(Clone, Debug)]
pub(super) struct Origin {
    pub(super) ep: Option<Endpoint>,
    pub(super) side: Option<Side>,
    pub(super) seq: u32,
    pub(super) timing: Timing,
}

impl Origin {
    /// No sender (timers, server decisions).
    pub(super) fn none(timing: Timing) -> Origin {
        Origin { ep: None, side: None, seq: 0, timing }
    }
}

/// Runs room code, turning a panic into `None` (logged): a bug must not take the host down.
pub(super) fn guarded<R>(log: &Logger, game: GameId, what: &str, f: impl FnOnce() -> R) -> Option<R> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(panic) => {
            log_error!(log, what, { "gameId": game, "err": panic_message(panic.as_ref()) });
            None
        }
    }
}

/// The message of a panic payload.
pub(super) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_owned()
    }
}

/// An `Error` frame.
pub(super) fn error_frame(code: ErrorCode, seq: u32, fatal: bool, game: GameId) -> Option<Bytes> {
    scacelith_protocol::Error { r#ref: seq, code, fatal, game }.to_bytes().ok()
}

/// Sends a frame when there is an endpoint (a closed connection refuses it; nothing to do).
pub(super) fn send(ep: Option<&Endpoint>, frame: &Bytes) {
    if let Some(ep) = ep {
        ep.send(frame.clone());
    }
}

/// The game a client request is about (0 for a message that is not a game request).
fn game_of(msg: &ClientMsg) -> GameId {
    match msg {
        ClientMsg::Move(m) => m.game,
        ClientMsg::Resign(m) => m.game,
        ClientMsg::DrawOffer(m) => m.game,
        ClientMsg::DrawAnswer(m) => m.game,
        ClientMsg::DrawClaim(m) => m.game,
        ClientMsg::Abort(m) => m.game,
        ClientMsg::Resync(m) => m.game,
        ClientMsg::Rematch(m) => m.game,
        _ => 0,
    }
}

/// The games of one shard (see the module documentation).
pub struct Shard {
    pub(super) shard: u32,
    pub(super) settings: ShardSettings,
    pub(super) clock: SharedClock,
    pub(super) store: Arc<dyn GameStore>,
    pub(super) journal: Option<Journal>,
    pub(super) events: Arc<dyn HostEvents>,
    pub(super) anomalies: Arc<dyn AnomalySink>,
    rules: RulesFactory,
    pub(super) log: Logger,
    pub(super) rooms: HashMap<GameId, Entry>,
    /// The running game of each player.
    by_user: HashMap<UserId, GameId>,
    /// Finished games waiting for their commit, in order of end.
    pub(super) pending: IndexSet<GameId>,
    ids: GameIdAllocator,
    timers: TimerSet,
    /// Games in progress.
    active: usize,
    pub(super) meter: Meter,
    shared: Arc<Shared>,
    /// Time of the latest beat (`None`: no stall detection yet, or shut down).
    beat: Option<i64>,
    /// Start of the stall the latest beat detected, until its timers ran.
    stall_from: Option<i64>,
    /// Time of the beat that detected the latest stall.
    last_stall_end: i64,
    // ---- commit pipeline (commit.rs) ----
    pub(super) next_commit_at: i64,
    pub(super) backoff_ms: i64,
    pub(super) commit_in_flight: bool,
    /// `failed_writes()` of the journal when last looked at (see `journal_again`).
    pub(super) journal_failures: u64,
    /// Journal flushes failed in a row before a commit.
    pub(super) gate_failures: u32,
    /// An episode of commits made without waiting for the journal.
    pub(super) unjournaled: bool,
    /// The flush that may end that episode is in flight.
    pub(super) probe_in_flight: bool,
    /// Result of the latest commit attempt (`None` while one is in flight).
    pub(super) last_commit: Option<bool>,
    pub(super) closed: bool,
    /// The flushes, commits and rematch answers waited for.
    pub(super) tasks: JoinSet<Done>,
    task_kinds: HashMap<TaskId, TaskKind>,
    /// Every record appended, for the tests.
    #[cfg(test)]
    pub(super) journal_log: Vec<(GameId, JournalRecord)>,
}

impl std::fmt::Debug for Shard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shard")
            .field("shard", &self.shard)
            .field("games", &self.rooms.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl Shard {
    /// A host without games (call [`Shard::recover`] before anything else).
    ///
    /// # Panics
    ///
    /// When `deps.shard` is not below 64 (checked by the caller).
    pub fn new(deps: ShardDeps) -> Shard {
        let mut ids = GameIdAllocator::new(deps.shard);
        ids.seed(deps.last_game_id);
        let journal_failures = deps.journal.as_ref().map_or(0, Journal::failed_writes);
        Shard {
            shard: deps.shard,
            shared: Arc::new(Shared::new(&deps.settings)),
            settings: deps.settings,
            clock: deps.clock,
            store: deps.store,
            journal: deps.journal,
            events: deps.events,
            anomalies: deps.anomalies,
            rules: deps.rules,
            log: deps.logger,
            rooms: HashMap::new(),
            by_user: HashMap::new(),
            pending: IndexSet::new(),
            ids,
            timers: TimerSet::new(),
            active: 0,
            meter: Meter::new(),
            beat: None,
            stall_from: None,
            last_stall_end: i64::MIN,
            next_commit_at: NEVER,
            backoff_ms: 0,
            commit_in_flight: false,
            journal_failures,
            gate_failures: 0,
            unjournaled: false,
            probe_in_flight: false,
            last_commit: None,
            closed: false,
            tasks: JoinSet::new(),
            task_kinds: HashMap::new(),
            #[cfg(test)]
            journal_log: Vec::new(),
        }
    }

    /// The numbers shared with the handles.
    pub(super) fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// The current time (monotonic ms).
    pub(super) fn now(&self) -> i64 {
        self.clock.now_ms()
    }

    /// The room of a game (tests, admin).
    #[must_use]
    pub fn room(&self, game: GameId) -> Option<&GameRoom> {
        self.rooms.get(&game).map(|e| &e.room)
    }

    /// The running game of a player on this shard.
    #[must_use]
    pub fn active_game_of(&self, user: UserId) -> Option<GameId> {
        self.by_user.get(&user).copied()
    }

    /// The deadline of a game's timer.
    #[must_use]
    pub fn deadline_of(&self, game: GameId) -> Option<i64> {
        self.timers.deadline_of(game)
    }

    /// The connection attached for a player of a game.
    #[must_use]
    pub fn endpoint(&self, game: GameId, side: Side) -> Option<&Endpoint> {
        self.rooms.get(&game).and_then(|e| e.ep[side.index()].as_ref())
    }

    /// The shard's journal.
    #[must_use]
    pub fn journal(&self) -> Option<&Journal> {
        self.journal.as_ref()
    }

    /// Games hosted, running or not.
    #[must_use]
    pub fn games(&self) -> usize {
        self.rooms.len()
    }

    /// Games in progress.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active
    }

    /// Players with a running game here.
    #[must_use]
    pub fn players(&self) -> usize {
        self.by_user.len()
    }

    /// Timers scheduled.
    #[must_use]
    pub fn timers(&self) -> usize {
        self.timers.len()
    }

    /// What the host counted.
    #[must_use]
    pub fn counters(&self) -> &Counters {
        &self.meter.counts
    }

    /// Publishes the load numbers to the handles.
    pub(super) fn publish_load(&self) {
        self.shared.games.store(self.rooms.len(), Ordering::Relaxed);
        self.shared.players.store(self.by_user.len(), Ordering::Relaxed);
    }

    fn publish_stall(&self) {
        self.shared.beat.store(self.beat.unwrap_or(i64::MIN), Ordering::Relaxed);
        self.shared.stall_from.store(self.stall_from.unwrap_or(NEVER), Ordering::Relaxed);
        self.shared.last_stall_end.store(self.last_stall_end, Ordering::Relaxed);
    }

    // ---- games --------------------------------------------------------------------------------

    /// Creates a game; its timers start now. `rated` holds only for an official category.
    ///
    /// # Errors
    ///
    /// `Internal` when the room cannot be built (a bug), `ShuttingDown` once shut down.
    pub fn create(&mut self, game: NewGame) -> Result<GameId, ErrorCode> {
        if self.closed {
            return Err(ErrorCode::ShuttingDown);
        }
        let t = self.now();
        let id = self.ids.next(t);
        if self.rooms.contains_key(&id) {
            log_error!(self.log, "game id given twice", { "gameId": id });
            return Err(ErrorCode::Internal);
        }
        let rated = game.rated && self.settings.categories.contains(&game.category);
        let spec = RoomSpec {
            id,
            category: game.category,
            base_ms: game.base_ms,
            inc_ms: game.inc_ms,
            rated,
            white: game.white,
            black: game.black,
            created_at: t,
            rematch_of: game.rematch_of.unwrap_or(0),
            auto_press: game.auto_press,
        };
        let (settings, rules) = (self.settings.room, &self.rules);
        let built = guarded(&self.log, id, "game creation failed", || {
            GameRoom::new(spec, settings, rules()).map(|room| {
                let created = room.created_record();
                (room, created)
            })
        });
        let (room, created) = match built {
            Some(Ok(built)) => built,
            Some(Err(e)) => {
                log_error!(self.log, "game creation failed", { "gameId": id, "err": e.to_string() });
                return Err(ErrorCode::Internal);
            }
            None => return Err(ErrorCode::Internal),
        };
        for side in Side::BOTH {
            self.by_user.insert(room.player(side).user_id, id);
        }
        self.rooms
            .insert(id, Entry { room, ep: [None, None], queued: false, committed: false, rejournal: false });
        self.active += 1;
        self.meter.created();
        self.meter.game_started();
        self.append(id, created);
        self.reschedule(id);
        Ok(id)
    }

    /// Binds a player's connection to a game and sends it the snapshot; `false` (and
    /// `Error{NotInGame}`) when the game is unknown or the user does not play it.
    pub fn attach(&mut self, game: GameId, user: UserId, ep: Endpoint) -> bool {
        let Some(side) = self.rooms.get(&game).and_then(|e| e.room.side_of(user)) else {
            if let Some(frame) = error_frame(ErrorCode::NotInGame, 0, false, game) {
                ep.send(frame);
            }
            return false;
        };
        let (timing, _) = self.credited(self.now());
        self.bind(game, side, ep, true, timing);
        true
    }

    /// A player's connection closed; ignored when another connection replaced it (`conn` is the
    /// closed one's id; `None`: whichever is attached).
    pub fn detach(&mut self, game: GameId, user: UserId, conn: Option<ConnId>) -> bool {
        let (timing, _) = self.credited(self.now());
        let Some(entry) = self.rooms.get_mut(&game) else { return false };
        let Some(side) = entry.room.side_of(user) else { return false };
        let slot = &mut entry.ep[side.index()];
        if let (Some(cur), Some(conn)) = (slot.as_ref(), conn)
            && cur.conn_id() != conn
        {
            return false;
        }
        *slot = None;
        if let Some(out) =
            guarded(&self.log, game, "game disconnection failed", || entry.room.on_disconnect(side, timing))
        {
            self.process(game, out, &Origin::none(timing));
        } else {
            self.reschedule(game);
        }
        true
    }

    /// A strictly decoded game request (Move .. Rematch) of `user`, read from its connection at
    /// `recv_at` (monotonic ms). Anything else gets `Error{NotInGame}`, as does a request for a
    /// game not hosted here; a request for a game the user does not play is also a
    /// `foreign_game` anomaly. The request counts as arrived at its read time or, after a stall of
    /// the actor, at the start of the stall (`GAME_STALL_CREDIT_MAX_MS` earlier at most),
    /// whichever is earlier.
    pub fn client(&mut self, user: UserId, msg: &ClientMsg, ep: Option<Endpoint>, recv_at: f64) {
        let started = Instant::now();
        let seq = msg.seq();
        let game = game_of(msg);
        let t = self.now();
        let Some(entry) = self.rooms.get(&game) else {
            self.reject(ep.as_ref(), ErrorCode::NotInGame, seq, game);
            return;
        };
        let Some(side) = entry.room.side_of(user) else {
            self.reject(ep.as_ref(), ErrorCode::NotInGame, seq, game);
            let detail = format!("message type {}", msg.msg_type().to_u8());
            self.handle_anomaly(None, user, game, "foreign_game", detail, false, ep, seq, Timing::at(t));
            return;
        };
        let (mut timing, credit) = self.credited(t);
        if recv_at.is_finite() {
            timing.recv_at = timing.recv_at.min(recv_at.floor() as i64);
        }
        if let Some(ep) = &ep {
            if entry.ep[side.index()].is_none() {
                self.bind(game, side, ep.clone(), false, timing);
            } else if ep.rtt_ms() > 0
                && let Some(entry) = self.rooms.get_mut(&game)
            {
                entry.room.on_rtt(side, f64::from(ep.rtt_ms()));
            }
        }
        let Some(entry) = self.rooms.get_mut(&game) else { return };
        let room = &mut entry.room;
        let out = guarded(&self.log, game, "game message failed", || match msg {
            ClientMsg::Move(m) => Some(room.on_move(side, m, timing)),
            ClientMsg::Resign(_) => Some(room.on_resign(side, seq, timing)),
            ClientMsg::DrawOffer(_) => Some(room.on_draw_offer(side, seq, timing)),
            ClientMsg::DrawAnswer(m) => Some(room.on_draw_answer(side, m.accept, seq, timing)),
            ClientMsg::DrawClaim(_) => Some(room.on_draw_claim(side, seq, timing)),
            ClientMsg::Abort(_) => Some(room.on_abort(side, seq, timing)),
            ClientMsg::Resync(_) => Some(room.on_resync(side, timing)),
            ClientMsg::Rematch(m) => Some(room.on_rematch(side, m.accept, seq, timing)),
            _ => None,
        });
        let out = match out {
            Some(Some(out)) => out,
            Some(None) => return self.reject(ep.as_ref(), ErrorCode::NotInGame, seq, game),
            None => {
                self.reject(ep.as_ref(), ErrorCode::Internal, seq, game);
                return self.reschedule(game);
            }
        };
        let moved = out.moved;
        self.process(game, out, &Origin { ep, side: Some(side), seq, timing });
        if credit > 0 && !matches!(msg, ClientMsg::Resync(_) | ClientMsg::Rematch(_)) {
            self.meter.stall_credit(credit);
        }
        if matches!(msg, ClientMsg::Move(_)) {
            if moved {
                self.meter.moved();
            }
            self.meter.move_timed(started.elapsed().as_secs_f64() * 1e6);
        }
    }

    /// The player joined a queue: its finished game's rematch window closes, as with a
    /// `Rematch{accept: false}` (DESIGN 6.3). No reply, and no refusal is counted.
    pub fn decline_rematch(&mut self, game: GameId, user: UserId) {
        let (timing, _) = self.credited(self.now());
        let Some(entry) = self.rooms.get_mut(&game) else { return };
        let Some(side) = entry.room.side_of(user) else { return };
        let Some(mut out) = guarded(&self.log, game, "rematch decline failed", || {
            entry.room.on_rematch(side, false, 0, timing)
        }) else {
            return self.reschedule(game);
        };
        out.rejected = None;
        self.process(game, out, &Origin { ep: None, side: Some(side), seq: 0, timing });
    }

    /// Relays a player's gesture (the raw `C_Gesture` frame) to the opponent as a droppable
    /// `S_Gesture`. Cosmetic: nothing is journaled, timed or checked beyond decoding; dropped
    /// without an anomaly when it cannot be relayed. Returns whether it was queued.
    pub fn gesture(&mut self, game: GameId, user: UserId, frame: &[u8]) -> bool {
        let dropped = match self.rooms.get(&game) {
            None => GestureDrop::NoGame,
            Some(entry) => match entry.room.side_of(user) {
                None => GestureDrop::NotPlayer,
                Some(side) => match &entry.ep[side.opponent().index()] {
                    None => GestureDrop::NoOpponent,
                    Some(ep) => match ClientGesture::relay_frame(frame) {
                        Err(_) => GestureDrop::Malformed,
                        Ok(out) if ep.send_droppable(Bytes::copy_from_slice(&out)) => {
                            self.meter.gesture_relayed();
                            return true;
                        }
                        Ok(_) => GestureDrop::Backlog,
                    },
                },
            },
        };
        self.meter.gesture_dropped(dropped);
        false
    }

    /// A round-trip measurement of a player (it moves the deadlines).
    pub fn rtt(&mut self, game: GameId, user: UserId, rtt_ms: u32) {
        let Some(entry) = self.rooms.get_mut(&game) else { return };
        let Some(side) = entry.room.side_of(user) else { return };
        entry.room.on_rtt(side, f64::from(rtt_ms));
        self.reschedule(game);
    }

    /// Ends the running game of `user` on this shard as a forfeit (a sanction from elsewhere).
    pub fn forfeit_user(&mut self, user: UserId) -> bool {
        // Like a game request, the sanction may have waited during a stall.
        let (timing, _) = self.credited(self.now());
        let Some(&game) = self.by_user.get(&user) else { return false };
        let Some(entry) = self.rooms.get_mut(&game) else { return false };
        let Some(side) = entry.room.side_of(user) else { return false };
        if entry.room.is_over() {
            return false;
        }
        if let Some(out) = guarded(&self.log, game, "forfeit failed", || entry.room.forfeit(side, timing)) {
            self.process(game, out, &Origin::none(timing));
        }
        true
    }

    /// Ends a game the lobby gave up creating: `ServerAborted`, no conduct incident.
    pub fn cancel(&mut self, game: GameId) -> bool {
        let now = self.now();
        let Some(entry) = self.rooms.get_mut(&game) else { return false };
        if entry.room.is_over() {
            return false;
        }
        if let Some(out) = guarded(&self.log, game, "game cancel failed", || entry.room.server_abort(now)) {
            self.process(game, out, &Origin::none(Timing::at(now)));
        }
        true
    }

    // ---- time ---------------------------------------------------------------------------------

    /// The timing of a request handled at `t`, with the stall credit it gets.
    fn credited(&self, t: i64) -> (Timing, i64) {
        let from = self.stall_start(t);
        let credit = self.stall_credit(t, from);
        (Timing { now: t, recv_at: t - credit, stalled_since: from }, credit)
    }

    /// Start of the stall a request handled at `t` waited through ([`NEVER`]: none).
    #[must_use]
    pub fn stall_start(&self, t: i64) -> i64 {
        stall_start(self.beat, self.stall_from, t, self.settings.stall_min_ms)
    }

    /// Time given back to a request handled at `t` (0 without a stall), from the start `from` of
    /// the stall it waited through; `GAME_STALL_CREDIT_MAX_MS` at most.
    #[must_use]
    pub fn stall_credit(&self, t: i64, from: i64) -> i64 {
        stall_credit(t, from, self.settings.stall_credit_max_ms)
    }

    /// Whether a stall of the actor overlaps the time since `t0` (a round trip measured over it
    /// includes the stall).
    #[must_use]
    pub fn stall_during(&self, t0: i64, t: i64) -> bool {
        self.last_stall_end > t0 || self.stall_credit(t, self.stall_start(t)) > 0
    }

    /// Fires, at `t`, the timers due at `due_by` (`t` at most); `stalled_since` is the start of
    /// the stall they ran late for. Returns the number fired.
    pub fn run_timers(&mut self, t: i64, stalled_since: i64, due_by: i64) -> usize {
        let mut fired = 0;
        for (game, deadline) in self.timers.due(due_by.min(t)) {
            if !self.timers.claim(game) {
                continue;
            }
            let Some(entry) = self.rooms.get_mut(&game) else { continue };
            fired += 1;
            self.meter.timer_fired(t - deadline);
            let timing = Timing::at(t).stalled_since(stalled_since);
            match guarded(&self.log, game, "game tick failed", || entry.room.tick(timing)) {
                Some(out) => self.process(game, out, &Origin::none(timing)),
                // A bug must not leave the game without a timer: try again in a second.
                None => self.timers.schedule(game, Some(t + 1000)),
            }
        }
        fired
    }

    /// One beat at `t`: the timers, commits and compaction, unless the beat reveals a stall
    /// (then the actor first reads what waited in its inbox, and calls [`Shard::after_stall`]).
    /// Returns whether a stall was detected.
    pub fn heartbeat(&mut self, t: i64) -> bool {
        let prev = self.beat.replace(t);
        if let Some(prev) = prev
            && t - prev > SLOT_MS + self.settings.stall_min_ms
        {
            self.meter.stall(t - prev - SLOT_MS);
            self.last_stall_end = t;
            if self.stall_from.is_none() {
                self.stall_from = Some(prev + SLOT_MS);
            }
            self.publish_stall();
            return true;
        }
        self.publish_stall();
        if self.stall_from.is_none() {
            self.tick(t, NEVER, t);
        }
        false
    }

    /// The timers, commits and compaction after a detected stall, once the requests that waited
    /// through it are handled: only the deadlines due by the beat that detected it fire now (a
    /// later one waits for the next beat), and they know when the stall began.
    pub fn after_stall(&mut self) {
        let Some(from) = self.stall_from.take() else { return };
        self.publish_stall();
        if !self.closed {
            let t = self.now();
            let due_by = self.beat.unwrap_or(t);
            self.tick(t, from, due_by);
        }
    }

    /// Stops the stall detection (shutdown).
    pub(super) fn stop_beat(&mut self) {
        self.beat = None;
        self.stall_from = None;
        self.publish_stall();
    }

    /// Whether a detected stall waits for [`Shard::after_stall`].
    #[must_use]
    pub fn stall_pending(&self) -> bool {
        self.stall_from.is_some()
    }

    fn tick(&mut self, t: i64, stalled_since: i64, due_by: i64) {
        self.run_timers(t, stalled_since, due_by);
        self.poll_commits(t);
        self.compact(t);
    }

    /// Journal compaction: appends a snapshot of each game the journal asks for that is hosted
    /// here and not committed yet. Returns the number of snapshots appended.
    pub fn compact(&mut self, t: i64) -> usize {
        let Some(journal) = &self.journal else { return 0 };
        let mut n = 0;
        for game in journal.compaction_candidates(SNAPSHOTS_PER_TICK) {
            let Some(entry) = self.rooms.get(&game) else { continue };
            if entry.committed {
                continue;
            }
            let Some(rec) =
                guarded(&self.log, game, "journal snapshot failed", || entry.room.journal_snapshot(t))
            else {
                continue;
            };
            self.append(game, rec);
            n += 1;
        }
        self.meter.counts.snapshots += n as u64;
        n
    }

    // ---- recovery -----------------------------------------------------------------------------

    /// Replays the journal (start-up, before anything else): running games are restored with
    /// the recovery grace and their clock held, ended ones are queued for their commit, games
    /// that cannot be fully replayed end `ServerAborted`, games that cannot be rebuilt at all are
    /// dropped. Returns the number of games taken back.
    pub fn recover(&mut self) -> usize {
        let Some(journal) = self.journal.as_mut() else { return 0 };
        let games = journal.take_recovered();
        let t = self.now();
        let mut count = 0;
        for (game, records) in games {
            // Never given again, even with the clock behind.
            self.ids.seed(game);
            if self.rooms.contains_key(&game) {
                continue;
            }
            let (settings, rules) = (self.settings.room, &self.rules);
            let replay = |strict| {
                guarded(&self.log, game, "journal replay failed", || {
                    GameRoom::from_journal(&records, settings, rules(), strict)
                })
            };
            let (room, broken) = match replay(true) {
                Some(Ok(room)) => (Some(room), None),
                failed => {
                    let why = match failed {
                        Some(Err(e)) => e.to_string(),
                        _ => "panic".to_owned(),
                    };
                    (replay(false).and_then(Result::ok), Some(why))
                }
            };
            drop(records);
            let Some(room) = room else {
                log_error!(self.log, "journal: game cannot be rebuilt, dropped", { "gameId": game, "err": broken });
                self.journal_committed(game);
                continue;
            };
            count += 1;
            let over = room.is_over();
            let entry = self.rooms.entry(game).or_insert(Entry {
                room,
                ep: [None, None],
                queued: false,
                committed: false,
                rejournal: false,
            });
            if over {
                // Only its rematch window closes; the commit follows.
                let _ = guarded(&self.log, game, "game recovery failed", || entry.room.recover(t));
                let reason = entry.room.result().map(|r| r.reason);
                self.meter.counts.requeued += 1;
                self.queue_commit(game);
                if let Some(reason) = reason {
                    self.meter.ended(reason);
                }
                continue;
            }
            self.active += 1;
            self.meter.game_started();
            let timing = Timing::at(t);
            if let Some(why) = broken {
                log_warn!(self.log, "journal: game replayed partially, aborted", { "gameId": game, "err": why });
                self.meter.counts.aborted += 1;
                let out = entry.room.server_abort(t);
                self.process(game, out, &Origin::none(timing));
                continue;
            }
            let out = match guarded(&self.log, game, "game recovery failed", || entry.room.recover(t)) {
                Some(out) => out,
                None => entry.room.server_abort(t),
            };
            self.process(game, out, &Origin::none(timing));
            let Some(entry) = self.rooms.get(&game) else { continue };
            if entry.room.is_over() {
                continue;
            }
            let (white, black) =
                (entry.room.player(Side::White).user_id, entry.room.player(Side::Black).user_id);
            self.by_user.insert(white, game);
            self.by_user.insert(black, game);
            self.meter.counts.recovered += 1;
            self.events.game_recovered(game, white, black);
        }
        if let Some(journal) = self.journal.as_mut() {
            journal.release_recovered();
        }
        let c = &self.meter.counts;
        log_info!(self.log, "games recovered from the journal", {
            "shard": self.shard,
            "restored": c.recovered,
            "requeued": c.requeued,
            "aborted": c.aborted,
        });
        count
    }

    // ---- internals ----------------------------------------------------------------------------

    /// Binds `ep` as the connection of `side`, then reconnects the player (or processes the
    /// deadlines due at its arrival when it was connected) and sends the snapshot if asked.
    fn bind(&mut self, game: GameId, side: Side, ep: Endpoint, snapshot: bool, timing: Timing) {
        let Some(entry) = self.rooms.get_mut(&game) else { return };
        entry.ep[side.index()] = Some(ep.clone());
        if ep.rtt_ms() > 0 {
            entry.room.on_rtt(side, f64::from(ep.rtt_ms()));
        }
        let room = &mut entry.room;
        let out = guarded(&self.log, game, "game reconnection failed", || {
            if room.is_connected(side) {
                room.tick(Timing::at(timing.recv_at).stalled_since(timing.stalled_since))
            } else {
                room.on_reconnect(side, timing)
            }
        });
        match out {
            Some(out) => {
                self.process(game, out, &Origin { ep: Some(ep.clone()), side: Some(side), seq: 0, timing });
            }
            None => self.reschedule(game),
        }
        if snapshot
            && let Some(entry) = self.rooms.get(&game)
            && let Some(frame) =
                guarded(&self.log, game, "snapshot failed", || entry.room.snapshot_frame(side, timing.now))
        {
            ep.send(frame);
        }
    }

    /// Refuses a request (`Error`, not fatal) and counts it.
    fn reject(&mut self, ep: Option<&Endpoint>, code: ErrorCode, seq: u32, game: GameId) {
        self.meter.rejected(code);
        if let Some(frame) = error_frame(code, seq, false, game) {
            send(ep, &frame);
        }
    }

    /// Appends a record to the journal (a failure is logged: the commit gate deals with lost
    /// records).
    pub(super) fn append(&mut self, game: GameId, rec: JournalRecord) {
        if let Some(journal) = &self.journal
            && let Err(e) = journal.append(rec.kind, game, &rec.payload, rec.at as f64)
        {
            log_error!(self.log, "journal append failed", { "gameId": game, "kind": rec.kind.as_u8(), "err": log::error(&e) });
        }
        #[cfg(test)]
        self.journal_log.push((game, rec));
    }

    /// Appends a game's `committed` record.
    pub(super) fn journal_committed(&mut self, game: GameId) {
        if let Some(journal) = &self.journal
            && let Err(e) = journal.committed(game)
        {
            log_error!(self.log, "journal.committed failed", { "gameId": game, "err": log::error(&e) });
        }
        #[cfg(test)]
        self.journal_log.push((
            game,
            JournalRecord { kind: crate::journal::RecordKind::Committed, at: 0, payload: Vec::new() },
        ));
    }

    /// Delivers an outcome and applies its side effects. `from`: the request's sender and
    /// arrival, which the forfeit of a certain cheat it revealed keeps.
    pub(super) fn process(&mut self, game: GameId, out: Outcome, from: &Origin) {
        let Some(entry) = self.rooms.get(&game) else { return };
        let eps = entry.ep.clone();
        let anomaly_user = out.anomaly.as_ref().map(|a| entry.room.player(a.side).user_id);
        for frame in &out.broadcast {
            send(eps[0].as_ref(), frame);
            send(eps[1].as_ref(), frame);
        }
        if let Some(ep) = &from.ep {
            for frame in &out.reply {
                ep.send(frame.clone());
            }
        }
        for rec in out.journal {
            self.append(game, rec);
        }
        for (user, kind) in out.conduct {
            self.events.conduct(user, kind);
        }
        if let Some(code) = out.rejected {
            self.meter.rejected(code);
        }
        if out.ended {
            self.on_ended(game);
        }
        if let Some(spec) = out.rematch {
            self.request_rematch(spec);
        }
        if let Some(side) = out.clock_started
            && !out.moved
            && let Some(entry) = self.rooms.get(&game)
            && !entry.room.is_over()
        {
            // A clock held since a recovery started: the opponent's display shows it stopped.
            let (opp, now) = (side.opponent(), self.now());
            if let Some(ep) = &entry.ep[opp.index()]
                && let Some(frame) =
                    guarded(&self.log, game, "snapshot failed", || entry.room.snapshot_frame(opp, now))
            {
                ep.send(frame);
            }
        }
        self.reschedule(game);
        if let (Some(a), Some(user)) = (out.anomaly, anomaly_user) {
            let own = from.side == Some(a.side);
            let ep = if own { from.ep.clone() } else { eps[a.side.index()].clone() };
            let seq = if own { from.seq } else { 0 };
            self.handle_anomaly(
                Some(a.side),
                user,
                game,
                a.kind,
                a.detail,
                a.pos_matched,
                ep,
                seq,
                from.timing,
            );
        }
    }

    /// Reports an anomaly; a certain cheat (with `AUTO_SANCTION_CERTAIN_CHEATS`) forfeits the
    /// cheater's game at the arrival of the request that revealed it, gets a fatal
    /// `Error{CheatDetected}`, the sanction and the connection closed with 4302.
    #[allow(clippy::too_many_arguments)]
    fn handle_anomaly(
        &mut self,
        side: Option<Side>,
        user: UserId,
        game: GameId,
        kind: &'static str,
        detail: String,
        pos_matched: bool,
        ep: Option<Endpoint>,
        seq: u32,
        timing: Timing,
    ) {
        self.anomalies.record(Anomaly { user, game, kind, detail: json!({ "info": detail }), pos_matched });
        if !self.settings.auto_sanction || !is_certain(kind, pos_matched) {
            return;
        }
        if let Some(side) = side
            && let Some(entry) = self.rooms.get_mut(&game)
            && !entry.room.is_over()
            && let Some(out) = guarded(&self.log, game, "forfeit failed", || entry.room.forfeit(side, timing))
        {
            self.process(game, out, &Origin::none(timing));
        }
        if let Some(frame) = error_frame(ErrorCode::CheatDetected, seq, true, game) {
            send(ep.as_ref(), &frame);
        }
        self.anomalies.sanction_certain(user, game, kind);
        if let Some(ep) = ep {
            ep.close(close::CHEAT_DETECTED, "cheat detected");
        }
    }

    fn on_ended(&mut self, game: GameId) {
        let Some(entry) = self.rooms.get(&game) else { return };
        let users = Side::BOTH.map(|s| entry.room.player(s).user_id);
        let reason = entry.room.result().map(|r| r.reason);
        self.active = self.active.saturating_sub(1);
        self.meter.counts.ended += 1;
        self.meter.game_finished();
        for user in users {
            if self.by_user.get(&user) == Some(&game) {
                self.by_user.remove(&user);
            }
        }
        if let Some(reason) = reason {
            self.meter.ended(reason);
        }
        self.queue_commit(game);
    }

    /// Both players want a rematch: the lobby creates it or answers with the error to report.
    fn request_rematch(&mut self, spec: RematchSpec) {
        let game = spec.game;
        let (reply, answer) = oneshot::channel();
        let request = RematchRequest {
            game: NewGame {
                category: spec.category,
                base_ms: spec.base_ms,
                inc_ms: spec.inc_ms,
                rated: spec.rated,
                white: spec.white,
                black: spec.black,
                created_at: self.clock.wall_ms(),
                rematch_of: Some(game),
                auto_press: spec.auto_press,
            },
        };
        self.events.rematch(request, reply);
        self.spawn(TaskKind::Rematch(game), async move { Done::Rematch { game, answer: answer.await.ok() } });
    }

    /// The lobby's answer to a rematch request: on a refusal both players get the error.
    pub(super) fn rematch_answered(&mut self, game: GameId, answer: Option<Result<GameId, ErrorCode>>) {
        let code = match answer {
            Some(Ok(_)) => return,
            Some(Err(code)) => code,
            None => ErrorCode::RematchUnavailable,
        };
        log_info!(self.log, "rematch refused", { "gameId": game, "why": code.name() });
        if let Some(entry) = self.rooms.get(&game)
            && let Some(frame) = error_frame(code, 0, false, game)
        {
            send(entry.ep[0].as_ref(), &frame);
            send(entry.ep[1].as_ref(), &frame);
        }
    }

    /// (Re)schedules a game's timer at its next deadline; a committed game without deadline left
    /// (its rematch window closed) is removed.
    pub(super) fn reschedule(&mut self, game: GameId) {
        let Some(entry) = self.rooms.get(&game) else { return };
        match entry.room.next_deadline() {
            Some(d) => self.timers.schedule(game, Some(d)),
            None => {
                self.timers.cancel(game);
                if entry.committed && entry.room.is_over() {
                    self.rooms.remove(&game);
                }
            }
        }
    }

    // ---- tasks --------------------------------------------------------------------------------

    /// Runs a task whose result comes back to [`Shard::on_task`].
    pub(super) fn spawn(&mut self, kind: TaskKind, task: impl Future<Output = Done> + Send + 'static) {
        let handle = self.tasks.spawn(task);
        self.task_kinds.insert(handle.id(), kind);
    }

    /// Whether tasks are in flight.
    #[must_use]
    pub fn has_tasks(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// Waits for the next task to finish and applies its result; `false` when none was in
    /// flight.
    pub async fn next_task(&mut self) -> bool {
        match self.tasks.join_next_with_id().await {
            Some(result) => {
                self.on_task(result);
                true
            }
            None => false,
        }
    }

    /// Applies the result of a task (or its failure: a panic in a task is a bug; its effect is
    /// undone so that the host goes on).
    pub(super) fn on_task(&mut self, result: Result<(TaskId, Done), JoinError>) {
        match result {
            Ok((id, done)) => {
                self.task_kinds.remove(&id);
                self.on_done(done);
            }
            Err(e) => {
                let kind = self.task_kinds.remove(&e.id());
                log_error!(self.log, "game host task failed", { "shard": self.shard, "err": e.to_string() });
                match kind {
                    Some(TaskKind::Commit) => {
                        self.commit_in_flight = false;
                        let now = self.now();
                        self.commit_failed(&CommitError::TaskFailed, self.pending.len(), now);
                        self.last_commit = Some(false);
                    }
                    Some(TaskKind::Probe) => self.probe_in_flight = false,
                    Some(TaskKind::Rematch(game)) => self.rematch_answered(game, None),
                    None => {}
                }
            }
        }
    }

    /// Waits for every task in flight (shutdown, tests).
    pub async fn drain_tasks(&mut self) {
        while self.next_task().await {}
    }
}
