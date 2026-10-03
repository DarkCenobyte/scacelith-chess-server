//! The I/O thread of one journal: the scan at open(), then the batches, one at a time (write,
//! fdatasync, bookkeeping, deletions), then the close.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::{IndexMap, IndexSet};
use parking_lot::MutexGuard;
use tokio::sync::{oneshot, watch};

use super::books::Segment;
use super::format::{ParseError, RecordKind, parse_segment, segment_name, segment_seq};
use super::io::Io;
use super::metrics::{BYTES, DISK_BYTES, ERRORS, FLUSH_MS, SEGMENTS, SEGMENTS_DELETED, SNAPSHOTS};
use super::state::{Batch, Shared, State};
use super::{JournalError, JournalOptions, Problem, Record, RecoveryInfo};
use crate::clock::SharedClock;
use crate::ids::GameId;
use crate::log::{Level, Logger};
use crate::metrics::Gauge;
use crate::{log_error, log_warn};

/// Largest buffer kept for reuse once its batch is written.
const MAX_SPARE: usize = 4 * 1024 * 1024;

/// What open() found.
pub(super) struct Opened {
    pub recovered: IndexMap<GameId, Vec<Record>>,
    pub recovery: RecoveryInfo,
}

/// The I/O thread's side of a journal.
pub(super) struct Worker {
    shared: Arc<Shared>,
    dir: PathBuf,
    shard: u32,
    fsync: bool,
    segment_bytes: u64,
    flush: Duration,
    clock: SharedClock,
    log: Logger,
    io: Io,
    /// The active segment (opened at the first write: existing segments are never appended to).
    file: Option<File>,
    segments_gauge: Gauge,
    disk_bytes_gauge: Gauge,
}

impl Worker {
    pub fn new(shared: Arc<Shared>, dir: PathBuf, options: &JournalOptions) -> Worker {
        let label = options.shard.to_string();
        Worker {
            shared,
            dir,
            shard: options.shard,
            fsync: options.fsync,
            segment_bytes: options.segment_bytes.max(1),
            flush: Duration::from_millis(options.flush_ms),
            clock: options.clock.clone(),
            log: options.logger.clone(),
            io: Io::new(options.probe.clone()),
            file: None,
            segments_gauge: SEGMENTS.with(&[&label]),
            disk_bytes_gauge: DISK_BYTES.with(&[&label]),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock()
    }

    /// The thread's body: open, serve the batches until the close, exit.
    pub fn run(mut self, opened: oneshot::Sender<Result<Opened, JournalError>>, exited: watch::Sender<bool>) {
        let outcome = catch_unwind(AssertUnwindSafe(|| match self.open() {
            Ok(o) => {
                // A dropped receiver means open() was abandoned: nobody will write.
                if opened.send(Ok(o)).is_ok() {
                    self.serve();
                }
            }
            Err(e) => {
                let _ = opened.send(Err(JournalError::io(&e)));
            }
        }));
        if outcome.is_err() {
            log_error!(self.log, "journal I/O thread panicked", { "shard": self.shard });
        }
        self.file = None;
        let waiters = {
            let mut st = self.lock();
            st.stopped = true;
            st.closing = true;
            st.closed = true;
            st.writing = false;
            st.ready = None;
            let mut waiters = std::mem::take(&mut st.waiters);
            waiters.append(&mut st.inflight);
            waiters
        };
        for w in waiters {
            let _ = w.send(Err(JournalError::Closed));
        }
        let _ = exited.send(true);
    }

    /// Waits for batches and writes them: a batch a flush cut, or the buffer once it is due
    /// (flush_ms after its first record, at once when a flush waits for it or while closing).
    fn serve(&mut self) {
        loop {
            let batch = {
                let mut st = self.lock();
                loop {
                    if let Some(batch) = st.ready.take() {
                        break Some(batch);
                    }
                    if !st.writing && !st.buf.is_empty() {
                        let deadline = st.pending_since.map(|t| t + self.flush);
                        if st.closing
                            || !st.waiters.is_empty()
                            || deadline.is_none_or(|d| Instant::now() >= d)
                        {
                            break Some(st.take_batch());
                        }
                        if let Some(d) = deadline {
                            self.shared.wake.wait_until(&mut st, d);
                        }
                        continue;
                    }
                    if st.closed && !st.writing {
                        break None;
                    }
                    self.shared.wake.wait(&mut st);
                }
            };
            match batch {
                Some(batch) => self.write(batch),
                None => return,
            }
        }
    }

    fn write(&mut self, batch: Batch) {
        let t0 = Instant::now();
        match self.write_batch(&batch.buf, !batch.snaps.is_empty()) {
            Ok(seq) => self.written(seq, batch, t0),
            Err(e) => self.failed(batch, e),
        }
    }

    /// Writes one batch (and fdatasyncs it when fsync is on). A batch holding a snapshot is made
    /// durable even with fsync off, its segment's directory entry included, because the snapshot
    /// releases older segments.
    fn write_batch(&mut self, buf: &[u8], has_snapshot: bool) -> io::Result<u64> {
        if let Some(hook) = self.shared.write_override.lock().as_mut() {
            hook(buf)?;
        }
        let rotate = {
            let st = self.lock();
            self.file.is_none() || st.books.force_rotate || st.books.seg_size >= self.segment_bytes
        };
        if rotate {
            self.rotate()?;
        }
        let seq = self.lock().books.seq;
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("journal segment not open"));
        };
        let mut written = 0;
        let result = self.io.write(file, seq, buf, &mut written);
        {
            let mut st = self.shared.state.lock();
            if let Some(seg) = st.books.segs.get_mut(&seq) {
                seg.bytes += written;
            }
            st.books.disk_bytes += written;
            if result.is_ok() {
                st.books.seg_size += buf.len() as u64;
            }
        }
        result?;
        if self.fsync || has_snapshot {
            self.io.datasync(file, seq)?;
        }
        if has_snapshot && seq > self.lock().books.synced_seq {
            self.sync_dir(None);
        }
        Ok(seq)
    }

    /// Starts the next segment. If the new file cannot be created, the number is not used and
    /// the next batch tries again (the previous segment then stops being the active one all the
    /// same).
    fn rotate(&mut self) -> io::Result<()> {
        let (prev, seq) = {
            let mut st = self.lock();
            st.books.force_rotate = false;
            let cur = st.books.seq;
            (st.books.segs.contains_key(&cur).then_some(cur), cur + 1)
        };
        self.file = None;
        let path = self.dir.join(segment_name(seq));
        self.file = Some(self.io.create(&path, seq)?);
        if self.fsync {
            self.sync_dir(Some(seq));
        }
        {
            let mut st = self.lock();
            st.books.seq = seq;
            st.books.seg_size = 0;
            st.books
                .segs
                .insert(seq, Segment { path, games: IndexSet::new(), pins: 0, active: true, bytes: 0 });
            if let Some(p) = prev
                && let Some(seg) = st.books.segs.get_mut(&p)
            {
                seg.active = false;
            }
        }
        if let Some(p) = prev {
            self.gc(vec![p]);
        }
        let mut st = self.lock();
        st.enqueue_stale();
        self.update_gauges(&st);
        Ok(())
    }

    /// The batch is written (and fsynced): bookkeeping, deletions, then its flushes are answered.
    /// The next batch starts after the deletions.
    fn written(&self, seq: u64, mut batch: Batch, t0: Instant) {
        FLUSH_MS.observe(t0.elapsed().as_secs_f64() * 1000.0);
        BYTES.add(batch.buf.len() as u64);
        let touched = {
            let mut st = self.lock();
            for game in &batch.games {
                st.books.track(*game, seq);
            }
            let mut touched = IndexSet::new();
            // The batch is durable: its snapshots release their games' older segments.
            for game in &batch.snaps {
                st.snaps_pending.remove(game);
                touched.extend(st.books.mark_snapshot(*game, seq));
            }
            if !batch.snaps.is_empty() {
                st.snapshots += batch.snaps.len() as u64;
                SNAPSHOTS.add(batch.snaps.len() as u64);
            }
            for game in &batch.commits {
                touched.extend(st.books.mark_committed(*game, seq));
            }
            touched
        };
        if !touched.is_empty() {
            self.gc(touched.into_iter().collect());
        }
        let waiters = {
            let mut st = self.lock();
            self.update_gauges(&st);
            if batch.buf.capacity() <= MAX_SPARE {
                batch.buf.clear();
                st.spare = Some(batch.buf);
            }
            st.settle()
        };
        for w in waiters {
            let _ = w.send(Ok(()));
        }
    }

    /// The batch is not known to be on disk: the next batch goes to a new segment (later records
    /// must not follow a possibly torn write), its `committed` records are appended again (the
    /// host has forgotten these games, and without the record their segments would stay until
    /// the next restart), and its other games go to the heal queue (they lost records: a new
    /// snapshot supersedes them).
    fn failed(&self, batch: Batch, err: io::Error) {
        ERRORS.inc();
        log_error!(self.log, "journal write failed", {
            "shard": self.shard, "err": err.to_string(), "bytes": batch.buf.len(),
        });
        let error = JournalError::io(&err);
        let waiters = {
            let mut st = self.lock();
            st.failed_writes += 1;
            st.books.force_rotate = true;
            for game in &batch.snaps {
                st.snaps_pending.remove(game);
            }
            if !st.closing && !st.closed {
                let now = self.clock.wall_ms() as f64;
                for game in &batch.commits {
                    st.append_record(RecordKind::Committed, *game, &[], now);
                }
            }
            let done: HashSet<GameId> = batch.commits.iter().copied().collect();
            for game in &batch.games {
                let committed = st.books.games.get(game).is_some_and(|g| g.committed);
                if !done.contains(game) && !committed {
                    st.heal_queue.insert(*game);
                }
            }
            st.settle()
        };
        for w in waiters {
            let _ = w.send(Err(error.clone()));
        }
    }

    /// Deletes the candidate segments that no game needs any more, then the segments this makes
    /// deletable in turn (one holding a game's `committed` record, once the game's other segments
    /// are gone). With fsync on, the segments holding a `committed` record are deleted after the
    /// others, and only once every earlier deletion is durable.
    fn gc(&self, candidates: Vec<u64>) {
        let mut queue = candidates;
        while !queue.is_empty() {
            let mut next = Vec::new();
            let mut held = Vec::new();
            for seq in queue {
                let path = {
                    let st = self.lock();
                    if !st.books.deletable(seq) {
                        continue;
                    }
                    if self.fsync && st.books.holds_commit(seq) {
                        held.push(seq);
                        continue;
                    }
                    st.books.segs[&seq].path.clone()
                };
                self.remove(seq, &path, &mut next);
            }
            if !held.is_empty() {
                let unsynced = {
                    let st = self.lock();
                    st.books.unlinks > st.books.synced_unlinks
                };
                if unsynced {
                    self.sync_dir(None);
                }
                // All of them deletable at the same moment: none waits for another's deletion.
                let ready: Vec<(u64, PathBuf)> = {
                    let st = self.lock();
                    held.into_iter()
                        .filter(|seq| st.books.deletable(*seq))
                        .map(|seq| (seq, st.books.segs[&seq].path.clone()))
                        .collect()
                };
                for (seq, path) in ready {
                    self.remove(seq, &path, &mut next);
                }
            }
            queue = next;
        }
    }

    /// Deletes one segment and forgets it; `next` receives the segments holding a `committed`
    /// record that may be deletable now.
    fn remove(&self, seq: u64, path: &Path, next: &mut Vec<u64>) {
        match self.io.unlink(path, seq) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                log_warn!(self.log, "journal segment not deleted", {
                    "shard": self.shard, "file": path.display().to_string(), "err": e.to_string(),
                });
                return;
            }
        }
        SEGMENTS_DELETED.inc();
        let mut st = self.lock();
        st.books.unlinks += 1;
        st.books.forget(seq, next);
    }

    /// Directory fsync: the segments created and deleted so far are durable (`created`: the
    /// segment a rotation is creating, not counted in `seq` yet).
    fn sync_dir(&self, created: Option<u64>) {
        let (unlinks, seq) = {
            let st = self.lock();
            (st.books.unlinks, created.map_or(st.books.seq, |c| c.max(st.books.seq)))
        };
        self.io.sync_dir(&self.dir);
        let mut st = self.lock();
        st.books.synced_unlinks = st.books.synced_unlinks.max(unlinks);
        st.books.synced_seq = st.books.synced_seq.max(seq);
    }

    fn update_gauges(&self, st: &State) {
        self.segments_gauge.set(st.books.segs.len() as f64);
        self.disk_bytes_gauge.set(st.books.disk_bytes as f64);
    }

    /// Scans the segments in order: the games to recover, the bookkeeping, then the deletions it
    /// allows, once what it read is durable.
    fn open(&mut self) -> io::Result<Opened> {
        fs::create_dir_all(&self.dir)?;
        let mut seqs = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            if let Some(seq) = entry?.file_name().to_str().and_then(segment_seq) {
                seqs.push(seq);
            }
        }
        seqs.sort_unstable();
        let last = seqs.last().copied();
        let mut per_game: IndexMap<GameId, Vec<Record>> = IndexMap::new();
        let mut committed: HashSet<GameId> = HashSet::new();
        let mut snapshot_segs = Vec::new();
        let mut info = RecoveryInfo { segments: seqs.len(), ..RecoveryInfo::default() };
        for &seq in &seqs {
            let path = self.dir.join(segment_name(seq));
            let buf = self.io.read_segment(&path, seq, self.fsync)?;
            let mut has_snapshot = false;
            let outcome = {
                let mut st = self.lock();
                let books = &mut st.books;
                books.seq = books.seq.max(seq);
                books.disk_bytes += buf.len() as u64;
                let bytes = buf.len() as u64;
                books
                    .segs
                    .insert(seq, Segment { path, games: IndexSet::new(), pins: 0, active: false, bytes });
                parse_segment(&buf, |r| {
                    info.records += 1;
                    books.track(r.game, seq);
                    match r.kind {
                        RecordKind::Snapshot => {
                            info.snapshots += 1;
                            has_snapshot = true;
                            books.mark_snapshot(r.game, seq);
                            // It supersedes the game's earlier records (the game keeps its place).
                            if !committed.contains(&r.game) {
                                per_game.insert(r.game, vec![Record::from(r)]);
                            }
                        }
                        RecordKind::Committed => {
                            committed.insert(r.game);
                            books.mark_committed(r.game, seq);
                            // Never recovered: its records need not stay in memory.
                            if let Some(list) = per_game.get_mut(&r.game) {
                                *list = Vec::new();
                            }
                        }
                        _ => {
                            if !committed.contains(&r.game) {
                                per_game.entry(r.game).or_default().push(Record::from(r));
                            }
                        }
                    }
                })
            };
            if has_snapshot {
                snapshot_segs.push(seq);
            }
            if let Some(error) = outcome.error {
                let problem = Problem {
                    segment: seq,
                    offset: outcome.end as u64,
                    error,
                    bytes_ignored: (buf.len() - outcome.end) as u64,
                    last: Some(seq) == last,
                };
                // A torn end of the newest segment is the expected trace of a crash.
                let level = if problem.last && error == ParseError::Torn { Level::Info } else { Level::Warn };
                let fields = serde_json::json!({
                    "shard": self.shard, "segment": seq, "offset": problem.offset, "error": error.as_str(),
                    "bytesIgnored": problem.bytes_ignored, "last": problem.last,
                });
                self.log.emit(level, "journal segment tail ignored", Some(fields));
                info.problems.push(problem);
            }
        }
        let recovered: IndexMap<GameId, Vec<Record>> =
            per_game.into_iter().filter(|(game, _)| !committed.contains(game)).collect();
        info.games = recovered.len();
        // What was read is durable (read_segment), and so are the directory's entries (a previous
        // process's deletions included) before anything is deleted on their strength. With fsync
        // off, the segments holding a snapshot are made durable all the same, since a snapshot
        // read here lets its game's older segments be deleted.
        if self.fsync && !seqs.is_empty() {
            self.sync_dir(None);
        } else if !snapshot_segs.is_empty() {
            for seq in snapshot_segs {
                self.io.datasync_path(&self.dir.join(segment_name(seq)), seq);
            }
            self.sync_dir(None);
        }
        self.gc(seqs);
        let mut st = self.lock();
        st.enqueue_stale();
        self.update_gauges(&st);
        Ok(Opened { recovered, recovery: info })
    }
}
