//! `GameRoom`: one authoritative online game (DESIGN 5.3, 6.1 to 6.5).
//!
//! The room is deterministic: time is always passed in (integer epoch milliseconds of the host's
//! monotonic clock), it owns no timer and does no I/O. Every entry point first processes the
//! deadlines due at its time (flag, first-move timeout, grace expiry, rematch window), so
//! simultaneous events resolve by time: a resignation or a move arriving after the flag deadline
//! finds the game already lost on time.
//!
//! Each call returns an [`Outcome`]: the frames to broadcast to both players and to reply to the
//! sender, an anomaly, the journal records to append, conduct incidents, a rematch to create,
//! and flags for the host's metrics. A frame is encoded once (the `MoveMade` of a move is one
//! buffer for both players). `gseq` increments on every broadcast event (`MoveMade`, `GameEvent`,
//! `GameEnd`), by [`RECOVERY_GSEQ_JUMP`] at a recovery, and never otherwise.
//!
//! # Time
//!
//! A request carries a [`Timing`]: the real time `now`, the credited arrival `recv_at` (the
//! moment the host counts it as arrived when its actor stalled while the request waited) and
//! `stalled_since` (the start of that stall). The deadlines processed first, the flag check and
//! the time charged for a move use the credited arrival (never before the latest move, never
//! after `now`); the next turn starts at `now` all the same, so a stall is charged to nobody.
//! A resignation, a draw agreement or claim, an abort and a forfeit take effect at the arrival;
//! a disconnection, a reconnection, a resync and a rematch request happen at `now` but process
//! the deadlines due at the arrival only, so that what waited in the sockets during a stall never
//! lets a deadline that fell during the stall overtake a request that waited with it. A first-move
//! timeout that fell at or after `stalled_since` aborts the game without a `noshow` incident.
//!
//! # Restarts
//!
//! [`GameRoom::from_journal`] rebuilds a room from its journal records (see [`journal`]);
//! [`GameRoom::recover`] then applies the restart semantics of DESIGN 6.4: both players are away
//! with the recovery grace, and the clock (or first-move timer) of the side to move is held until
//! that player is back, `RECOVERY_CLOCK_HOLD_MS` at most. When the hold ends without its player,
//! the room journals a checkpoint and reports [`Outcome::clock_started`]. In a game restored
//! before its second ply, the first reconnection of each player since the recovery restarts its
//! first-move timer when it is that player's turn. A restored game aborted `NoShow` while the
//! side to move is still away records no `noshow` incident: the server broke the connection.
//!
//! Other rules worth knowing: a draw offer made while the opponent's offer is pending is an
//! agreement; a game reaching [`MAX_PLIES`] ends `ServerAborted`; nothing is journaled after the
//! `ended` record except compaction snapshots (presence and rematch changes after the end are
//! not journaled); the round-trip averages are not journaled.

pub mod journal;

#[cfg(test)]
mod journal_tests;
#[cfg(test)]
mod real_rules_tests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;

use bytes::Bytes;
use scacelith_protocol::{
    EndReason, ErrorCode, GameEnd, GameEvent, GameEventKind, GameSnapshot, GameStatus, Message, MoveMade,
    MoveRec, MoveRejected, PlayerInfo,
};

use crate::config::Config;
use crate::events::IncidentKind;
use crate::ids::{self, GameId, UserId};

use super::clock::{ClockPolicy, GameClock, GracePolicy, IMPLAUSIBLE_MARGIN_MS};
use super::rules::{Rules, Side, color_of, win_for};

pub use self::journal::{JournalError, JournalRecord};

/// A rematch can be agreed during this long after the end.
pub const REMATCH_WINDOW_MS: i64 = 60000;
/// A declined draw offer cannot be repeated before this many plies.
pub const DRAW_REOFFER_PLIES: i32 = 10;
/// Both players leaving within this interval (inclusive) is a network or server event.
pub const BOTH_DISCONNECT_WINDOW_MS: i64 = 5000;
/// Longest game: `Move.ply` is at most 1199 and a snapshot holds at most 1200 moves.
pub const MAX_PLIES: usize = scacelith_protocol::MAX_PLIES;
/// `gseq` jump at a server restart (events of the last journal flush may have been lost).
pub const RECOVERY_GSEQ_JUMP: u32 = 256;
/// Anomaly kinds that are certain cheats when the position was synchronised (DESIGN 6.5); the
/// host uses them since the anomaly sink returns no verdict.
pub const CERTAIN_KINDS: [&str; 3] = ["foreign_game", "out_of_turn", "illegal_move"];

/// No deadline (also "no stall" for [`Timing::stalled_since`]).
pub const NEVER: i64 = i64::MAX;

/// Bits of [`GameRecord::flags`] (finished game record, DESIGN 5.5).
pub mod record_flag {
    /// The game was created rated.
    pub const RATED_REQUESTED: u32 = 1;
    /// The game was restored from the journal after a restart.
    pub const RECOVERED: u32 = 2;
    /// A player forfeited (certain cheat).
    pub const FORFEIT: u32 = 4;
    /// The players pressed the clock themselves (`autoPress` false).
    pub const MANUAL_PRESS: u32 = 8;
}

// Bits of a move record: a draw offer made with the move, the move declined the opponent's offer.
const MB_OFFER: u8 = 1;
const MB_DECLINED: u8 = 2;

/// When a room call happens (see the module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Real time of the call.
    pub now: i64,
    /// Credited arrival of the request (`now` without a stall).
    pub recv_at: i64,
    /// Start of the stall the request waited through ([`NEVER`]: none).
    pub stalled_since: i64,
}

impl Timing {
    /// A call at `now`, without stall credit.
    #[must_use]
    pub const fn at(now: i64) -> Timing {
        Timing { now, recv_at: now, stalled_since: NEVER }
    }

    /// A call at `now` for a request credited as arrived at `recv_at`.
    #[must_use]
    pub const fn credited(now: i64, recv_at: i64) -> Timing {
        Timing { now, recv_at, stalled_since: NEVER }
    }

    /// The same call with the start of the stall it waited through.
    #[must_use]
    pub const fn stalled_since(self, since: i64) -> Timing {
        Timing { stalled_since: since, ..self }
    }
}

impl From<i64> for Timing {
    fn from(now: i64) -> Timing {
        Timing::at(now)
    }
}

/// Settings of every room of a server (from the configuration).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoomSettings {
    /// Clock settings.
    pub clock: ClockPolicy,
    /// Reconnection graces and the recovery hold.
    pub grace: GracePolicy,
    /// `DRAW_OFFERS_PER_GAME`.
    pub draw_offers_per_game: u32,
}

impl Default for RoomSettings {
    fn default() -> Self {
        RoomSettings { clock: ClockPolicy::default(), grace: GracePolicy::default(), draw_offers_per_game: 3 }
    }
}

impl RoomSettings {
    /// The room settings of the configuration.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        RoomSettings {
            clock: ClockPolicy::from_config(config),
            grace: GracePolicy::from_config(config),
            draw_offers_per_game: u32::try_from(config.draw_offers_per_game.max(0)).unwrap_or(u32::MAX),
        }
    }
}

/// What a room is created from.
#[derive(Clone, Debug, PartialEq)]
pub struct RoomSpec {
    /// Game id (id53, non-zero).
    pub id: GameId,
    /// Official category id (`"3+2"`) or `"custom"` (at most 7 bytes are kept).
    pub category: String,
    /// Base time.
    pub base_ms: u32,
    /// Increment.
    pub inc_ms: u32,
    /// Rated game.
    pub rated: bool,
    /// White player (the name is normalised: no NUL, at most 24 bytes, `?` when empty).
    pub white: PlayerInfo,
    /// Black player.
    pub black: PlayerInfo,
    /// Start of the game: White's first-move timer starts here.
    pub created_at: i64,
    /// The game this one is a rematch of (0: none).
    pub rematch_of: GameId,
    /// The robots press the clock by themselves (`GameSnapshot.autoPress`).
    pub auto_press: bool,
}

/// Why a room could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomError {
    /// The game id is zero or not below 2^53.
    InvalidGameId(GameId),
    /// The journal records cannot be replayed.
    Journal(JournalError),
}

impl std::fmt::Display for RoomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoomError::InvalidGameId(id) => write!(f, "invalid game id {id}"),
            RoomError::Journal(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for RoomError {}

impl From<JournalError> for RoomError {
    fn from(e: JournalError) -> Self {
        RoomError::Journal(e)
    }
}

/// An anomaly revealed by a request (DESIGN 6.5). The host reports it with the detail as
/// `{"info": detail}`, as the reference anti-cheat stored the room's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomAnomaly {
    /// The player concerned.
    pub side: Side,
    /// Stored kind (`clock_implausible`, `out_of_turn`, `desync`...).
    pub kind: &'static str,
    /// Human-readable detail (`ply 3 move 1234 (game at ply 3)`).
    pub detail: String,
    /// The client's position hash matched the server's.
    pub pos_matched: bool,
}

/// Both players accepted a rematch: the new game, colours swapped.
#[derive(Clone, Debug, PartialEq)]
pub struct RematchSpec {
    /// The finished game.
    pub game: GameId,
    /// White of the new game (Black of the finished one).
    pub white: PlayerInfo,
    /// Black of the new game.
    pub black: PlayerInfo,
    /// Category of the finished game.
    pub category: String,
    /// Base time.
    pub base_ms: u32,
    /// Increment.
    pub inc_ms: u32,
    /// The finished game was created rated.
    pub rated: bool,
    /// The finished game's `autoPress`.
    pub auto_press: bool,
}

/// Result of one room call (see the module documentation). The host sends `broadcast` to both
/// players, then `reply` to the sender.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// Frames for both players, in order.
    pub broadcast: Vec<Bytes>,
    /// Frames for the sender, after the broadcast.
    pub reply: Vec<Bytes>,
    /// An anomaly to report.
    pub anomaly: Option<RoomAnomaly>,
    /// The game ended during this call.
    pub ended: bool,
    /// Records to append to the journal, in order.
    pub journal: Vec<JournalRecord>,
    /// Conduct incidents (abandon, abort, no-show).
    pub conduct: Vec<(UserId, IncidentKind)>,
    /// Both players accepted a rematch.
    pub rematch: Option<RematchSpec>,
    /// The request was refused with this code (metrics).
    pub rejected: Option<ErrorCode>,
    /// A move was accepted.
    pub moved: bool,
    /// An already played move was resent (idempotent reply).
    pub duplicate: bool,
    /// The clock of that side, held since a recovery, started (or its first-move timer restarted
    /// at its first reconnection after a recovery): the host sends the other player a new
    /// snapshot once the call is done, unless a `MoveMade` or a `GameEnd` already told it.
    pub clock_started: Option<Side>,
}

/// The result of a finished game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameResult {
    /// Final status (never `Ongoing`).
    pub status: GameStatus,
    /// Why the game ended.
    pub reason: EndReason,
    /// White's remaining time at the end.
    pub white_ms: u32,
    /// Black's remaining time at the end.
    pub black_ms: u32,
    /// When the game ended.
    pub ended_at: i64,
}

/// The finished game record committed to the database (DESIGN 5.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameRecord {
    /// Game id.
    pub id: GameId,
    /// Category id or `"custom"`.
    pub category: String,
    /// Rated: created rated and not aborted.
    pub rated: bool,
    /// Base time.
    pub base_ms: u32,
    /// Increment.
    pub inc_ms: u32,
    /// White's user id.
    pub white_id: UserId,
    /// Black's user id.
    pub black_id: UserId,
    /// White's name at the start.
    pub white_name: String,
    /// Black's name at the start.
    pub black_name: String,
    /// White's rating at the start.
    pub white_rating: u16,
    /// Black's rating at the start.
    pub black_rating: u16,
    /// Start of the game (epoch ms).
    pub started_at: i64,
    /// End of the game (epoch ms).
    pub ended_at: i64,
    /// Final status.
    pub status: GameStatus,
    /// End reason.
    pub reason: EndReason,
    /// Packed moves.
    pub moves: Vec<u16>,
    /// Time charged per move.
    pub spent_ms: Vec<u32>,
    /// Mover's remaining time after each move, increment included.
    pub clock_ms: Vec<u32>,
    /// The game this one is a rematch of (0: none).
    pub rematch_of: GameId,
    /// [`record_flag`] bits.
    pub flags: u32,
}

/// One played move and the clock values it produced (one journal move record).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PlyRecord {
    mv: u16,
    /// `MoveFlag` bits from the rules.
    flags: u8,
    /// [`MB_OFFER`] / [`MB_DECLINED`].
    bits: u8,
    spent: u32,
    clock_after: u32,
    quota_after: u32,
    /// gseq of its `MoveMade`.
    gseq: u32,
    /// When the move was accepted (the next turn's start).
    at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RematchState {
    /// The game is not over.
    None,
    /// The rematch window is open.
    Open,
    /// Both players accepted.
    Agreed,
    /// The window closed (expiry, decline, departure, server abort, restart).
    Closed,
}

/// Kinds of the journal event records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum EventKind {
    DrawOffer = 1,
    /// `color` = the decliner.
    DrawDecline = 2,
    /// `arg` = the grace.
    Disconnect = 3,
    Reconnect = 4,
    Desync = 5,
    /// Server restart: `arg` = the recovery grace of both players, plus the clock hold.
    Recovered = 6,
    /// Every counter the move records do not carry.
    Checkpoint = 7,
}

impl EventKind {
    fn from_u8(v: u8) -> Option<EventKind> {
        Some(match v {
            1 => EventKind::DrawOffer,
            2 => EventKind::DrawDecline,
            3 => EventKind::Disconnect,
            4 => EventKind::Reconnect,
            5 => EventKind::Desync,
            6 => EventKind::Recovered,
            7 => EventKind::Checkpoint,
            _ => return None,
        })
    }
}

/// Flags of a `recovered` event record: the first reconnection of each player restarts its
/// first-move timer.
pub(crate) const REC_FIRST_MOVE_RESTART: u8 = 1;

/// Encodes a frame built by the room. Every value comes from the room's validated state (names,
/// category, clamped clocks, known statuses), so encoding cannot fail; a failure is a bug, which
/// the host turns into `Internal` like any panic of a room.
fn frame<M: Message>(msg: &M) -> Bytes {
    msg.to_bytes().expect("room frames hold validated values")
}

/// Clamps a non-negative clock value into `u32`.
fn u32_of(v: i64) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(u32::MAX)
}

/// Strips NULs and truncates `s` to at most `max` bytes on a character boundary.
fn trunc_utf8(s: &str, max: usize) -> String {
    let mut t: String = s.chars().filter(|&c| c != '\0').collect();
    if t.len() > max {
        let mut end = max;
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        t.truncate(end);
    }
    t
}

fn norm_player(p: PlayerInfo) -> PlayerInfo {
    let name = trunc_utf8(&p.name, 24);
    PlayerInfo { name: if name.is_empty() { "?".to_owned() } else { name }, ..p }
}

/// One online game: canonical position, clocks, offers, presence and result.
pub struct GameRoom {
    id: GameId,
    category: String,
    base_ms: u32,
    inc_ms: u32,
    rated: bool,
    players: [PlayerInfo; 2],
    created_at: i64,
    rematch_of: GameId,
    auto_press: bool,
    settings: RoomSettings,
    grace_ms: i64,
    recovery_grace_ms: i64,
    recovery_hold_ms: i64,
    rules: Box<dyn Rules>,
    clock: GameClock,
    plies: Vec<PlyRecord>,
    gseq: u32,
    draw_offer: Option<Side>,
    draw_offers_used: [u16; 2],
    draw_declined_at: [i32; 2],
    desyncs: [u16; 2],
    connected: [bool; 2],
    disconnected_at: [i64; 2],
    /// Grace of each player's current disconnection.
    disconnect_grace: [i64; 2],
    /// The side to move's clock is held since a recovery, until `clock.turn_start()`.
    clock_held: bool,
    /// Per player: not back since a recovery before the second ply.
    away_since_recovery: [bool; 2],
    result: Option<GameResult>,
    end_gseq: u32,
    /// Side whose act or absence ended the game.
    culprit: Option<Side>,
    rematch_by: Option<Side>,
    rematch: RematchState,
    flags: u32,
    /// Set by a lenient replay that stopped early.
    replay_error: Option<JournalError>,
}

impl std::fmt::Debug for GameRoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameRoom")
            .field("id", &self.id)
            .field("ply", &self.plies.len())
            .field("gseq", &self.gseq)
            .field("result", &self.result)
            .finish_non_exhaustive()
    }
}

impl GameRoom {
    /// A new game at its start.
    ///
    /// # Errors
    ///
    /// [`RoomError::InvalidGameId`] when the id is zero or not below 2^53.
    pub fn new(spec: RoomSpec, settings: RoomSettings, rules: Box<dyn Rules>) -> Result<GameRoom, RoomError> {
        if !ids::is_game_id(spec.id) {
            return Err(RoomError::InvalidGameId(spec.id));
        }
        let category = trunc_utf8(&spec.category, 7);
        let base = i64::from(spec.base_ms);
        let grace = settings.grace;
        Ok(GameRoom {
            id: spec.id,
            category: if category.is_empty() { "custom".to_owned() } else { category },
            base_ms: spec.base_ms,
            inc_ms: spec.inc_ms,
            rated: spec.rated,
            players: [norm_player(spec.white), norm_player(spec.black)],
            created_at: spec.created_at,
            rematch_of: if ids::is_game_id(spec.rematch_of) { spec.rematch_of } else { 0 },
            auto_press: spec.auto_press,
            settings,
            grace_ms: grace.grace_for(base),
            recovery_grace_ms: grace.recovery_grace_for(base),
            recovery_hold_ms: grace.recovery_hold_for(base),
            rules,
            clock: GameClock::new(base, i64::from(spec.inc_ms), settings.clock, spec.created_at),
            plies: Vec::with_capacity(64),
            gseq: 0,
            draw_offer: None,
            draw_offers_used: [0; 2],
            draw_declined_at: [-DRAW_REOFFER_PLIES; 2],
            desyncs: [0; 2],
            connected: [true; 2],
            disconnected_at: [0; 2],
            disconnect_grace: [grace.grace_for(base); 2],
            clock_held: false,
            away_since_recovery: [false; 2],
            result: None,
            end_gseq: 0,
            culprit: None,
            rematch_by: None,
            rematch: RematchState::None,
            flags: if spec.rated { record_flag::RATED_REQUESTED } else { 0 },
            replay_error: None,
        })
    }

    // ---- state accessors ------------------------------------------------------------------

    /// Game id.
    #[must_use]
    pub fn id(&self) -> GameId {
        self.id
    }

    /// Category id or `"custom"`.
    #[must_use]
    pub fn category(&self) -> &str {
        &self.category
    }

    /// Created rated.
    #[must_use]
    pub fn rated(&self) -> bool {
        self.rated
    }

    /// The robots press the clock by themselves.
    #[must_use]
    pub fn auto_press(&self) -> bool {
        self.auto_press
    }

    /// Start of the game.
    #[must_use]
    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    /// Plies played so far.
    #[must_use]
    pub fn ply(&self) -> usize {
        self.plies.len()
    }

    /// Number of the last broadcast event.
    #[must_use]
    pub fn gseq(&self) -> u32 {
        self.gseq
    }

    /// True once the result is decided.
    #[must_use]
    pub fn is_over(&self) -> bool {
        self.result.is_some()
    }

    /// The result, once the game is over.
    #[must_use]
    pub fn result(&self) -> Option<&GameResult> {
        self.result.as_ref()
    }

    /// The side to move.
    #[must_use]
    pub fn side_to_move(&self) -> Side {
        Side::to_move(self.ply())
    }

    /// True while a rematch can still be offered or accepted.
    #[must_use]
    pub fn rematch_open(&self) -> bool {
        self.rematch == RematchState::Open
    }

    /// The side with a pending rematch offer.
    #[must_use]
    pub fn rematch_by(&self) -> Option<Side> {
        self.rematch_by
    }

    /// Moves played so far.
    #[must_use]
    pub fn moves(&self) -> Vec<u16> {
        self.plies.iter().map(|p| p.mv).collect()
    }

    /// The player of a side.
    #[must_use]
    pub fn player(&self, side: Side) -> &PlayerInfo {
        &self.players[side.index()]
    }

    /// The side of a user in this game (`None`: not a player).
    #[must_use]
    pub fn side_of(&self, user: UserId) -> Option<Side> {
        Side::BOTH.into_iter().find(|s| self.players[s.index()].user_id == user)
    }

    /// Whether a player is connected (as far as the room knows).
    #[must_use]
    pub fn is_connected(&self, side: Side) -> bool {
        self.connected[side.index()]
    }

    /// The error of a lenient replay that stopped early.
    #[must_use]
    pub fn replay_error(&self) -> Option<&JournalError> {
        self.replay_error.as_ref()
    }

    /// Digest of the current position (as the rules compute it).
    #[must_use]
    pub fn digest(&self) -> u32 {
        self.rules.digest()
    }

    /// The game clock.
    #[must_use]
    pub fn clock(&self) -> &GameClock {
        &self.clock
    }

    /// Earliest moment at which [`GameRoom::tick`] has something to do (`None`: nothing).
    #[must_use]
    pub fn next_deadline(&self) -> Option<i64> {
        match &self.result {
            None => {
                let d = self.clock.deadline(self.ply()).min(self.grace_deadline());
                Some(if self.clock_held { d.min(self.clock.turn_start()) } else { d })
            }
            Some(r) if self.rematch == RematchState::Open => Some(r.ended_at + REMATCH_WINDOW_MS),
            Some(_) => None,
        }
    }

    // ---- inputs ---------------------------------------------------------------------------

    /// Move intent of `side`, validated in the order of DESIGN 6.2 (the host has checked that the
    /// sender is a player of this game).
    pub fn on_move(&mut self, side: Side, msg: &scacelith_protocol::Move, t: impl Into<Timing>) -> Outcome {
        let t = t.into();
        let now = t.now;
        let at = self.arrival(now, t.recv_at);
        let mut out = Outcome::default();
        let ply = usize::from(msg.ply);
        let mv = msg.r#move;
        let was_over = self.is_over();
        self.advance(at, &mut out, t.stalled_since);
        // 2. Game over (a flag that fell at this very moment for the sender is FlagFell).
        if let Some(r) = &self.result {
            let fell = !was_over
                && self.culprit == Some(side)
                && ply == self.ply()
                && matches!(r.reason, EndReason::Timeout | EndReason::TimeoutVsInsufficient);
            return if fell {
                self.reject_move(out, side, ply, mv, ErrorCode::FlagFell, now, None)
            } else {
                self.reject_move(out, side, ply, mv, ErrorCode::GameOver, now, Some(("game_over", false)))
            };
        }
        // 3. Duplicate of a move already played: idempotent when identical.
        if ply < self.ply() {
            if Side::to_move(ply) == side && self.plies[ply].mv == mv {
                out.reply.push(self.encode_move_made(ply));
                out.duplicate = true;
                return out;
            }
            return self.reject_move(
                out,
                side,
                ply,
                mv,
                ErrorCode::StalePly,
                now,
                Some(("stale_ply", false)),
            );
        }
        // 4 and 5. Position mismatch, or a future ply: desynchronised client.
        if ply > self.ply() || msg.pos_hash != self.rules.digest() {
            self.apply_event(EventKind::Desync, Some(side), 0, now, 0, 0);
            out.journal.push(self.event_record(EventKind::Desync, Some(side), 0, now));
            let kind = if self.desyncs[side.index()] >= 3 { "repeated_desync" } else { "desync" };
            return self.reject_move(out, side, ply, mv, ErrorCode::Desync, now, Some((kind, false)));
        }
        // 6. Not the sender's turn although it knew the position.
        if self.side_to_move() != side {
            return self.reject_move(
                out,
                side,
                ply,
                mv,
                ErrorCode::NotYourTurn,
                now,
                Some(("out_of_turn", true)),
            );
        }
        // 7. Illegal move in the synchronised position.
        if !self.rules.is_legal(mv) {
            return self.reject_move(
                out,
                side,
                ply,
                mv,
                ErrorCode::IllegalMove,
                now,
                Some(("illegal_move", true)),
            );
        }
        // 8. Clock, then play. The time charged runs to the credited arrival; the next turn
        // starts now, when the MoveMade goes out.
        let idx = self.ply();
        let think = i64::from(msg.think_ms);
        let c = self.clock.check(side, idx, at, think);
        // The client counts its thinking time from the previous move. After a restart the turn
        // starts later than that: only a thinkMs longer than the real time since that move is
        // impossible (a stall credit shortens the elapsed time the clock sees, not the thinking).
        let since_prev = now - self.last_move_at();
        if c.implausible && think > since_prev + IMPLAUSIBLE_MARGIN_MS {
            out.anomaly = Some(RoomAnomaly {
                side,
                kind: "clock_implausible",
                detail: format!("ply {idx} thinkMs {} elapsed {}", msg.think_ms, c.elapsed),
                pos_matched: true,
            });
        }
        if c.flagged {
            self.flag(side, at, &mut out);
            return self.reject_move(out, side, ply, mv, ErrorCode::FlagFell, now, None);
        }
        let Some(played) = self.rules.play(mv) else {
            // The rules accepted is_legal() but refused play(): a rules inconsistency, not a cheat.
            return self.reject_move(out, side, ply, mv, ErrorCode::IllegalMove, now, None);
        };
        let mut bits = 0;
        let mut offer_refused = false;
        if self.draw_offer == Some(side.opponent()) {
            bits |= MB_DECLINED;
        }
        if msg.draw_offer && self.draw_offer != Some(side) {
            if self.may_offer(side, idx + 1) {
                bits |= MB_OFFER;
            } else {
                offer_refused = true;
            }
        }
        self.apply_move(PlyRecord {
            mv,
            flags: played.flags,
            bits,
            spent: u32_of(c.charged),
            clock_after: u32_of(c.clock_after),
            quota_after: u32_of(c.quota_after),
            gseq: self.gseq.wrapping_add(1),
            at: now,
        });
        self.gseq = self.gseq.wrapping_add(1);
        out.broadcast.push(self.encode_move_made(idx));
        if bits & MB_DECLINED != 0 {
            self.gseq = self.gseq.wrapping_add(1);
            out.broadcast.push(self.game_event(GameEventKind::DrawDeclined, Some(side), 0));
        }
        out.journal.push(self.move_record(idx));
        out.moved = true;
        if offer_refused {
            out.reply.push(self.error(ErrorCode::DrawOfferLimit, msg.seq));
        }
        if played.status != GameStatus::Ongoing {
            self.end(played.status, played.reason, now, &mut out, None, None);
        } else if self.ply() >= MAX_PLIES {
            self.end(GameStatus::Aborted, EndReason::ServerAborted, now, &mut out, None, None);
        }
        out
    }

    /// Resignation (any time while the game runs).
    pub fn on_resign(&mut self, side: Side, seq: u32, t: impl Into<Timing>) -> Outcome {
        let (now, mut out) = self.begin_at_arrival(t.into());
        if self.is_over() {
            return self.refuse(out, ErrorCode::GameOver, seq);
        }
        self.end(win_for(side.opponent()), EndReason::Resignation, now, &mut out, Some(side), None);
        out
    }

    /// Draw offer without a move (DESIGN 6.3).
    pub fn on_draw_offer(&mut self, side: Side, seq: u32, t: impl Into<Timing>) -> Outcome {
        let (now, mut out) = self.begin_at_arrival(t.into());
        if self.is_over() {
            return self.refuse(out, ErrorCode::GameOver, seq);
        }
        if self.draw_offer == Some(side.opponent()) {
            // Both want a draw.
            self.end(GameStatus::Draw, EndReason::Agreement, now, &mut out, None, None);
            return out;
        }
        if self.draw_offer == Some(side) {
            return out; // already standing
        }
        if !self.may_offer(side, self.ply()) {
            return self.refuse(out, ErrorCode::DrawOfferLimit, seq);
        }
        self.apply_event(EventKind::DrawOffer, Some(side), 0, now, 0, 0);
        self.gseq = self.gseq.wrapping_add(1);
        out.broadcast.push(self.game_event(GameEventKind::DrawOffered, Some(side), 0));
        out.journal.push(self.event_record(EventKind::DrawOffer, Some(side), 0, now));
        out
    }

    /// Answer to the opponent's pending draw offer.
    pub fn on_draw_answer(&mut self, side: Side, accept: bool, seq: u32, t: impl Into<Timing>) -> Outcome {
        let (now, mut out) = self.begin_at_arrival(t.into());
        if self.is_over() {
            return self.refuse(out, ErrorCode::GameOver, seq);
        }
        if self.draw_offer != Some(side.opponent()) {
            return self.refuse(out, ErrorCode::NoPendingOffer, seq);
        }
        if accept {
            self.end(GameStatus::Draw, EndReason::Agreement, now, &mut out, None, None);
            return out;
        }
        self.apply_event(EventKind::DrawDecline, Some(side), 0, now, 0, 0);
        self.gseq = self.gseq.wrapping_add(1);
        out.broadcast.push(self.game_event(GameEventKind::DrawDeclined, Some(side), 0));
        out.journal.push(self.event_record(EventKind::DrawDecline, Some(side), 0, now));
        out
    }

    /// Draw claim: threefold repetition or fifty-move rule in the current position.
    pub fn on_draw_claim(&mut self, side: Side, seq: u32, t: impl Into<Timing>) -> Outcome {
        let (now, mut out) = self.begin_at_arrival(t.into());
        if self.is_over() {
            return self.refuse(out, ErrorCode::GameOver, seq);
        }
        let reason = if self.rules.can_claim_threefold() {
            EndReason::ThreefoldClaim
        } else if self.rules.can_claim_fifty_move() {
            EndReason::FiftyMoveClaim
        } else {
            let mut out = self.refuse(out, ErrorCode::NothingToClaim, seq);
            out.anomaly = Some(RoomAnomaly {
                side,
                kind: "nothing_to_claim",
                detail: format!("ply {}", self.ply()),
                pos_matched: true,
            });
            return out;
        };
        self.end(GameStatus::Draw, reason, now, &mut out, None, None);
        out
    }

    /// Abort: only before the sender's own first move (conduct incident `abort`).
    pub fn on_abort(&mut self, side: Side, seq: u32, t: impl Into<Timing>) -> Outcome {
        let (now, mut out) = self.begin_at_arrival(t.into());
        if self.is_over() {
            return self.refuse(out, ErrorCode::GameOver, seq);
        }
        if self.ply() > side.index() {
            return self.refuse(out, ErrorCode::AbortNotAllowed, seq);
        }
        self.end(GameStatus::Aborted, EndReason::Aborted, now, &mut out, Some(side), None);
        out.conduct.push((self.player(side).user_id, IncidentKind::Abort));
        out
    }

    /// Rematch after the end: `accept` offers or accepts, `!accept` declines or withdraws. When
    /// both accepted, [`Outcome::rematch`] holds the new game (colours swapped).
    pub fn on_rematch(&mut self, side: Side, accept: bool, seq: u32, t: impl Into<Timing>) -> Outcome {
        let t = t.into();
        let mut out = Outcome::default();
        self.advance(self.arrival(t.now, t.recv_at), &mut out, t.stalled_since);
        if !self.is_over() || self.rematch != RematchState::Open {
            return self.refuse(out, ErrorCode::RematchUnavailable, seq);
        }
        if !accept {
            self.close_rematch(&mut out, Some(side));
            return out;
        }
        if self.rematch_by == Some(side.opponent()) {
            self.rematch = RematchState::Agreed;
            self.rematch_by = None;
            out.rematch = Some(RematchSpec {
                game: self.id,
                white: self.players[1].clone(),
                black: self.players[0].clone(),
                category: self.category.clone(),
                base_ms: self.base_ms,
                inc_ms: self.inc_ms,
                rated: self.rated,
                auto_press: self.auto_press,
            });
            return out;
        }
        if self.rematch_by == Some(side) {
            return out;
        }
        self.rematch_by = Some(side);
        self.gseq = self.gseq.wrapping_add(1);
        out.broadcast.push(self.game_event(GameEventKind::RematchOffered, Some(side), 0));
        out
    }

    /// The player's connection is gone (DESIGN 6.4). After the end it only closes the rematch
    /// window.
    pub fn on_disconnect(&mut self, side: Side, t: impl Into<Timing>) -> Outcome {
        let t = t.into();
        let now = t.now;
        let mut out = Outcome::default();
        self.advance(self.arrival(now, t.recv_at), &mut out, t.stalled_since);
        let i = side.index();
        if self.is_over() {
            self.connected[i] = false;
            self.close_rematch(&mut out, Some(side));
            return out;
        }
        if !self.connected[i] {
            return out;
        }
        let grace = self.grace_ms;
        self.apply_event(EventKind::Disconnect, Some(side), grace, now, 0, 0);
        self.gseq = self.gseq.wrapping_add(1);
        out.broadcast.push(self.game_event(GameEventKind::PlayerDisconnected, Some(side), grace));
        out.journal.push(self.event_record(EventKind::Disconnect, Some(side), grace, now));
        out
    }

    /// The player is back (the host sends the snapshot).
    pub fn on_reconnect(&mut self, side: Side, t: impl Into<Timing>) -> Outcome {
        let t = t.into();
        let now = t.now;
        let mut out = Outcome::default();
        self.advance(self.arrival(now, t.recv_at), &mut out, t.stalled_since);
        let i = side.index();
        if self.is_over() {
            self.connected[i] = true;
            return out;
        }
        if self.connected[i] {
            return out;
        }
        // A clock held for this player, or its first-move timer after a recovery, starts now.
        let started = self.apply_event(EventKind::Reconnect, Some(side), 0, now, 0, 0);
        self.gseq = self.gseq.wrapping_add(1);
        out.broadcast.push(self.game_event(GameEventKind::PlayerReconnected, Some(side), 0));
        out.journal.push(self.event_record(EventKind::Reconnect, Some(side), 0, now));
        if started {
            out.clock_started = Some(side);
        }
        out
    }

    /// A round-trip measurement of the player (exponential average, capped at 2 s). It changes
    /// the compensation cap, hence the deadlines: the host reschedules.
    pub fn on_rtt(&mut self, side: Side, rtt_ms: f64) {
        self.clock.set_rtt(side, rtt_ms);
    }

    /// The full snapshot for `side`, as a reply, after processing the due deadlines.
    pub fn on_resync(&mut self, side: Side, t: impl Into<Timing>) -> Outcome {
        let t = t.into();
        let mut out = Outcome::default();
        self.advance(self.arrival(t.now, t.recv_at), &mut out, t.stalled_since);
        out.reply.push(self.snapshot_frame(side, t.now));
        out
    }

    /// Anti-cheat: `side` loses (`Forfeit`; a rated game is rated normally). Nothing happens when
    /// the game is already over.
    pub fn forfeit(&mut self, side: Side, t: impl Into<Timing>) -> Outcome {
        let (now, mut out) = self.begin_at_arrival(t.into());
        if self.is_over() {
            return out;
        }
        self.end(win_for(side.opponent()), EndReason::Forfeit, now, &mut out, Some(side), None);
        out
    }

    /// The server cannot continue this game: `Aborted` / `ServerAborted` (unrated). No deadline
    /// is processed first.
    pub fn server_abort(&mut self, now: i64) -> Outcome {
        let mut out = Outcome::default();
        if !self.is_over() {
            self.end(GameStatus::Aborted, EndReason::ServerAborted, now, &mut out, None, None);
        }
        out
    }

    /// Processes every deadline due at `t.now` (flag, first-move timeout, grace, rematch window);
    /// `t.stalled_since` is the start of the host stall these deadlines waited through.
    pub fn tick(&mut self, t: impl Into<Timing>) -> Outcome {
        let t = t.into();
        let mut out = Outcome::default();
        self.advance(t.now, &mut out, t.stalled_since);
        out
    }

    /// Server restart semantics (DESIGN 6.4) on a room rebuilt by [`GameRoom::from_journal`]:
    /// both players away with the recovery grace, the running clock (or first-move timer) held
    /// from its journaled value until the side to move is back, the recovery hold at most; before
    /// the second ply, the first reconnection of the side to move also restarts its first-move
    /// timer after the hold. A finished game only loses its rematch window. The outcome carries
    /// the journal record of the recovery (or the end of a game whose `ended` record was lost).
    pub fn recover(&mut self, now: i64) -> Outcome {
        let mut out = Outcome::default();
        if !self.is_over() {
            // The journal kept the move that ended the game but not its `ended` record: an end of
            // the rules, or the ply limit's (as in on_move).
            let at = self.last_move_at();
            let status = self.rules.status();
            if status != GameStatus::Ongoing {
                let reason = self.rules.reason();
                self.end(status, reason, at, &mut out, None, None);
            } else if self.ply() >= MAX_PLIES {
                self.end(GameStatus::Aborted, EndReason::ServerAborted, at, &mut out, None, None);
            }
        }
        if self.is_over() {
            self.rematch = RematchState::Closed;
            self.rematch_by = None;
            return out;
        }
        let (grace, hold) = (self.recovery_grace_ms, self.recovery_hold_ms);
        self.apply_event(EventKind::Recovered, None, grace, now, hold, REC_FIRST_MOVE_RESTART);
        self.gseq = self.gseq.wrapping_add(RECOVERY_GSEQ_JUMP);
        out.journal.push(self.recovered_record(grace, now, hold, REC_FIRST_MOVE_RESTART));
        out
    }

    // ---- outputs --------------------------------------------------------------------------

    /// Authoritative state for `side` (`GameSnapshot`), clocks at `now`.
    #[must_use]
    pub fn snapshot(&self, side: Side, now: i64) -> GameSnapshot {
        let n = self.ply();
        let moves = self
            .plies
            .iter()
            .map(|p| MoveRec { r#move: p.mv, spent_ms: p.spent, clock_ms: p.clock_after })
            .collect();
        let (status, reason, white_ms, black_ms, grace_ms, first_move_ms) = match &self.result {
            Some(r) => (r.status, r.reason, r.white_ms, r.black_ms, 0, 0),
            None => (
                GameStatus::Ongoing,
                EndReason::None,
                u32_of(self.clock.remaining_at(Side::White, n, now)),
                u32_of(self.clock.remaining_at(Side::Black, n, now)),
                self.grace_left(side, now),
                u32_of(self.clock.first_move_left(n, now)),
            ),
        };
        // A clock held after a recovery is not running yet (the client counts the running clock
        // down from serverTime).
        let running = !self.is_over() && n >= 2 && !self.clock.held_at(now);
        GameSnapshot {
            game: self.id,
            gseq: self.gseq,
            category: self.category.clone(),
            base_ms: self.base_ms,
            inc_ms: self.inc_ms,
            rated: self.rated,
            white: self.players[0].clone(),
            black: self.players[1].clone(),
            you: side.color(),
            moves,
            running: color_of(running.then(|| Side::to_move(n))),
            white_ms,
            black_ms,
            server_time: now as f64,
            draw_offer: color_of(self.draw_offer),
            status,
            reason,
            white_connected: self.connected[0],
            black_connected: self.connected[1],
            grace_ms,
            first_move_ms,
            started_at: self.created_at as f64,
            rematch: color_of(if self.rematch == RematchState::Open { self.rematch_by } else { None }),
            auto_press: self.auto_press,
        }
    }

    /// The encoded [`GameRoom::snapshot`].
    #[must_use]
    pub fn snapshot_frame(&self, side: Side, now: i64) -> Bytes {
        frame(&self.snapshot(side, now))
    }

    /// The finished game record for the database (`None` while the game runs).
    #[must_use]
    pub fn record(&self) -> Option<GameRecord> {
        let r = self.result?;
        let [white, black] = &self.players;
        Some(GameRecord {
            id: self.id,
            category: self.category.clone(),
            rated: self.rated && r.status != GameStatus::Aborted,
            base_ms: self.base_ms,
            inc_ms: self.inc_ms,
            white_id: white.user_id,
            black_id: black.user_id,
            white_name: white.name.clone(),
            black_name: black.name.clone(),
            white_rating: white.rating,
            black_rating: black.rating,
            started_at: self.created_at,
            ended_at: r.ended_at,
            status: r.status,
            reason: r.reason,
            moves: self.moves(),
            spent_ms: self.plies.iter().map(|p| p.spent).collect(),
            clock_ms: self.plies.iter().map(|p| p.clock_after).collect(),
            rematch_of: self.rematch_of,
            flags: self.flags | if self.auto_press { 0 } else { record_flag::MANUAL_PRESS },
        })
    }

    // ---- internals ------------------------------------------------------------------------

    /// Processes the deadlines due at the credited arrival; returns the arrival (the time of the
    /// call's effect) and the outcome to fill.
    fn begin_at_arrival(&mut self, t: Timing) -> (i64, Outcome) {
        let now = self.arrival(t.now, t.recv_at);
        let mut out = Outcome::default();
        self.advance(now, &mut out, t.stalled_since);
        (now, out)
    }

    /// When the latest move was accepted (the creation for none).
    fn last_move_at(&self) -> i64 {
        self.plies.last().map_or(self.created_at, |p| p.at)
    }

    /// The moment a request counts as arrived: `recv_at` when the host credited a stall, but
    /// never before the latest move (whose `MoveMade` the request may answer) and never after
    /// `now` (so the time a room sees never goes back).
    fn arrival(&self, now: i64, recv_at: i64) -> i64 {
        if recv_at >= now {
            return now;
        }
        let last = self.last_move_at();
        if recv_at > last { recv_at } else { last.min(now) }
    }

    fn may_offer(&self, side: Side, ply_now: usize) -> bool {
        let i = side.index();
        u32::from(self.draw_offers_used[i]) < self.settings.draw_offers_per_game
            && ply_now as i64 >= i64::from(self.draw_declined_at[i]) + i64::from(DRAW_REOFFER_PLIES)
    }

    fn apply_move(&mut self, rec: PlyRecord) {
        let side = self.side_to_move();
        self.clock.apply(side, i64::from(rec.clock_after), i64::from(rec.quota_after), rec.at);
        self.clock_held = false; // the next turn starts now (the MoveMade tells both players)
        self.plies.push(rec);
        if rec.bits & MB_DECLINED != 0 {
            self.draw_offer = None;
            self.draw_declined_at[side.opponent().index()] = self.ply() as i32;
        }
        if rec.bits & MB_OFFER != 0 {
            self.draw_offer = Some(side);
            self.draw_offers_used[side.index()] = self.draw_offers_used[side.index()].saturating_add(1);
        }
    }

    /// Applies an event (live or replayed). Returns true when it started the clock or the
    /// first-move timer of the side to move (a reconnection after a recovery). `side` is `Some`
    /// for every kind but `Recovered`.
    fn apply_event(
        &mut self,
        kind: EventKind,
        side: Option<Side>,
        arg: i64,
        at: i64,
        hold: i64,
        rec_flags: u8,
    ) -> bool {
        match (kind, side) {
            (EventKind::DrawOffer, Some(s)) => {
                self.draw_offer = Some(s);
                self.draw_offers_used[s.index()] = self.draw_offers_used[s.index()].saturating_add(1);
            }
            (EventKind::DrawDecline, Some(s)) => {
                self.draw_offer = None;
                self.draw_declined_at[s.opponent().index()] = self.ply() as i32;
            }
            (EventKind::Disconnect, Some(s)) => {
                let i = s.index();
                self.connected[i] = false;
                self.disconnected_at[i] = at;
                self.disconnect_grace[i] = if arg > 0 { arg } else { self.grace_ms };
            }
            (EventKind::Reconnect, Some(s)) => {
                let i = s.index();
                self.connected[i] = true;
                let first = std::mem::take(&mut self.away_since_recovery[i]);
                if s != self.side_to_move() {
                    return false;
                }
                if self.clock_held {
                    // The side to move is back before the end of the hold: its clock starts now.
                    if at < self.clock.turn_start() {
                        self.clock.restart(at);
                    }
                    self.clock_held = false;
                    return true;
                }
                if first && self.ply() < 2 && at > self.clock.turn_start() {
                    // Restored before the second ply and back for the first time after the hold
                    // ended (or after the opponent's first move): its whole first-move time.
                    self.clock.restart(at);
                    return true;
                }
            }
            (EventKind::Desync, Some(s)) => {
                self.desyncs[s.index()] = self.desyncs[s.index()].saturating_add(1);
            }
            (EventKind::Recovered, _) => {
                self.connected = [false; 2];
                self.disconnected_at = [at; 2];
                self.disconnect_grace = [if arg > 0 { arg } else { self.recovery_grace_ms }; 2];
                // Held until the side to move is back, `hold` at most.
                self.clock.restart(at + hold);
                self.clock_held = hold > 0;
                self.away_since_recovery = [rec_flags & REC_FIRST_MOVE_RESTART != 0 && self.ply() < 2; 2];
                self.flags |= record_flag::RECOVERED;
            }
            // A side-less event other than `Recovered`, or a checkpoint (applied by its replay):
            // the callers never pass them.
            _ => {}
        }
        false
    }

    fn apply_end(&mut self, result: GameResult, culprit: Option<Side>) {
        self.culprit = culprit;
        self.draw_offer = None;
        self.clock_held = false;
        self.away_since_recovery = [false; 2];
        if result.reason == EndReason::Forfeit {
            self.flags |= record_flag::FORFEIT;
        }
        self.rematch =
            if result.reason == EndReason::ServerAborted { RematchState::Closed } else { RematchState::Open };
        self.rematch_by = None;
        if self.rules.status() == GameStatus::Ongoing {
            // The rules object is only informative once the game is over.
            self.rules.end(result.status, result.reason);
        }
        self.result = Some(result);
    }

    /// Ends the game now: result, `GameEnd` broadcast, `ended` journal record.
    fn end(
        &mut self,
        status: GameStatus,
        reason: EndReason,
        now: i64,
        out: &mut Outcome,
        culprit: Option<Side>,
        flagged: Option<Side>,
    ) {
        let n = self.ply();
        let ms = |s: Side| if flagged == Some(s) { 0 } else { u32_of(self.clock.remaining_at(s, n, now)) };
        let result = GameResult {
            status,
            reason,
            white_ms: ms(Side::White),
            black_ms: ms(Side::Black),
            ended_at: now,
        };
        self.apply_end(result, culprit);
        self.gseq = self.gseq.wrapping_add(1);
        self.end_gseq = self.gseq;
        out.broadcast.push(frame(&GameEnd {
            game: self.id,
            gseq: self.gseq,
            status,
            reason,
            white_ms: result.white_ms,
            black_ms: result.black_ms,
            server_time: now as f64,
        }));
        out.journal.push(self.ended_record());
        out.ended = true;
    }

    fn flag(&mut self, side: Side, now: i64, out: &mut Outcome) {
        let opp = side.opponent();
        if self.rules.can_color_mate(opp) {
            self.end(win_for(opp), EndReason::Timeout, now, out, Some(side), Some(side));
        } else {
            self.end(GameStatus::Draw, EndReason::TimeoutVsInsufficient, now, out, Some(side), Some(side));
        }
    }

    /// End of the grace of `side`'s current disconnection.
    fn grace_end(&self, side: Side) -> i64 {
        self.disconnected_at[side.index()] + self.disconnect_grace[side.index()]
    }

    fn both_left_together(&self) -> bool {
        (self.disconnected_at[0] - self.disconnected_at[1]).abs() <= BOTH_DISCONNECT_WINDOW_MS
    }

    /// Grace expiry moment ([`NEVER`] when both are connected). Both gone within 5 s of each
    /// other: the later grace end; otherwise the first grace to end is the one of the player who
    /// abandons.
    fn grace_deadline(&self) -> i64 {
        match (self.connected[0], self.connected[1]) {
            (true, true) => NEVER,
            (false, false) => {
                let (w, b) = (self.grace_end(Side::White), self.grace_end(Side::Black));
                if self.both_left_together() { w.max(b) } else { w.min(b) }
            }
            (false, true) => self.grace_end(Side::White),
            (true, false) => self.grace_end(Side::Black),
        }
    }

    fn grace_deadline_of(&self, side: Side) -> i64 {
        if self.connected[side.index()] {
            return NEVER;
        }
        let other = side.opponent();
        if !self.connected[other.index()] && self.both_left_together() {
            return self.grace_end(side).max(self.grace_end(other));
        }
        self.grace_end(side)
    }

    /// Grace left of the player the viewer waits for (the opponent, else the viewer itself).
    fn grace_left(&self, side: Side, now: i64) -> u32 {
        let mut d = self.grace_deadline_of(side.opponent());
        if d == NEVER {
            d = self.grace_deadline_of(side);
        }
        if d == NEVER { 0 } else { u32_of(d - now) }
    }

    fn advance(&mut self, now: i64, out: &mut Outcome, stalled_since: i64) {
        if !self.is_over() {
            if self.clock_held && now >= self.clock.turn_start() {
                // The hold after a recovery is over and the side to move is still away: its clock
                // runs from the end of the hold. Journaled (a checkpoint) so that a replay clears
                // it too.
                self.clock_held = false;
                out.journal.push(self.checkpoint_record(now));
                out.clock_started = Some(self.side_to_move());
            }
            let tm = self.clock.deadline(self.ply());
            let tg = self.grace_deadline();
            if tm <= now && tm <= tg {
                self.on_time_deadline(now, out, tm >= stalled_since);
            } else if tg <= now {
                self.on_grace_expired(now, out);
            }
        }
        if let Some(r) = &self.result
            && self.rematch == RematchState::Open
            && now >= r.ended_at + REMATCH_WINDOW_MS
        {
            self.close_rematch(out, None);
        }
    }

    /// `in_stall`: the deadline fell during a stall of the host.
    fn on_time_deadline(&mut self, now: i64, out: &mut Outcome, in_stall: bool) {
        let side = self.side_to_move();
        if self.ply() >= 2 {
            self.flag(side, now, out);
            return;
        }
        self.end(GameStatus::Aborted, EndReason::NoShow, now, out, Some(side), None);
        // A restored game whose side to move is still away: the server broke the connection,
        // not the player (the game is aborted all the same, unrated). Nor is a first-move time
        // that ran out while the server was not answering held against the player.
        let recovered = self.flags & record_flag::RECOVERED != 0;
        if !in_stall && (!recovered || self.connected[side.index()]) {
            out.conduct.push((self.player(side).user_id, IncidentKind::NoShow));
        }
    }

    fn on_grace_expired(&mut self, now: i64, out: &mut Outcome) {
        let (w, b) = (!self.connected[0], !self.connected[1]);
        if w && b && self.both_left_together() {
            self.end(GameStatus::Aborted, EndReason::BothDisconnected, now, out, None, None);
            return;
        }
        let absent = if w && b {
            if self.grace_end(Side::White) <= self.grace_end(Side::Black) { Side::White } else { Side::Black }
        } else if w {
            Side::White
        } else {
            Side::Black
        };
        let user = self.player(absent).user_id;
        if self.ply() < 2 {
            self.end(GameStatus::Aborted, EndReason::NoShow, now, out, Some(absent), None);
            out.conduct.push((user, IncidentKind::NoShow));
            return;
        }
        let opp = absent.opponent();
        if self.rules.can_color_mate(opp) {
            self.end(win_for(opp), EndReason::Abandonment, now, out, Some(absent), None);
        } else {
            self.end(GameStatus::Draw, EndReason::AbandonmentVsInsufficient, now, out, Some(absent), None);
        }
        out.conduct.push((user, IncidentKind::Abandon));
    }

    /// Closes the rematch window. A decline or a departure (`by`) always tells both players; an
    /// expiry only when someone had offered.
    fn close_rematch(&mut self, out: &mut Outcome, by: Option<Side>) {
        if self.rematch != RematchState::Open {
            return;
        }
        self.rematch = RematchState::Closed;
        if self.rematch_by.is_some() || by.is_some() {
            self.gseq = self.gseq.wrapping_add(1);
            out.broadcast.push(self.game_event(GameEventKind::RematchDeclined, by, 0));
        }
        self.rematch_by = None;
    }

    /// Refuses a move: `MoveRejected` then a snapshot (taken after the end when the move flagged,
    /// so it shows the result), and the anomaly `(kind, pos_matched)` when there is one.
    #[allow(clippy::too_many_arguments)]
    fn reject_move(
        &self,
        mut out: Outcome,
        side: Side,
        ply: usize,
        mv: u16,
        code: ErrorCode,
        now: i64,
        anomaly: Option<(&'static str, bool)>,
    ) -> Outcome {
        out.rejected = Some(code);
        // The protocol bounds a client's ply and move (strict decoding): they fit the frame.
        out.reply.push(frame(&MoveRejected { game: self.id, ply: ply as u16, r#move: mv, code }));
        out.reply.push(self.snapshot_frame(side, now));
        if let Some((kind, pos_matched)) = anomaly {
            out.anomaly = Some(RoomAnomaly {
                side,
                kind,
                detail: format!("ply {ply} move {mv} (game at ply {})", self.ply()),
                pos_matched,
            });
        }
        out
    }

    fn refuse(&self, mut out: Outcome, code: ErrorCode, seq: u32) -> Outcome {
        out.rejected = Some(code);
        out.reply.push(self.error(code, seq));
        out
    }

    fn error(&self, code: ErrorCode, seq: u32) -> Bytes {
        frame(&scacelith_protocol::Error { r#ref: seq, code, fatal: false, game: self.id })
    }

    fn game_event(&self, kind: GameEventKind, side: Option<Side>, arg: i64) -> Bytes {
        frame(&GameEvent { game: self.id, gseq: self.gseq, kind, color: color_of(side), arg: u32_of(arg) })
    }

    /// The `MoveMade` of ply `i`, byte-identical to the one broadcast when it was played.
    fn encode_move_made(&self, i: usize) -> Bytes {
        let p = &self.plies[i];
        let mover = Side::to_move(i);
        let other_ms = if i >= 1 { self.plies[i - 1].clock_after } else { self.base_ms };
        let (white_ms, black_ms) =
            if mover == Side::White { (p.clock_after, other_ms) } else { (other_ms, p.clock_after) };
        frame(&MoveMade {
            game: self.id,
            gseq: p.gseq,
            ply: i as u16,
            r#move: p.mv,
            flags: p.flags,
            spent_ms: p.spent,
            white_ms,
            black_ms,
            server_time: p.at as f64,
            draw_offer: p.bits & MB_OFFER != 0,
            first_move_ms: if i == 0 { u32_of(self.settings.clock.first_move_ms) } else { 0 },
        })
    }
}
