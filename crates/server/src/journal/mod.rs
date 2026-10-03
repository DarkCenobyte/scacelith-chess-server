//! Game journal of one host shard: append-only segments, group flush, compaction snapshots,
//! recovery at start and the commit gate's failure accounting. See docs/RUST-PORT.md (6.3) and
//! DESIGN.md (5.6).
//!
//! Crash safety of the games in progress without a database write per move. One journal per
//! shard, `JOURNAL_DIR/shard-<n>/segment-<seq>.log`, in the native format of [`mod@format`]
//! (CRC-32C per record).
//!
//! # Writing
//!
//! [`Journal::append`] encodes a record into an in-memory buffer (no allocation per record beyond
//! the buffer's occasional growth). The shard's I/O thread writes the whole buffer with one
//! `write` (+ `fdatasync` when `JOURNAL_FSYNC` is on), one batch in flight: records appended
//! while a batch is written go to the next one (group commit). A batch is written
//! `JOURNAL_FLUSH_MS` after its first record, or at once when [`Journal::flush`] is called (the
//! batch is cut at that call: later records go to the next batch); the flush's future resolves
//! once the batch is written and fsynced and the deletions it allowed are done. Segments rotate
//! once they reach the segment size (checked before each batch, so a segment exceeds it by at
//! most one batch; records never span segments). Existing segments are never appended to: the
//! first batch after open() starts a new segment.
//!
//! # Deleting
//!
//! Per segment the journal knows the games it mentions and how many of them still need it; per
//! game, the segments that mention it, the first segment it needs (the one of its first record,
//! or of its latest snapshot) and the one holding its `committed` record. A game not committed
//! needs every segment from its first one on. A segment is deleted when no game needs it any more
//! (the records that release it, `committed` or `snapshot`, durably written), and a segment
//! holding a game's `committed` record outlives every other segment that mentions that game, so a
//! later recovery can never see a committed game's records without its `committed` record.
//!
//! # Compaction
//!
//! A `snapshot` record holds the whole state of one game (the game module's encoding) and
//! supersedes every earlier record of that game. When the journal starts segment N, the games not
//! committed whose first needed segment is N - `JOURNAL_COMPACT_SEGMENTS` or older are queued; the
//! host takes a few at a time ([`Journal::compaction_candidates`], at most
//! `compact_per_flush` snapshots per batch) and appends their snapshot. Once the batch holding a
//! snapshot is durable, its segment becomes the game's first needed one, and the older segments
//! go by the rule above, so a shard's journal stays at about `JOURNAL_COMPACT_SEGMENTS + 1`
//! segments however long its games last. Until then every older segment is still on disk (a torn
//! snapshot is ignored like any torn record, and the game replays from its older records); after
//! it, recovery starts the game from its latest snapshot and drops the records before it,
//! wherever they are (a crash in the middle of the deletions leaves some behind).
//!
//! # Durability of the deletions
//!
//! With fsync on (a power loss, not only a process crash): a batch is fdatasynced before the
//! bookkeeping that releases segments runs, so the record that releases a segment is durable
//! before that segment is unlinked. open() fdatasyncs every segment it reads, then the directory,
//! before it deletes anything (a record found at start-up may live only in the page cache of a
//! process that died). A segment holding a `committed` record is unlinked only once the unlinks
//! before it are durable (a directory fsync in between, about once per rotation). With fsync off,
//! a batch holding a snapshot is still fdatasynced before its bookkeeping runs (after a directory
//! fsync when its segment was created since the last one), and open() makes the segments holding
//! a snapshot durable before it deletes anything: a snapshot deletes older records of a running
//! game, so a power loss must not keep the deletions and lose the snapshot.
//!
//! # Write failures
//!
//! After a failed write, the next batch goes to a new segment; the batch's `committed` records are
//! appended again (the host has forgotten those games). Every other game of the batch lost records
//! and goes to the heal queue, which [`Journal::compaction_candidates`] serves first, whatever the
//! game's age and even when the journal does not track it yet: the host appends a snapshot of each
//! such game it still hosts and has not committed. [`Journal::failed_writes`] counts the failures
//! and [`Journal::has_unwritten`] tells whether a flush is still needed before the records
//! appended so far are on disk (the commit gate reads both).
//!
//! # Recovery
//!
//! open() reads the segments in order and stops reading a segment at the first invalid record
//! (impossible length, cut short, wrong CRC or kind); the records before it are kept, and the next
//! segment is read normally. [`Journal::recover`] gives the games without a `committed` record,
//! in order of first appearance, each with its records in order from its latest snapshot.
//!
//! A shard's journal directory must be used by one process at a time.

mod books;
mod crc;
pub mod format;
mod io;
mod metrics;
mod state;
mod worker;

#[cfg(test)]
mod tests;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::{Condvar, Mutex};
use tokio::sync::{oneshot, watch};

pub use crc::{crc32c, crc32c_update};
pub use format::{MAX_PAYLOAD, ParseError, RecordKind};
pub use io::{IoOp, IoProbe};
pub use state::WriteBatchOverride;

use crate::clock::{self, SharedClock};
use crate::config::Config;
use crate::ids::{GameId, ID53_LIMIT};
use crate::log::Logger;
use state::{Shared, State};
use worker::Worker;

/// Default segment size before rotation.
pub const SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
/// Default `JOURNAL_COMPACT_SEGMENTS`: a game is snapshotted once the journal is this many
/// segments past the first one it needs.
pub const COMPACT_SEGMENTS: u64 = 4;
/// Default number of snapshots one batch holds at most (the pace of the compaction).
pub const COMPACT_PER_FLUSH: usize = 8;

/// The settings of one shard's journal.
#[derive(Clone)]
pub struct JournalOptions {
    /// `JOURNAL_DIR`: the journal lives in its `shard-<n>` subdirectory.
    pub dir: PathBuf,
    pub shard: u32,
    /// `JOURNAL_FLUSH_MS`: a batch is written this long after its first record.
    pub flush_ms: u64,
    /// `JOURNAL_FSYNC`: fdatasync every batch.
    pub fsync: bool,
    /// Rotation size.
    pub segment_bytes: u64,
    /// `JOURNAL_COMPACT_SEGMENTS` (at least 1).
    pub compact_segments: u64,
    /// Snapshots per batch at most (at least 1).
    pub compact_per_flush: usize,
    /// Wall time of the `committed` records.
    pub clock: SharedClock,
    pub logger: Logger,
    /// Observes (and may fail) each file-system operation: tests and fault injection.
    pub probe: Option<IoProbe>,
}

impl JournalOptions {
    /// The defaults for the journal of `shard` under `dir`.
    pub fn new(dir: impl Into<PathBuf>, shard: u32) -> JournalOptions {
        JournalOptions {
            dir: dir.into(),
            shard,
            flush_ms: 50,
            fsync: true,
            segment_bytes: SEGMENT_BYTES,
            compact_segments: COMPACT_SEGMENTS,
            compact_per_flush: COMPACT_PER_FLUSH,
            clock: clock::system(),
            logger: Logger::root().child("journal"),
            probe: None,
        }
    }

    /// The settings of the configuration for `shard`.
    pub fn from_config(config: &Config, shard: u32) -> JournalOptions {
        JournalOptions {
            flush_ms: config.journal_flush_ms.max(0) as u64,
            fsync: config.journal_fsync,
            compact_segments: config.journal_compact_segments.max(1) as u64,
            ..JournalOptions::new(&config.journal_dir, shard)
        }
    }
}

impl std::fmt::Debug for JournalOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalOptions")
            .field("dir", &self.dir)
            .field("shard", &self.shard)
            .field("flush_ms", &self.flush_ms)
            .field("fsync", &self.fsync)
            .field("segment_bytes", &self.segment_bytes)
            .field("compact_segments", &self.compact_segments)
            .field("compact_per_flush", &self.compact_per_flush)
            .field("probe", &self.probe.is_some())
            .finish_non_exhaustive()
    }
}

/// An error of the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalError {
    /// The journal is closed.
    Closed,
    /// A payload above [`MAX_PAYLOAD`].
    PayloadTooLarge(usize),
    /// A game id not below 2^53.
    BadGameId(GameId),
    /// A failed file-system operation (for a flush: the batch is not known to be on disk).
    Io { kind: std::io::ErrorKind, message: String },
}

impl JournalError {
    fn io(e: &std::io::Error) -> JournalError {
        JournalError::Io { kind: e.kind(), message: e.to_string() }
    }
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Closed => f.write_str("journal closed"),
            JournalError::PayloadTooLarge(n) => write!(f, "journal: payload too large ({n} bytes)"),
            JournalError::BadGameId(id) => write!(f, "journal: bad game id {id}"),
            JournalError::Io { message, .. } => write!(f, "journal: {message}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// A record found at open().
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub kind: RecordKind,
    /// Epoch milliseconds, as appended.
    pub at: f64,
    /// Opaque bytes, as appended.
    pub payload: Vec<u8>,
}

impl From<format::RecordRef<'_>> for Record {
    fn from(r: format::RecordRef<'_>) -> Record {
        Record { kind: r.kind, at: r.at, payload: r.payload.to_vec() }
    }
}

/// A segment open() did not read to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub segment: u64,
    /// Bytes of valid records before the invalid one.
    pub offset: u64,
    pub error: ParseError,
    pub bytes_ignored: u64,
    /// The newest segment (a torn tail there is the expected trace of a crash).
    pub last: bool,
}

/// What open() read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryInfo {
    pub segments: usize,
    pub records: u64,
    /// Games to recover.
    pub games: usize,
    pub snapshots: u64,
    pub problems: Vec<Problem>,
}

/// Numbers for metrics and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalStats {
    /// Segments on disk.
    pub segments: usize,
    /// Highest segment number used.
    pub seq: u64,
    /// Bytes written to the active segment.
    pub segment_bytes: u64,
    /// Bytes appended and not cut into a batch yet.
    pub pending_bytes: usize,
    pub disk_bytes: u64,
    /// Snapshots written by this process.
    pub snapshots: u64,
    pub compact_queue: usize,
    pub heal_queue: usize,
    pub tracked_games: usize,
    /// A batch is being written.
    pub writing: bool,
    pub recovery: RecoveryInfo,
}

/// The journal of one shard, owned by its host actor. Every method returns at once: the file
/// work happens on the shard's I/O thread. Dropping the handle without [`Journal::close`] lets
/// the thread write what is pending, then exit.
pub struct Journal {
    shared: Arc<Shared>,
    shard: u32,
    dir: PathBuf,
    clock: SharedClock,
    recovered: IndexMap<GameId, Vec<Record>>,
    recovery: RecoveryInfo,
    exited: watch::Receiver<bool>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal").field("shard", &self.shard).field("dir", &self.dir).finish_non_exhaustive()
    }
}

impl Journal {
    /// Opens (creating it if needed) the journal of one shard and scans its segments; the games to
    /// recover are then available from [`Journal::recover`]. The scan runs on the shard's I/O
    /// thread.
    pub async fn open(options: JournalOptions) -> Result<Journal, JournalError> {
        let dir = options.dir.join(format!("shard-{}", options.shard));
        let state = State::new(options.compact_segments.max(1), options.compact_per_flush.max(1));
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            wake: Condvar::new(),
            write_override: Mutex::new(None),
        });
        let (opened_tx, opened_rx) = oneshot::channel();
        let (exited_tx, exited_rx) = watch::channel(false);
        let worker = Worker::new(shared.clone(), dir.clone(), &options);
        std::thread::Builder::new()
            .name(format!("journal-{}", options.shard))
            .spawn(move || worker.run(opened_tx, exited_tx))
            .map_err(|e| JournalError::io(&e))?;
        let opened = opened_rx.await.map_err(|_| JournalError::Closed)??;
        Ok(Journal {
            shared,
            shard: options.shard,
            dir,
            clock: options.clock,
            recovered: opened.recovered,
            recovery: opened.recovery,
            exited: exited_rx,
        })
    }

    /// The shard.
    pub fn shard(&self) -> u32 {
        self.shard
    }

    /// The shard's directory (`JOURNAL_DIR/shard-<n>`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Buffers one record; it is written with the next batch. Errors: `Closed`,
    /// `PayloadTooLarge`, `BadGameId`.
    pub fn append(
        &self,
        kind: RecordKind,
        game: GameId,
        payload: &[u8],
        at: f64,
    ) -> Result<(), JournalError> {
        if game >= ID53_LIMIT {
            return Err(JournalError::BadGameId(game));
        }
        if payload.len() > MAX_PAYLOAD {
            return Err(JournalError::PayloadTooLarge(payload.len()));
        }
        let mut st = self.shared.state.lock();
        if st.closed {
            return Err(JournalError::Closed);
        }
        let first = st.append_record(kind, game, payload, at);
        let idle = !st.writing;
        drop(st);
        if first && idle {
            self.shared.wake.notify_one();
        }
        Ok(())
    }

    /// Marks a game as committed to the database: its records may be forgotten (appends its
    /// `committed` record, at the clock's wall time).
    pub fn committed(&self, game: GameId) -> Result<(), JournalError> {
        self.append(RecordKind::Committed, game, &[], self.clock.wall_ms() as f64)
    }

    /// Writes everything appended so far: the batch is cut now (records appended later go to the
    /// next one), and the future resolves once it is written (and fsynced when enabled) and its
    /// bookkeeping is done. With nothing appended, it waits for the batch being written, if any.
    /// The future need not be polled for the write to happen.
    pub fn flush(&self) -> impl Future<Output = Result<(), JournalError>> + Send + 'static + use<> {
        let rx = {
            let mut st = self.shared.state.lock();
            if st.stopped {
                let done = st.buf.is_empty() && !st.writing;
                drop(st);
                return Flush::Done(if done { Ok(()) } else { Err(JournalError::Closed) });
            }
            let (tx, rx) = oneshot::channel();
            if !st.buf.is_empty() {
                st.waiters.push(tx);
                if !st.writing {
                    let batch = st.take_batch();
                    st.ready = Some(batch);
                    drop(st);
                    self.shared.wake.notify_one();
                }
            } else if st.writing {
                st.inflight.push(tx);
            } else {
                return Flush::Done(Ok(()));
            }
            rx
        };
        Flush::Wait(rx)
    }

    /// Whether records appended so far are not written yet (buffered, or in the batch being
    /// written): a flush is needed before relying on them being on disk.
    pub fn has_unwritten(&self) -> bool {
        let st = self.shared.state.lock();
        !st.buf.is_empty() || st.writing
    }

    /// Batches whose write (or fsync) failed so far.
    pub fn failed_writes(&self) -> u64 {
        self.shared.state.lock().failed_writes
    }

    /// Games whose snapshot the journal wants now: first the games whose records a failed write
    /// lost (heal queue), then those the compaction queued. At most `max`, and at most
    /// `compact_per_flush` snapshots per batch counting those already appended to it; nothing
    /// while closing. Each game is handed out once: the caller appends a
    /// [`RecordKind::Snapshot`] for each game it still hosts and has not committed, from a state
    /// that includes every record it appended for that game; a game it skips is queued again at a
    /// later rotation (a stale one) or not at all (it no longer needs one).
    pub fn compaction_candidates(&self, max: usize) -> Vec<GameId> {
        self.shared.state.lock().compaction_candidates(max)
    }

    /// Games found at open() without a `committed` record, in order of first appearance, with
    /// their records in order; a game with a snapshot starts with its latest one.
    pub fn recover(&self) -> &IndexMap<GameId, Vec<Record>> {
        &self.recovered
    }

    /// Takes the games of [`Journal::recover`], which is empty afterwards.
    pub fn take_recovered(&mut self) -> IndexMap<GameId, Vec<Record>> {
        std::mem::take(&mut self.recovered)
    }

    /// Lets the records of [`Journal::recover`] go once the games are rebuilt.
    pub fn release_recovered(&mut self) {
        self.recovered = IndexMap::new();
    }

    /// Numbers for metrics and tests.
    pub fn stats(&self) -> JournalStats {
        let st = self.shared.state.lock();
        JournalStats {
            segments: st.books.segs.len(),
            seq: st.books.seq,
            segment_bytes: st.books.seg_size,
            pending_bytes: st.buf.len(),
            disk_bytes: st.books.disk_bytes,
            snapshots: st.snapshots,
            compact_queue: st.compact_queue.len(),
            heal_queue: st.heal_queue.len(),
            tracked_games: st.books.games.len(),
            writing: st.writing,
            recovery: self.recovery.clone(),
        }
    }

    /// Replaces the write of the next batches (fault injection for the commit-gate tests): the
    /// hook is called with each batch's bytes before anything is written; an error fails the
    /// batch like a failed write (nothing written, no rotation), `Ok` lets it be written.
    /// `None` removes the hook.
    pub fn set_write_batch_override(&self, hook: Option<WriteBatchOverride>) {
        *self.shared.write_override.lock() = hook;
    }

    /// Flushes, then stops the I/O thread and closes the segment. Fails if the last flush failed.
    /// Appends fail with `Closed` once it resolves; a second call only waits for the end.
    pub fn close(&self) -> impl Future<Output = Result<(), JournalError>> + Send + 'static + use<> {
        let first = {
            let mut st = self.shared.state.lock();
            st.closing = true;
            !st.closed
        };
        let flush = first.then(|| self.flush());
        let shared = self.shared.clone();
        let mut exited = self.exited.clone();
        async move {
            let result = match flush {
                Some(flush) => flush.await,
                None => Ok(()),
            };
            shared.state.lock().closed = true;
            shared.wake.notify_one();
            let _ = exited.wait_for(|done| *done).await;
            result
        }
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        {
            let mut st = self.shared.state.lock();
            st.closing = true;
            st.closed = true;
        }
        self.shared.wake.notify_one();
    }
}

/// The future of [`Journal::flush`].
enum Flush {
    Done(Result<(), JournalError>),
    Wait(oneshot::Receiver<Result<(), JournalError>>),
}

impl Future for Flush {
    type Output = Result<(), JournalError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.get_mut() {
            Flush::Done(result) => std::task::Poll::Ready(std::mem::replace(result, Ok(()))),
            Flush::Wait(rx) => {
                std::pin::Pin::new(rx).poll(cx).map(|r| r.unwrap_or(Err(JournalError::Closed)))
            }
        }
    }
}
