//! Metrics of the host actors, with the former server's names, help texts and buckets. They are
//! process-wide (the former per-worker `shard` label is gone: one process hosts every shard), so
//! each host also keeps its own [`Counters`] for the tests and the admin view.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use scacelith_protocol::{EndReason, ErrorCode};

use crate::metrics::{self, Counter, CounterVec, Gauge, Histogram};

/// Why a gesture was not relayed (label of `scacelith_gestures_dropped_total`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GestureDrop {
    /// The game is not hosted here (any more).
    NoGame,
    /// The sender plays no colour in the game.
    NotPlayer,
    /// The opponent has no connection attached.
    NoOpponent,
    /// The frame is not a valid `C_Gesture`.
    Malformed,
    /// The opponent's connection already holds a backlog.
    Backlog,
}

impl GestureDrop {
    /// Every reason, in label order.
    pub const ALL: [GestureDrop; 5] =
        [GestureDrop::NoGame, GestureDrop::NotPlayer, GestureDrop::NoOpponent, GestureDrop::Backlog, GestureDrop::Malformed];

    /// The metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GestureDrop::NoGame => "no_game",
            GestureDrop::NotPlayer => "not_player",
            GestureDrop::NoOpponent => "no_opponent",
            GestureDrop::Malformed => "malformed",
            GestureDrop::Backlog => "backlog",
        }
    }
}

/// What one host counted since it started (the process-wide metrics sum every host).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counters {
    /// Games created.
    pub created: u64,
    /// Moves accepted.
    pub moves: u64,
    /// Move intents timed (`scacelith_game_move_processing_us` observations).
    pub moves_timed: u64,
    /// Games ended (during play, by a recovery or found ended in the journal).
    pub ended: u64,
    /// Games committed to the database.
    pub committed: u64,
    /// Running games restored from the journal.
    pub recovered: u64,
    /// Ended games found in the journal and queued for their commit.
    pub requeued: u64,
    /// Games the journal could only partly replay (ended `ServerAborted`).
    pub aborted: u64,
    /// Compaction snapshots appended.
    pub snapshots: u64,
    /// Stalls of the actor detected by its beat.
    pub stalls: u64,
    /// Milliseconds given back to requests after stalls.
    pub stall_credit_ms: u64,
    /// Deadlines fired by the timers (`scacelith_game_timer_late_ms` observations).
    pub timer_firings: u64,
    /// Gestures relayed.
    pub gestures: u64,
    /// Gestures dropped, by reason label.
    pub gesture_drops: BTreeMap<&'static str, u64>,
    /// Game requests refused, by error code name.
    pub rejects: BTreeMap<&'static str, u64>,
    /// Games ended, by end reason name.
    pub ended_by_reason: BTreeMap<&'static str, u64>,
    /// Database commits that succeeded (`scacelith_game_commit_batch_size` observations).
    pub commit_batches: u64,
    /// Failed commit attempts.
    pub commit_errors: u64,
    /// Games committed without waiting for the journal.
    pub unjournaled: u64,
}

/// The process-wide metric handles.
struct Global {
    active: Gauge,
    moves: Counter,
    move_us: Histogram,
    rejected: CounterVec,
    ended: CounterVec,
    batch: Histogram,
    commit_ms: Histogram,
    commit_errors: Counter,
    unjournaled: Counter,
    stall_ms: Histogram,
    stall_credit: Counter,
    timer_late: Histogram,
    gestures: Counter,
    gesture_drops: [Counter; 5],
}

static GLOBAL: LazyLock<Global> = LazyLock::new(|| {
    let drops = metrics::counter_vec("scacelith_gestures_dropped_total", "Gestures not relayed, by reason", &["reason"]);
    Global {
        active: metrics::gauge("scacelith_games_active", "Games in progress"),
        moves: metrics::counter("scacelith_game_moves_total", "Moves accepted"),
        move_us: metrics::histogram(
            "scacelith_game_move_processing_us",
            "Time to process one move intent, delivery and journaling included (microseconds)",
            &[5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0],
        ),
        rejected: metrics::counter_vec("scacelith_game_rejects_total", "Game requests refused, by error code", &["code"]),
        ended: metrics::counter_vec("scacelith_games_ended_total", "Games ended, by reason", &["reason"]),
        batch: metrics::histogram(
            "scacelith_game_commit_batch_size",
            "Finished games per database commit",
            &[1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0],
        ),
        commit_ms: metrics::histogram(
            "scacelith_game_commit_latency_ms",
            "Database commit latency of finished games",
            &[1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0],
        ),
        commit_errors: metrics::counter(
            "scacelith_game_commit_errors_total",
            "Failed commits of finished games (retried)",
        ),
        unjournaled: metrics::counter(
            "scacelith_game_commit_unjournaled_total",
            "Finished games committed to the database without waiting for the journal, whose writes kept failing",
        ),
        stall_ms: metrics::histogram(
            "scacelith_game_stall_ms",
            "Stalls of the game host actors detected by their timers (ms, longer than GAME_STALL_MIN_MS)",
            &[25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0],
        ),
        stall_credit: metrics::counter(
            "scacelith_game_stall_credit_ms_total",
            "Time given back to game requests handled after a stall of their host actor (ms)",
        ),
        timer_late: metrics::histogram(
            "scacelith_game_timer_late_ms",
            "Lateness of the game deadlines (flags, first-move timeouts, graces) when their timer fired (ms)",
            &[1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 1000.0, 5000.0],
        ),
        gestures: metrics::counter("scacelith_gestures_relayed_total", "Gestures relayed to the opponent"),
        gesture_drops: GestureDrop::ALL.map(|d| drops.with(&[d.as_str()])),
    }
});

/// The meters of one host: every event goes to the process-wide metrics and to the host's
/// [`Counters`].
#[derive(Debug, Default)]
pub(super) struct Meter {
    pub(super) counts: Counters,
}

impl Meter {
    /// Registers the metrics (so that they show at 0 before the first event).
    pub(super) fn new() -> Meter {
        LazyLock::force(&GLOBAL);
        Meter::default()
    }

    pub(super) fn game_started(&mut self) {
        GLOBAL.active.inc();
    }

    pub(super) fn game_finished(&mut self) {
        GLOBAL.active.dec();
    }

    pub(super) fn created(&mut self) {
        self.counts.created += 1;
    }

    pub(super) fn moved(&mut self) {
        GLOBAL.moves.inc();
        self.counts.moves += 1;
    }

    pub(super) fn move_timed(&mut self, us: f64) {
        GLOBAL.move_us.observe(us);
        self.counts.moves_timed += 1;
    }

    pub(super) fn rejected(&mut self, code: ErrorCode) {
        let name = code.name();
        GLOBAL.rejected.with(&[name]).inc();
        *self.counts.rejects.entry(name).or_default() += 1;
    }

    pub(super) fn ended(&mut self, reason: EndReason) {
        let name = reason.name();
        GLOBAL.ended.with(&[name]).inc();
        *self.counts.ended_by_reason.entry(name).or_default() += 1;
    }

    pub(super) fn committed(&mut self, games: usize, ms: f64, unjournaled: bool) {
        GLOBAL.commit_ms.observe(ms);
        GLOBAL.batch.observe(games as f64);
        self.counts.commit_batches += 1;
        if unjournaled {
            GLOBAL.unjournaled.add(games as u64);
            self.counts.unjournaled += games as u64;
        }
    }

    pub(super) fn commit_failed(&mut self) {
        GLOBAL.commit_errors.inc();
        self.counts.commit_errors += 1;
    }

    pub(super) fn stall(&mut self, ms: i64) {
        GLOBAL.stall_ms.observe(ms as f64);
        self.counts.stalls += 1;
    }

    pub(super) fn stall_credit(&mut self, ms: i64) {
        let ms = u64::try_from(ms).unwrap_or(0);
        GLOBAL.stall_credit.add(ms);
        self.counts.stall_credit_ms += ms;
    }

    pub(super) fn timer_fired(&mut self, late_ms: i64) {
        GLOBAL.timer_late.observe(late_ms.max(0) as f64);
        self.counts.timer_firings += 1;
    }

    pub(super) fn gesture_relayed(&mut self) {
        GLOBAL.gestures.inc();
        self.counts.gestures += 1;
    }

    pub(super) fn gesture_dropped(&mut self, why: GestureDrop) {
        let i = GestureDrop::ALL.iter().position(|&d| d == why).unwrap_or(0);
        GLOBAL.gesture_drops[i].inc();
        *self.counts.gesture_drops.entry(why.as_str()).or_default() += 1;
    }
}
