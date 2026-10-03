//! The commit of finished games (DESIGN 5.5) and the journal gate in front of it.
//!
//! Finished games are committed with one `finish_batch` call at most `DB_COMMIT_MS` after the
//! first of them ended, one commit in flight at a time, retried with an exponential backoff
//! (from `max(100, DB_COMMIT_MS)` up to 10 s) while the journal keeps them. A batch is committed
//! only once the journal has written (and fsynced) every record appended before the batch was
//! chosen, their `ended` records included: a crash can then never find a game in the database
//! whose journal says it is still running.
//!
//! When a journal write failed since the last commit, or fails meanwhile, every game waiting for
//! its commit is marked (the failed write may have held its `ended` record) and journaled again
//! as a snapshot right before its own commit (the batch's games only, which bounds the work of
//! one call); in the second case the commit is retried later. At the [`JOURNAL_GATE_TRIES`]-th
//! failed flush in a row, or at the first one once shut down, the journal is considered down: the
//! batch is committed without waiting for it (one error logged per episode,
//! `scacelith_game_commit_unjournaled_total`), its snapshots and `committed` records still
//! appended, and each such commit starts one flush (the probe) that ends the episode when it
//! writes without a failure. The database is then the only durable copy of those results
//! (`finish_batch` ignores a game it already has, so a replay cannot apply it twice).
//!
//! When `finish_batch` fails because of one record (the error carries its game id), the batch is
//! committed one game at a time so that only that game stays pending (and in the journal). After
//! a commit: `RatingUpdate` to both players still attached, the journal's `committed` record,
//! then [`HostEvents::game_ended`](crate::events::HostEvents::game_ended). A finished room stays
//! until it is committed and its rematch window is closed.
//!
//! The anomalies of a game need no flush before its commit (the former server's
//! `anticheat.flush()`): [`AnomalySink::record`](crate::events::AnomalySink::record) queues them
//! on the store writer at once, ahead of the commit.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::time::Instant;

use scacelith_protocol::{GameStatus, Message, RatingUpdate};

use super::shard::{Shard, guarded, send};
use crate::events::GameEnded;
use crate::game::room::{GameRecord, NEVER};
use crate::game::rules::Side;
use crate::ids::GameId;
use crate::journal::JournalError;
use crate::log;
use crate::store::{CommitEntry, RatingChange, Store, StoreError};
use crate::{log_error, log_info};

/// Journal flushes failed in a row before commits stop waiting for the journal.
pub const JOURNAL_GATE_TRIES: u32 = 3;
/// Longest wait between two commit attempts.
pub const MAX_BACKOFF_MS: i64 = 10_000;

/// The future of a commit.
pub type CommitFuture = Pin<Box<dyn Future<Output = Result<Vec<CommitEntry>, StoreError>> + Send>>;

/// Where a host commits finished games: the [`Store`], or a test double.
pub trait GameStore: Send + Sync + 'static {
    /// Commits the games in one transaction (see [`Store::finish_batch`]); the work is queued
    /// when this is called, not when the future is first polled.
    fn finish_batch(&self, records: Vec<GameRecord>) -> CommitFuture;
}

impl GameStore for Store {
    fn finish_batch(&self, records: Vec<GameRecord>) -> CommitFuture {
        Box::pin(Store::finish_batch(self, records))
    }
}

/// Why a commit attempt failed.
#[derive(Debug)]
pub(super) enum CommitError {
    /// The journal flush the commit waited for failed.
    Journal(JournalError),
    /// A journal write failed while the commit waited for the journal.
    JournalWriteFailed,
    /// The database refused the batch.
    Store(StoreError),
    /// The task of the commit failed (a bug).
    TaskFailed,
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitError::Journal(e) => e.fmt(f),
            CommitError::JournalWriteFailed => f.write_str("journal write failed"),
            CommitError::Store(e) => e.fmt(f),
            CommitError::TaskFailed => f.write_str("commit task failed"),
        }
    }
}

impl std::error::Error for CommitError {}

/// What a task of the host waits for (to undo its effect if it fails).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TaskKind {
    /// A journal flush before a commit, or a commit.
    Commit,
    /// The flush that may end an episode of commits made without the journal.
    Probe,
    /// The lobby's answer to a rematch request of this game.
    Rematch(GameId),
}

/// The result of a task of the host.
#[derive(Debug)]
pub(super) enum Done {
    /// The journal flush a batch waited for (`failures`: the journal's failed writes before it).
    Flushed { batch: Vec<GameId>, failures: u64, result: Result<(), JournalError> },
    /// A batch commit.
    Committed { batch: Vec<GameId>, unjournaled: bool, started: Instant, result: Result<Vec<CommitEntry>, StoreError> },
    /// The games of a refused batch committed one by one (with each commit's duration in ms).
    CommittedEach { results: Vec<(GameId, f64, Result<Vec<CommitEntry>, StoreError>)>, unjournaled: bool },
    /// The probe flush.
    Probe { failures: u64, result: Result<(), JournalError> },
    /// The lobby's answer to a rematch request (`None`: it dropped the request).
    Rematch { game: GameId, answer: Option<Result<GameId, scacelith_protocol::ErrorCode>> },
}

/// A rating for `RatingUpdate` (clamped to the protocol's ranges).
fn rating_change(r: &RatingChange) -> scacelith_protocol::RatingChange {
    scacelith_protocol::RatingChange {
        before: r.before.clamp(0, i64::from(u16::MAX)) as u16,
        after: r.after.clamp(0, i64::from(u16::MAX)) as u16,
        games: r.games.clamp(0, i64::from(u32::MAX)) as u32,
        provisional: r.provisional,
    }
}

impl Shard {
    /// Starts a commit when one is due at `t` (the beat calls it); returns whether one started.
    pub fn poll_commits(&mut self, t: i64) -> bool {
        if self.pending.is_empty() || self.commit_in_flight || t < self.next_commit_at {
            return false;
        }
        self.commit(t);
        true
    }

    /// Whether a commit (or the journal flush it waits for) is in flight.
    #[must_use]
    pub fn commit_in_flight(&self) -> bool {
        self.commit_in_flight
    }

    /// Finished games waiting for their commit.
    #[must_use]
    pub fn pending_commits(&self) -> usize {
        self.pending.len()
    }

    /// The wait before the next commit attempt after a failure (0 after a success).
    #[must_use]
    pub fn backoff_ms(&self) -> i64 {
        self.backoff_ms
    }

    /// Whether commits are made without waiting for the journal (its writes keep failing).
    #[must_use]
    pub fn unjournaled(&self) -> bool {
        self.unjournaled
    }

    /// Whether the probe flush is in flight.
    #[must_use]
    pub fn probe_in_flight(&self) -> bool {
        self.probe_in_flight
    }

    /// Waits until no commit is in flight; returns the result of the latest attempt.
    pub async fn settle_commit(&mut self) -> bool {
        while self.commit_in_flight {
            if !self.next_task().await {
                break;
            }
        }
        self.last_commit.unwrap_or(false)
    }

    /// Queues a finished game for its commit (`DB_COMMIT_MS` after the first of a batch).
    pub(super) fn queue_commit(&mut self, game: GameId) {
        let now = self.now();
        let idle = self.pending.is_empty() && !self.commit_in_flight && self.backoff_ms == 0;
        let Some(entry) = self.rooms.get_mut(&game) else { return };
        if entry.queued || entry.committed {
            return;
        }
        if idle {
            self.next_commit_at = now + self.settings.commit_ms;
        }
        entry.queued = true;
        self.pending.insert(game);
    }

    /// One commit attempt of the pending games (see the module documentation).
    pub(super) fn commit(&mut self, t: i64) {
        self.last_commit = None;
        let batch: Vec<GameId> = self.pending.iter().take(self.settings.commit_batch_max.max(1)).copied().collect();
        if batch.is_empty() {
            self.last_commit = Some(true);
            return;
        }
        let Some(journal) = &self.journal else { return self.commit_batch(batch, t, false) };
        if self.gate_failures >= JOURNAL_GATE_TRIES {
            if journal.has_unwritten() || journal.failed_writes() != self.journal_failures {
                return self.commit_unjournaled(batch, t, None);
            }
            // Everything appended so far is written, without a failure.
            self.journal_works();
        }
        self.journal_again(t, &batch, false);
        let Some(journal) = &self.journal else { return };
        if !journal.has_unwritten() {
            return self.commit_batch(batch, t, false);
        }
        let failures = journal.failed_writes();
        let flush = journal.flush();
        self.commit_in_flight = true;
        self.spawn(TaskKind::Commit, async move { Done::Flushed { batch, failures, result: flush.await } });
    }

    /// Applies the result of a task.
    pub(super) fn on_done(&mut self, done: Done) {
        let now = self.now();
        match done {
            Done::Flushed { batch, failures, result } => {
                self.commit_in_flight = false;
                let err = match result {
                    Ok(()) if self.failed_writes() == failures => None,
                    Ok(()) => Some(CommitError::JournalWriteFailed),
                    Err(e) => Some(CommitError::Journal(e)),
                };
                let Some(err) = err else {
                    self.journal_works();
                    return self.commit_batch(batch, now, false);
                };
                self.gate_failures += 1;
                if self.closed || self.gate_failures >= JOURNAL_GATE_TRIES {
                    return self.commit_unjournaled(batch, now, Some(err));
                }
                self.journal_again(now, &batch, true);
                self.commit_failed(&err, batch.len(), now);
                self.last_commit = Some(false);
            }
            Done::Committed { batch, unjournaled, started, result } => {
                self.commit_in_flight = false;
                match result {
                    Ok(entries) => {
                        let ms = started.elapsed().as_secs_f64() * 1000.0;
                        self.commit_done(&batch, &entries, ms, now, unjournaled);
                        self.last_commit = Some(true);
                    }
                    // One bad record must not hold the others: one game at a time.
                    Err(e) if batch.len() > 1 && e.game_id().is_some() => self.commit_each(batch, unjournaled),
                    Err(e) => {
                        self.commit_failed(&CommitError::Store(e), batch.len(), now);
                        self.last_commit = Some(false);
                    }
                }
            }
            Done::CommittedEach { results, unjournaled } => {
                self.commit_in_flight = false;
                let mut failed = None;
                for (game, ms, result) in results {
                    match result {
                        Ok(entries) => self.commit_done(&[game], &entries, ms, now, unjournaled),
                        Err(e) => failed = Some(e),
                    }
                }
                if let Some(e) = failed {
                    self.commit_failed(&CommitError::Store(e), self.pending.len(), now);
                    self.last_commit = Some(false);
                } else {
                    self.last_commit = Some(true);
                }
            }
            Done::Probe { failures, result } => {
                self.probe_in_flight = false;
                if result.is_ok() && self.failed_writes() == failures && self.gate_failures >= JOURNAL_GATE_TRIES {
                    self.journal_works();
                }
            }
            Done::Rematch { game, answer } => self.rematch_answered(game, answer),
        }
    }

    fn failed_writes(&self) -> u64 {
        self.journal.as_ref().map_or(0, |j| j.failed_writes())
    }

    /// Journals again, one snapshot each, the games of `batch` whose `ended` record may have been
    /// lost with a failed journal write (all of them when `all`). A failure seen for the first
    /// time marks every game waiting for its commit: those not in this batch are journaled again
    /// before their own commit.
    fn journal_again(&mut self, t: i64, batch: &[GameId], all: bool) {
        let failures = self.failed_writes();
        if failures != self.journal_failures {
            self.journal_failures = failures;
            for game in &self.pending {
                if let Some(entry) = self.rooms.get_mut(game) {
                    entry.rejournal = true;
                }
            }
        }
        for &game in batch {
            let Some(entry) = self.rooms.get_mut(&game) else { continue };
            if !entry.rejournal && !all {
                continue;
            }
            entry.rejournal = false;
            let Some(rec) = guarded(&self.log, game, "journal snapshot failed", || entry.room.journal_snapshot(t))
            else {
                continue;
            };
            self.append(game, rec);
        }
    }

    /// The journal's writes keep failing (or the host is shutting down): the batch is committed
    /// without waiting for them; its snapshots are still appended, and one flush at a time tells
    /// when the journal works again.
    fn commit_unjournaled(&mut self, batch: Vec<GameId>, t: i64, err: Option<CommitError>) {
        self.journal_again(t, &batch, true);
        if !self.unjournaled {
            self.unjournaled = true;
            log_error!(self.log, "journal writes keep failing: finished games are committed without waiting for the journal", {
                "err": err.as_ref().map(log::error),
                "failedWrites": self.failed_writes(),
                "games": batch.len(),
                "pending": self.pending.len(),
            });
        }
        self.commit_batch(batch, t, true);
        self.probe_journal();
    }

    /// One journal flush at a time while commits do not wait for the journal: the first one that
    /// writes without a failure ends the episode.
    fn probe_journal(&mut self) {
        if self.probe_in_flight || self.closed {
            return;
        }
        let Some(journal) = &self.journal else { return };
        let failures = journal.failed_writes();
        let flush = journal.flush();
        self.probe_in_flight = true;
        self.spawn(TaskKind::Probe, async move { Done::Probe { failures, result: flush.await } });
    }

    /// A journal flush wrote without a failure: commits wait for the journal again.
    fn journal_works(&mut self) {
        self.gate_failures = 0;
        if self.unjournaled {
            self.unjournaled = false;
            log_info!(self.log, "journal writes succeed again: finished games wait for the journal before their commit", {
                "failedWrites": self.failed_writes(),
            });
        }
    }

    /// The records of the games of `batch` (a game without one cannot be committed: it leaves
    /// the queue, logged).
    fn records(&mut self, batch: Vec<GameId>) -> (Vec<GameId>, Vec<GameRecord>) {
        let mut ids = Vec::with_capacity(batch.len());
        let mut records = Vec::with_capacity(batch.len());
        for game in batch {
            let record = self
                .rooms
                .get(&game)
                .and_then(|e| guarded(&self.log, game, "game record failed", || e.room.record()).flatten());
            match record {
                Some(r) => {
                    ids.push(game);
                    records.push(r);
                }
                None => {
                    log_error!(self.log, "finished game without a record: not committed", { "gameId": game });
                    self.pending.shift_remove(&game);
                    if let Some(entry) = self.rooms.get_mut(&game) {
                        entry.queued = false;
                    }
                }
            }
        }
        (ids, records)
    }

    fn commit_batch(&mut self, batch: Vec<GameId>, t: i64, unjournaled: bool) {
        let (batch, records) = self.records(batch);
        if batch.is_empty() {
            self.next_commit_at = if self.pending.is_empty() { NEVER } else { t };
            self.last_commit = Some(true);
            return;
        }
        let started = Instant::now();
        let commit = self.store.finish_batch(records);
        self.commit_in_flight = true;
        self.spawn(TaskKind::Commit, async move { Done::Committed { batch, unjournaled, started, result: commit.await } });
    }

    /// The games of a refused batch, one commit each (queued at once, in order).
    fn commit_each(&mut self, batch: Vec<GameId>, unjournaled: bool) {
        let (batch, records) = self.records(batch);
        let commits: Vec<_> = batch
            .into_iter()
            .zip(records)
            .map(|(game, record)| (game, Instant::now(), self.store.finish_batch(vec![record])))
            .collect();
        self.commit_in_flight = true;
        self.spawn(TaskKind::Commit, async move {
            let mut results = Vec::with_capacity(commits.len());
            for (game, started, commit) in commits {
                let result = commit.await;
                results.push((game, started.elapsed().as_secs_f64() * 1000.0, result));
            }
            Done::CommittedEach { results, unjournaled }
        });
    }

    fn commit_done(&mut self, batch: &[GameId], entries: &[CommitEntry], ms: f64, t: i64, unjournaled: bool) {
        self.meter.committed(batch.len(), ms, unjournaled);
        let by_id: HashMap<GameId, &CommitEntry> = entries.iter().map(|e| (e.game_id, e)).collect();
        let done: HashSet<GameId> = batch.iter().copied().collect();
        self.pending.retain(|g| !done.contains(g));
        for &game in batch {
            let Some(entry) = self.rooms.get_mut(&game) else { continue };
            entry.queued = false;
            entry.committed = true;
            entry.rejournal = false;
            self.meter.counts.committed += 1;
            let room = &entry.room;
            if let Some(ratings) = by_id.get(&game).and_then(|e| e.ratings) {
                let update = RatingUpdate {
                    game,
                    category: room.category().to_owned(),
                    white: rating_change(&ratings.white),
                    black: rating_change(&ratings.black),
                };
                match update.to_bytes() {
                    Ok(frame) => {
                        send(entry.ep[0].as_ref(), &frame);
                        send(entry.ep[1].as_ref(), &frame);
                    }
                    Err(e) => log_error!(self.log, "RatingUpdate not encodable", { "gameId": game, "err": e.to_string() }),
                }
            }
            let ended = room.result().map(|r| GameEnded {
                game,
                white: room.player(Side::White).user_id,
                black: room.player(Side::Black).user_id,
                status: r.status,
                reason: r.reason,
                rated: room.rated() && r.status != GameStatus::Aborted,
                category: room.category().to_owned(),
            });
            self.journal_committed(game);
            if let Some(ended) = ended {
                self.events.game_ended(ended);
            }
            self.reschedule(game);
        }
        self.backoff_ms = 0;
        self.next_commit_at = if self.pending.is_empty() { NEVER } else { t };
    }

    pub(super) fn commit_failed(&mut self, err: &CommitError, games: usize, t: i64) {
        self.meter.commit_failed();
        self.backoff_ms = if self.backoff_ms > 0 {
            (self.backoff_ms * 2).min(MAX_BACKOFF_MS)
        } else {
            self.settings.commit_ms.max(100)
        };
        self.next_commit_at = t + self.backoff_ms;
        log_error!(self.log, "commit of finished games failed; retrying", {
            "err": log::error(err),
            "games": games,
            "retryInMs": self.backoff_ms,
        });
    }

    /// Stops the timers, commits what is pending (five attempts at most) and closes the journal
    /// (its final flush; a failure is logged). A batch whose journal flush fails is committed
    /// without waiting for the journal once shut down.
    pub async fn shutdown(&mut self) {
        self.closed = true;
        self.stop_beat();
        for _ in 0..5 {
            if self.pending.is_empty() && !self.commit_in_flight {
                break;
            }
            if self.commit_in_flight {
                self.settle_commit().await;
                continue;
            }
            let t = self.now();
            self.commit(t);
            if !self.settle_commit().await {
                // The database refused them: the journal keeps them for the next start.
                break;
            }
        }
        if let Some(journal) = &self.journal
            && let Err(e) = journal.close().await
        {
            log_error!(self.log, "journal flush failed at shutdown", {
                "err": log::error(&e),
                "pendingCommits": self.pending.len(),
            });
        }
        self.tasks.abort_all();
    }
}
