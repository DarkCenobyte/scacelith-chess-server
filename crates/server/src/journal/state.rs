//! The state a journal handle and its I/O thread share, behind one mutex. Neither holds the lock
//! during a system call: the handle encodes records into the buffer, the thread takes whole
//! batches out of it and updates the bookkeeping between its writes, syncs and deletions.

use std::collections::HashSet;
use std::io;
use std::time::Instant;

use indexmap::IndexSet;
use parking_lot::{Condvar, Mutex};
use tokio::sync::oneshot;

use super::JournalError;
use super::books::{Books, GameQueue};
use super::format::{RecordKind, encode_record};
use super::metrics::RECORDS;
use crate::ids::GameId;

/// Initial capacity of a batch buffer.
const INITIAL_BUFFER: usize = 64 * 1024;

/// The answer to one flush.
pub(super) type Waiter = oneshot::Sender<Result<(), JournalError>>;

/// Replaces the write of a batch (fault injection, see [`super::Journal::set_write_batch_override`]).
pub type WriteBatchOverride = Box<dyn FnMut(&[u8]) -> io::Result<()> + Send>;

/// What the handle and the I/O thread share.
pub(super) struct Shared {
    pub state: Mutex<State>,
    /// Wakes the I/O thread (records to write, a flush, a close).
    pub wake: Condvar,
    pub write_override: Mutex<Option<WriteBatchOverride>>,
}

/// One batch handed to the I/O thread.
pub(super) struct Batch {
    pub buf: Vec<u8>,
    /// The games it mentions, in order.
    pub games: IndexSet<GameId>,
    /// The games whose `committed` record it holds.
    pub commits: Vec<GameId>,
    /// The games whose snapshot it holds.
    pub snaps: Vec<GameId>,
}

/// The shared state.
pub(super) struct State {
    /// Records appended and not handed to the I/O thread yet.
    pub buf: Vec<u8>,
    /// A written batch's buffer, reused for the next one.
    pub spare: Option<Vec<u8>>,
    /// When the first record of `buf` was appended.
    pub pending_since: Option<Instant>,
    pub batch_games: IndexSet<GameId>,
    pub batch_commits: Vec<GameId>,
    pub batch_snaps: Vec<GameId>,
    /// Games with a snapshot appended and not written yet.
    pub snaps_pending: HashSet<GameId>,
    /// Games to snapshot (compaction), in queue order.
    pub compact_queue: GameQueue,
    /// Games whose records a failed write lost: snapshotted first.
    pub heal_queue: GameQueue,
    /// Flushes covering `buf`.
    pub waiters: Vec<Waiter>,
    /// Flushes covering the batch being written.
    pub inflight: Vec<Waiter>,
    /// A batch a flush cut, not taken by the I/O thread yet.
    pub ready: Option<Batch>,
    /// A batch is cut and not settled yet.
    pub writing: bool,
    /// close() started: no more compaction, records are written at once.
    pub closing: bool,
    /// No more records accepted; the I/O thread exits once everything is written.
    pub closed: bool,
    /// The I/O thread has exited.
    pub stopped: bool,
    pub books: Books,
    /// Snapshots written.
    pub snapshots: u64,
    /// Batches whose write (or fsync) failed.
    pub failed_writes: u64,
    pub compact_segments: u64,
    pub compact_per_flush: usize,
}

impl State {
    pub fn new(compact_segments: u64, compact_per_flush: usize) -> State {
        State {
            buf: Vec::with_capacity(INITIAL_BUFFER),
            spare: None,
            pending_since: None,
            batch_games: IndexSet::new(),
            batch_commits: Vec::new(),
            batch_snaps: Vec::new(),
            snaps_pending: HashSet::new(),
            compact_queue: GameQueue::default(),
            heal_queue: GameQueue::default(),
            waiters: Vec::new(),
            inflight: Vec::new(),
            ready: None,
            writing: false,
            closing: false,
            closed: false,
            stopped: false,
            books: Books::default(),
            snapshots: 0,
            failed_writes: 0,
            compact_segments,
            compact_per_flush,
        }
    }

    /// Encodes one record into the buffer (the caller has validated it). Returns whether the
    /// buffer was empty before.
    pub fn append_record(&mut self, kind: RecordKind, game: GameId, payload: &[u8], at: f64) -> bool {
        let was_empty = self.buf.is_empty();
        if was_empty {
            self.pending_since = Some(Instant::now());
        }
        encode_record(&mut self.buf, kind, game, at, payload);
        self.batch_games.insert(game);
        match kind {
            RecordKind::Committed => self.batch_commits.push(game),
            RecordKind::Snapshot => {
                self.batch_snaps.push(game);
                self.snaps_pending.insert(game);
            }
            _ => {}
        }
        RECORDS.inc();
        was_empty
    }

    /// Cuts the buffer into a batch: the flushes waiting for it now wait for its write.
    pub fn take_batch(&mut self) -> Batch {
        let buf = std::mem::replace(
            &mut self.buf,
            self.spare.take().unwrap_or_else(|| Vec::with_capacity(INITIAL_BUFFER)),
        );
        self.pending_since = None;
        self.writing = true;
        self.inflight.append(&mut self.waiters);
        Batch {
            buf,
            games: std::mem::take(&mut self.batch_games),
            commits: std::mem::take(&mut self.batch_commits),
            snaps: std::mem::take(&mut self.batch_snaps),
        }
    }

    /// The batch is settled: returns the flushes to answer.
    pub fn settle(&mut self) -> Vec<Waiter> {
        self.writing = false;
        std::mem::take(&mut self.inflight)
    }

    /// Queues for a snapshot the games not committed that still need a segment
    /// `compact_segments` or more behind the newest one (at open() and at every rotation).
    pub fn enqueue_stale(&mut self) {
        for game in self.books.stale_games(self.compact_segments) {
            if !self.snaps_pending.contains(&game) {
                self.compact_queue.insert(game);
            }
        }
    }

    /// See [`super::Journal::compaction_candidates`].
    pub fn compaction_candidates(&mut self, max: usize) -> Vec<GameId> {
        if (self.compact_queue.is_empty() && self.heal_queue.is_empty()) || self.closing || self.closed {
            return Vec::new();
        }
        let room = max.min(self.compact_per_flush.saturating_sub(self.batch_snaps.len()));
        let mut out = Vec::new();
        // A game of a failed write: whatever its age, and possibly not tracked yet (its first
        // record was lost); a snapshot already pending supersedes the lost records too.
        while out.len() < room {
            let Some(game) = self.heal_queue.pop_front() else { break };
            self.compact_queue.remove(game);
            let committed = self.books.games.get(&game).is_some_and(|g| g.committed);
            if !committed && !self.snaps_pending.contains(&game) {
                out.push(game);
            }
        }
        let limit = self.books.seq.checked_sub(self.compact_segments);
        while out.len() < room {
            let Some(game) = self.compact_queue.pop_front() else { break };
            let stale = self
                .books
                .games
                .get(&game)
                .is_some_and(|g| !g.committed && limit.is_some_and(|l| g.first <= l));
            if stale && !self.snaps_pending.contains(&game) {
                out.push(game);
            }
        }
        out
    }
}
