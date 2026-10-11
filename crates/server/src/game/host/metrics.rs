//! Metrics of the host actors, with the former server's names, help texts and buckets. They are
//! process-wide (the former per-worker `shard` label is gone: one process hosts every shard), so
//! each host also keeps its own [`Counters`] for the tests and the admin view.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use scacelith_protocol::{EndReason, ErrorCode};

use super::inbox::Refusal;
use crate::metrics::{self, Counter, CounterVec, Gauge, GaugeVec, Histogram};

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
    /// The host's inbox already holds `GESTURE_INBOX_MAX` gestures (dropped before reaching it).
    Overload,
}

impl GestureDrop {
    /// Every reason, in label order.
    pub const ALL: [GestureDrop; 6] = [
        GestureDrop::NoGame,
        GestureDrop::NotPlayer,
        GestureDrop::NoOpponent,
        GestureDrop::Backlog,
        GestureDrop::Malformed,
        GestureDrop::Overload,
    ];

    /// The metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GestureDrop::NoGame => "no_game",
            GestureDrop::NotPlayer => "not_player",
            GestureDrop::NoOpponent => "no_opponent",
            GestureDrop::Malformed => "malformed",
            GestureDrop::Backlog => "backlog",
            GestureDrop::Overload => "overload",
        }
    }
}

/// Why a game of the journal was dropped at start (label of
/// `scacelith_game_recovery_dropped_total`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryDrop {
    /// The database already holds it: finished, whatever the journal says.
    InDatabase,
    /// A player the database does not know: it can never be committed.
    UnknownPlayer,
}

impl RecoveryDrop {
    /// The metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RecoveryDrop::InDatabase => "in_database",
            RecoveryDrop::UnknownPlayer => "unknown_player",
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
    /// Games of the journal the database already held at start (dropped from the journal).
    pub recovery_in_database: u64,
    /// Games of the journal with a player the database did not know at start (dropped).
    pub recovery_unknown_player: u64,
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
    /// Stances (minor 2) queued for the opponent.
    pub stances: u64,
    /// Messages refused at the door of the inbox, by kind label (the gestures aside: they are in
    /// `gesture_drops`, as `overload`).
    pub inbox_refused: BTreeMap<&'static str, u64>,
    /// Lifecycle messages delivered beyond the inbox's reserve.
    pub inbox_over_reserve: u64,
    /// Attaches a connection tried again later: the inbox held an earlier connection's attach and
    /// detach of the same player's game.
    pub attaches_deferred: u64,
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
    recovery_dropped: [Counter; 2],
    draining: Gauge,
    stall_ms: Histogram,
    stall_credit: Counter,
    timer_late: Histogram,
    gestures: Counter,
    gesture_drops: [Counter; 6],
    stances: Counter,
    inbox: GaugeVec,
    inbox_refused: [Counter; 6],
    inbox_over_reserve: Counter,
    attaches_deferred: Counter,
}

static GLOBAL: LazyLock<Global> = LazyLock::new(|| {
    let drops = metrics::counter_vec(
        "scacelith_gestures_dropped_total",
        "Gestures not relayed, by reason",
        &["reason"],
    );
    Global {
        active: metrics::gauge("scacelith_games_active", "Games in progress"),
        moves: metrics::counter("scacelith_game_moves_total", "Moves accepted"),
        move_us: metrics::histogram(
            "scacelith_game_move_processing_us",
            "Time to process one move intent, delivery and journaling included (microseconds)",
            &[5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0],
        ),
        rejected: metrics::counter_vec(
            "scacelith_game_rejects_total",
            "Game requests refused, by error code",
            &["code"],
        ),
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
        recovery_dropped: {
            let dropped = metrics::counter_vec(
                "scacelith_game_recovery_dropped_total",
                "Games of the journal dropped at start: already in the database (finished, whatever the journal said) or with a player the database does not know",
                &["reason"],
            );
            [RecoveryDrop::InDatabase, RecoveryDrop::UnknownPlayer].map(|r| dropped.with(&[r.as_str()]))
        },
        draining: metrics::gauge(
            "scacelith_game_shards_draining",
            "Game shards outside SHARD_BASE to SHARD_BASE + WORKERS - 1 served for the games their journal held at start",
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
        stances: metrics::counter(
            "scacelith_stances_relayed_total",
            "Stances (protocol minor 2) relayed to the opponent (a session of an older minor never receives them)",
        ),
        inbox: metrics::gauge_vec(
            "scacelith_game_inbox_messages",
            "Messages waiting in the inbox of a game host (set by its beat)",
            &["shard"],
        ),
        inbox_refused: {
            let refused = metrics::counter_vec(
                "scacelith_game_inbox_refused_total",
                "Messages refused at the door of a game host's inbox that held INBOX_MAX (32,768) messages, \
                 INBOX_MAX + INBOX_RESERVE for a request that ends a game: stance, rtt and rematch_decline \
                 dropped, request and ending answered Error{RateLimited} (the gestures are \
                 scacelith_gestures_dropped_total{reason=\"overload\"})",
                &["kind"],
            );
            Refusal::ALL.map(|r| refused.with(&[r.as_str()]))
        },
        inbox_over_reserve: metrics::counter(
            "scacelith_game_inbox_over_reserve_total",
            "Lifecycle messages (attach, detach, create, cancel, forfeit, stats, shutdown) delivered to a game \
             host whose inbox already held INBOX_MAX + INBOX_RESERVE messages (never refused)",
        ),
        attaches_deferred: metrics::counter(
            "scacelith_game_attach_deferred_total",
            "Attaches of a player's game not posted to its game host while an earlier connection's attach and \
             detach of that game still waited in its inbox (LINK_PENDING_MAX): the connection tries again",
        ),
    }
});

/// Sets the number of draining shards (at start).
pub(super) fn draining_shards(n: usize) {
    GLOBAL.draining.set(n as f64);
}

/// A gesture dropped before the inbox of its host (counted by the host's `Backlog`, which the
/// host's [`Counters`] include).
pub(super) fn gesture_overload() {
    let i = GestureDrop::ALL.iter().position(|&d| d == GestureDrop::Overload).unwrap_or(0);
    GLOBAL.gesture_drops[i].inc();
}

/// A message refused at the door of a host's inbox (counted by the host's `Backlog`, which the
/// host's [`Counters`] include).
pub(super) fn inbox_refused(kind: Refusal) {
    let i = Refusal::ALL.iter().position(|&r| r == kind).unwrap_or(0);
    GLOBAL.inbox_refused[i].inc();
}

/// A lifecycle message delivered beyond the inbox's reserve.
pub(super) fn inbox_over_reserve() {
    GLOBAL.inbox_over_reserve.inc();
}

/// An attach deferred to its connection (counted by the host's `Backlog`, which the host's
/// [`Counters`] include).
pub(super) fn attach_deferred() {
    GLOBAL.attaches_deferred.inc();
}

/// A game request refused at the door of a host's inbox, as the host would count a refusal
/// (`scacelith_game_rejects_total{code}`).
pub(super) fn busy_reject(code: ErrorCode) {
    GLOBAL.rejected.with(&[code.name()]).inc();
}

/// The gauge of the messages waiting in the inbox of a shard's host.
pub(super) fn inbox_gauge(shard: u32) -> Gauge {
    GLOBAL.inbox.with(&[&shard.to_string()])
}

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

    pub(super) fn recovery_dropped(&mut self, why: RecoveryDrop) {
        match why {
            RecoveryDrop::InDatabase => {
                GLOBAL.recovery_dropped[0].inc();
                self.counts.recovery_in_database += 1;
            }
            RecoveryDrop::UnknownPlayer => {
                GLOBAL.recovery_dropped[1].inc();
                self.counts.recovery_unknown_player += 1;
            }
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

    pub(super) fn stance_relayed(&mut self) {
        GLOBAL.stances.inc();
        self.counts.stances += 1;
    }
}
