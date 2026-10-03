//! The anti-cheat service of the server: anomaly classification and recording, and the automatic
//! sanction of certain protocol cheats ([`Anticheat`], the [`AnomalySink`] of the hosts and the
//! connections).
//!
//! Every write goes through the store's single writer thread, which runs its jobs in submission
//! order: the anomalies recorded right before a commit of finished games are written before it
//! (the commit's analysis queue policy sees them), and a certain anomaly before the ban it causes.
//!
//! Recording never waits. The rows of non-certain anomalies wait in a buffer drained by one store
//! job, queued by the first anomaly that finds no job waiting: repeats of the same kind by the
//! same player in the same game while the job waits are coalesced into one row (`count`,
//! `lastAt`), so a flood of anomalies costs one row per writer turn, not one per message. A
//! certain anomaly gets a job of its own.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value};

use super::refunds::log_refunds;
use super::sanction::{AppliedSanction, CertainCheat, SanctionSettings, apply_certain_sanction};
use crate::clock::SharedClock;
use crate::config::Config;
use crate::events::{Anomaly, AnomalySink, Noop, SanctionApplied, SanctionEvents};
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::metrics::{self, Counter, CounterVec};
use crate::store::{NewAnomaly, Severity, Store, StoreError};
use crate::util::js;
use crate::{log_debug, log_error, log_security};

/// Severity of every known anomaly kind (DESIGN 6.5).
pub const ANOMALY_KINDS: [(&str, Severity); 12] = [
    ("malformed", Severity::Suspicious),
    ("forged_type", Severity::Certain),
    ("bad_seq", Severity::Suspicious),
    ("flood", Severity::Suspicious),
    ("foreign_game", Severity::Certain),
    ("out_of_turn", Severity::Certain),
    ("illegal_move", Severity::Certain),
    ("repeated_desync", Severity::Suspicious),
    ("clock_implausible", Severity::Suspicious),
    ("stale_ply", Severity::Info),
    ("desync", Severity::Info),
    ("nothing_to_claim", Severity::Info),
];

/// Kinds that are only certain when the client provably knew the position (its position hash
/// matched).
const NEEDS_SYNC: [&str; 2] = ["out_of_turn", "illegal_move"];

/// Distinct buffered rows past which info anomalies are dropped (twice as many for the others).
pub const MAX_PENDING: usize = 5000;
/// Remembered sanctions (player, game) past which the expired ones are forgotten.
const MAX_SANCTIONED: usize = 10_000;
const HOUR_MS: i64 = 3_600_000;

/// How an anomaly is classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classification {
    pub severity: Severity,
    /// A certain cheat (`severity` is `certain`).
    pub certain: bool,
    /// The kind is one of [`ANOMALY_KINDS`].
    pub known: bool,
}

/// Classifies an anomaly. Unknown kinds are `info` (a caller bug must not sanction anyone).
/// `out_of_turn` and `illegal_move` reported with `pos_matched == Some(false)` are downgraded to
/// `suspicious`: without a synchronised position they can be an honest race.
pub fn classify(kind: &str, pos_matched: Option<bool>) -> Classification {
    let Some(&(_, base)) = ANOMALY_KINDS.iter().find(|(k, _)| *k == kind) else {
        return Classification { severity: Severity::Info, certain: false, known: false };
    };
    let severity = if base == Severity::Certain && NEEDS_SYNC.contains(&kind) && pos_matched == Some(false) {
        Severity::Suspicious
    } else {
        base
    };
    Classification { severity, certain: severity == Severity::Certain, known: true }
}

/// What [`Anticheat::sanction`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SanctionResult {
    /// End of the ban standing for the cheat (0: none, the sanction is off or failed).
    pub ban_until: i64,
    /// A new ban was created by this call.
    pub applied: bool,
    /// Victims whose rating losses were refunded now.
    pub refunds: u32,
}

struct Metrics {
    anomalies: CounterVec,
    dropped: Counter,
    sanctions: CounterVec,
}

static METRICS: LazyLock<Metrics> = LazyLock::new(|| Metrics {
    anomalies: metrics::counter_vec(
        "scacelith_anticheat_anomalies_total",
        "Anomalies recorded, by kind and severity",
        &["kind", "severity"],
    ),
    dropped: metrics::counter(
        "scacelith_anticheat_anomalies_dropped_total",
        "Anomalies not persisted (buffer full or database error)",
    ),
    sanctions: metrics::counter_vec(
        "scacelith_anticheat_sanctions_total",
        "Automatic sanctions applied",
        &["kind"],
    ),
});

/// A buffered anomaly row and its repeats.
struct PendingRow {
    row: NewAnomaly,
    count: u64,
    last_at: i64,
}

impl PendingRow {
    fn into_row(self) -> NewAnomaly {
        let mut row = self.row;
        if self.count > 1 {
            let mut d = match row.detail.take() {
                Some(Value::Object(m)) => m,
                _ => Map::new(),
            };
            d.insert("count".into(), Value::from(self.count));
            d.insert("lastAt".into(), Value::from(self.last_at));
            row.detail = Some(Value::Object(d));
        }
        row
    }
}

type PendingKey = (UserId, GameId, &'static str);

#[derive(Default)]
struct Pending {
    rows: IndexMap<PendingKey, PendingRow>,
    /// The drain job waiting on the writer, if any (its generation).
    queued: Option<u64>,
    generation: u64,
}

impl Pending {
    fn take(&mut self) -> Vec<NewAnomaly> {
        self.queued = None;
        std::mem::take(&mut self.rows).into_values().map(PendingRow::into_row).collect()
    }
}

struct Inner {
    store: Store,
    clock: SharedClock,
    logger: Logger,
    auto_sanction: bool,
    settings: SanctionSettings,
    events: RwLock<Arc<dyn SanctionEvents>>,
    pending: Mutex<Pending>,
    /// (player, game) -> end of the ban standing for a certain cheat of that game.
    sanctioned: Mutex<HashMap<(UserId, GameId), i64>>,
}

/// The anti-cheat service (module documentation). Cheap to clone.
#[derive(Clone)]
pub struct Anticheat {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Anticheat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Anticheat")
            .field("auto_sanction", &self.inner.auto_sanction)
            .field("settings", &self.inner.settings)
            .finish_non_exhaustive()
    }
}

impl Anticheat {
    /// The service of a server: `AUTO_SANCTION_CERTAIN_CHEATS`, `BAN_DURATION_HOURS` and
    /// `RATING_REFUND_DAYS` from `config`, the wall clock of the anomaly and ban times. The
    /// sanctions are announced to [`Noop`] until [`Anticheat::set_sanction_events`] is called
    /// (the lobby is built after the hosts, which need this service).
    pub fn new(config: &Config, store: Store, clock: SharedClock) -> Anticheat {
        Anticheat {
            inner: Arc::new(Inner {
                store,
                clock,
                logger: Logger::root().child("anticheat"),
                auto_sanction: config.auto_sanction_certain_cheats,
                settings: SanctionSettings {
                    ban_hours: config.ban_duration_hours,
                    refund_days: config.rating_refund_days,
                },
                events: RwLock::new(Arc::new(Noop)),
                pending: Mutex::new(Pending::default()),
                sanctioned: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Where sanctions and refunds are announced (the lobby: kick, forfeit, refund notices).
    pub fn set_sanction_events(&self, events: Arc<dyn SanctionEvents>) {
        *self.inner.events.write() = events;
    }

    fn events(&self) -> Arc<dyn SanctionEvents> {
        self.inner.events.read().clone()
    }

    /// Buffered anomaly rows not handed to the writer thread yet (tests, diagnostics).
    pub fn pending_count(&self) -> usize {
        self.inner.pending.lock().rows.len()
    }

    /// Records an anomaly: metrics, log, and its row queued on the store writer before this
    /// returns (certain ones in a job of their own, the others coalesced in the drain job).
    pub fn record_anomaly(&self, a: &Anomaly) -> Classification {
        let c = classify(a.kind, Some(a.pos_matched));
        let label: &'static str = if c.known { a.kind } else { "unknown" };
        METRICS.anomalies.with(&[label, c.severity.as_str()]).inc();
        let at = self.inner.clock.wall_ms();
        let detail = anomaly_detail(a, c.known);
        let key = (a.user, a.game, label);
        let row = NewAnomaly {
            user_id: Some(a.user),
            game_id: Some(a.game),
            kind: label.to_string(),
            severity: c.severity,
            at: Some(at),
            detail: Some(Value::Object(detail)),
        };
        let lg = &self.inner.logger;
        if c.certain {
            log_security!(lg, "anomaly", { "userId": a.user, "gameId": a.game, "kind": label,
                "severity": c.severity.as_str(), "detail": row.detail });
            self.write_rows(vec![row], "certain anomaly not persisted");
            return c;
        }

        let mut pending = self.inner.pending.lock();
        // Repeats waiting for the writer are counted, not logged again (no log flooding).
        if c.severity == Severity::Info {
            log_debug!(lg, "anomaly", { "userId": a.user, "gameId": a.game, "kind": label,
                "severity": c.severity.as_str() });
        } else if !pending.rows.contains_key(&key) {
            log_security!(lg, "anomaly", { "userId": a.user, "gameId": a.game, "kind": label,
                "severity": c.severity.as_str(), "detail": row.detail });
        }
        let len = pending.rows.len();
        if let Some(prev) = pending.rows.get_mut(&key) {
            prev.count += 1;
            prev.last_at = at;
        } else if (len >= MAX_PENDING && c.severity == Severity::Info) || len >= 2 * MAX_PENDING {
            METRICS.dropped.inc();
        } else {
            pending.rows.insert(key, PendingRow { row, count: 1, last_at: at });
        }
        if pending.queued.is_none() && !pending.rows.is_empty() {
            pending.generation += 1;
            let generation = pending.generation;
            pending.queued = Some(generation);
            // Queued under the lock: the drain jobs keep the order of the anomalies.
            self.queue_drain(generation);
        }
        c
    }

    /// Queues the job that writes every buffered row when the writer runs it.
    fn queue_drain(&self, generation: u64) {
        let inner = self.inner.clone();
        let fut = self.inner.store.write(move |db| {
            let rows = inner.pending.lock().take();
            let n = rows.len();
            db.anomalies().insert_batch(&rows).map_err(|e| BatchError { rows: n, error: e })?;
            Ok::<_, BatchError>(())
        });
        let inner = self.inner.clone();
        spawn_detached(async move {
            if let Err(mut e) = fut.await {
                // The job did not run (store closed, writer lock not obtained): its rows are lost.
                let mut pending = inner.pending.lock();
                if pending.queued == Some(generation) {
                    e.rows = pending.take().len();
                }
                drop(pending);
                lost(&inner.logger, "anomaly batch lost", &e);
            }
        });
    }

    fn write_rows(&self, rows: Vec<NewAnomaly>, what: &'static str) {
        let n = rows.len();
        let fut = self.inner.store.write(move |db| {
            db.anomalies().insert_batch(&rows).map_err(|e| BatchError { rows: n, error: e })?;
            Ok::<_, BatchError>(())
        });
        let logger = self.inner.logger.clone();
        spawn_detached(async move {
            if let Err(mut e) = fut.await {
                e.rows = n;
                lost(&logger, what, &e);
            }
        });
    }

    /// Automatic sanction of a certain cheat: a ban of `BAN_DURATION_HOURS` (source `auto`),
    /// integrity level `confirmed` with the evidence appended, a security event, the rating
    /// refunds of the player's victims ([`apply_certain_sanction`], one store job queued before
    /// this returns), then [`SanctionEvents::sanction_applied`] for a new ban and
    /// [`SanctionEvents::refunds_pending`] when refunds were given. Idempotent within a game:
    /// several certain anomalies of one game make one ban, also while the first one is being
    /// written. Does nothing when `AUTO_SANCTION_CERTAIN_CHEATS` is off.
    ///
    /// The follow-up (metrics, logs, events) runs when the returned future completes;
    /// [`AnomalySink::sanction_certain`] spawns it.
    pub fn sanction(
        &self,
        user: UserId,
        game: GameId,
        kind: &str,
    ) -> impl Future<Output = SanctionResult> + Send + 'static + use<> {
        let submitted = self.submit_sanction(user, game, kind);
        let inner = self.inner.clone();
        let events = self.events();
        let kind = kind.to_string();
        async move {
            let (key, fut) = match submitted {
                Ok(s) => s,
                Err(ready) => return ready,
            };
            match fut.await {
                Ok(r) => sanction_done(&inner, &*events, key, &kind, r),
                Err(e) => {
                    // Not stored (nothing else was written): a later certain anomaly of this game
                    // tries again.
                    inner.sanctioned.lock().remove(&key);
                    log_error!(inner.logger, "automatic ban not stored", { "err": crate::log::error(&e),
                        "userId": user, "kind": kind });
                    SanctionResult::default()
                }
            }
        }
    }

    /// The synchronous part of [`Anticheat::sanction`]: the idempotency check and the store job.
    #[allow(clippy::type_complexity)]
    fn submit_sanction(
        &self,
        user: UserId,
        game: GameId,
        kind: &str,
    ) -> Result<
        (
            (UserId, GameId),
            impl Future<Output = Result<AppliedSanction, StoreError>> + Send + 'static + use<>,
        ),
        SanctionResult,
    > {
        let inner = &self.inner;
        if !inner.auto_sanction {
            return Err(SanctionResult::default());
        }
        let t = inner.clock.wall_ms();
        let key = (user, game);
        {
            let mut sanctioned = inner.sanctioned.lock();
            if let Some(&seen) = sanctioned.get(&key)
                && seen > t
            {
                return Err(SanctionResult { ban_until: seen, applied: false, refunds: 0 });
            }
            if sanctioned.len() > MAX_SANCTIONED {
                sanctioned.retain(|_, until| *until > t);
            }
            // Taken at once (the writer answers later): the next certain anomaly of this game is
            // a repeat.
            sanctioned.insert(key, t + inner.settings.ban_hours * HOUR_MS);
        }
        let cheat = CertainCheat { user, game, kind: kind.to_string(), at: t };
        let (settings, logger) = (inner.settings, inner.logger.clone());
        let fut = inner.store.write(move |db| apply_certain_sanction(db, settings, &cheat, &logger));
        Ok((key, fut))
    }
}

/// The follow-up of a stored sanction, after the commit.
fn sanction_done(
    inner: &Inner,
    events: &dyn SanctionEvents,
    key: (UserId, GameId),
    kind: &str,
    r: AppliedSanction,
) -> SanctionResult {
    let (user, game) = key;
    inner.sanctioned.lock().insert(key, r.until);
    let refunds = u32::try_from(r.refunds.len()).unwrap_or(u32::MAX);
    if let Some(request) = &r.refund_request {
        log_refunds(&inner.logger, request, &r.given);
    }
    if r.created {
        let label = if ANOMALY_KINDS.iter().any(|(k, _)| *k == kind) { kind } else { "unknown" };
        METRICS.sanctions.with(&[label]).inc();
        log_security!(inner.logger, "sanction.auto", { "userId": user, "gameId": game, "kind": kind,
            "until": r.until, "refunds": refunds });
        events.sanction_applied(SanctionApplied {
            user,
            until: r.until,
            reason: format!("{}{kind}", super::refunds::reason::CERTAIN),
            refunds,
        });
    }
    if !r.given.is_empty() {
        events.refunds_pending();
    }
    SanctionResult { ban_until: r.until, applied: r.created, refunds }
}

impl AnomalySink for Anticheat {
    fn record(&self, anomaly: Anomaly) {
        self.record_anomaly(&anomaly);
    }

    fn sanction_certain(&self, user: UserId, game: GameId, kind: &'static str) {
        let fut = self.sanction(user, game, kind);
        spawn_detached(async move {
            fut.await;
        });
    }
}

/// The stored detail of an anomaly: the caller's object (a scalar as `{info}`, an array by index),
/// then `posMatched`, then `reportedKind` for an unknown kind (40 UTF-16 units).
fn anomaly_detail(a: &Anomaly, known: bool) -> Map<String, Value> {
    let mut d = match &a.detail {
        Value::Null => Map::new(),
        Value::Object(m) => m.clone(),
        Value::Array(items) => items.iter().enumerate().map(|(i, v)| (i.to_string(), v.clone())).collect(),
        Value::String(s) => Map::from_iter([("info".to_string(), Value::String(s.clone()))]),
        Value::Bool(b) => Map::from_iter([("info".to_string(), Value::String(b.to_string()))]),
        Value::Number(n) => {
            let text = n.as_f64().map_or_else(|| n.to_string(), js::number_to_string);
            Map::from_iter([("info".to_string(), Value::String(text))])
        }
    };
    d.insert("posMatched".into(), Value::Bool(a.pos_matched));
    if !known {
        d.insert("reportedKind".into(), Value::String(js::truncate_utf16(a.kind, 40).to_string()));
    }
    d
}

/// A failed anomaly write and the rows it lost.
#[derive(Debug)]
struct BatchError {
    rows: usize,
    error: StoreError,
}

impl From<StoreError> for BatchError {
    fn from(error: StoreError) -> BatchError {
        BatchError { rows: 0, error }
    }
}

fn lost(logger: &Logger, what: &str, e: &BatchError) {
    METRICS.dropped.add(e.rows as u64);
    log_error!(logger, what, { "err": crate::log::error(&e.error), "rows": e.rows });
}

/// Runs a follow-up task on the current runtime. Outside a runtime (tools), the follow-up is
/// skipped: the store job it would wait for runs anyway.
fn spawn_detached(fut: impl Future<Output = ()> + Send + 'static) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
    }
}

#[cfg(test)]
mod tests;
