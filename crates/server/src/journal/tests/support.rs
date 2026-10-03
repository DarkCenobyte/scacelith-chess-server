//! Helpers of the journal tests: temporary directories, segment files, an I/O event recorder,
//! and a model host whose games can be checked against what a recovery rebuilds.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::ids::GameId;
use crate::journal::format::{encode_record, segment_name, segment_seq};
use crate::journal::{IoOp, IoProbe, Journal, JournalOptions, Record, RecordKind};

/// A directory removed when dropped.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "scacelith-journal-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Options for tests: shard 0, no timer flush in practice (1 s), fsync off.
pub fn opts(dir: &Path) -> JournalOptions {
    JournalOptions { flush_ms: 1000, fsync: false, ..JournalOptions::new(dir, 0) }
}

/// Opens a journal, panicking on error.
pub async fn open(options: JournalOptions) -> Journal {
    Journal::open(options).await.unwrap()
}

/// The segment file names of a shard, sorted.
pub fn segments(dir: &Path, shard: u32) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(dir.join(format!("shard-{shard}"))) {
        Ok(entries) => entries
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| segment_seq(n).is_some())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// `segment_name(seq)` for each of `seqs`.
pub fn names(seqs: &[u64]) -> Vec<String> {
    seqs.iter().map(|s| segment_name(*s)).collect()
}

/// The path of a segment file.
pub fn seg_path(dir: &Path, name: &str, shard: u32) -> PathBuf {
    dir.join(format!("shard-{shard}")).join(name)
}

/// Sum of the sizes of a shard's segments.
pub fn bytes_on_disk(dir: &Path, shard: u32) -> u64 {
    segments(dir, shard).iter().map(|n| fs::metadata(seg_path(dir, n, shard)).unwrap().len()).sum()
}

/// Writes a segment file directly (as any version of the server writes it).
pub fn write_segment(dir: &Path, shard: u32, seq: u64, records: &[(RecordKind, GameId, f64, &[u8])]) {
    let mut buf = Vec::new();
    for (kind, game, at, payload) in records {
        encode_record(&mut buf, *kind, *game, *at, payload);
    }
    fs::create_dir_all(dir.join(format!("shard-{shard}"))).unwrap();
    fs::write(seg_path(dir, &segment_name(seq), shard), buf).unwrap();
}

/// Copies a shard's directory to `dst` (a crash at this moment, reopened elsewhere).
pub fn copy_shard(src: &Path, dst: &Path, shard: u32) {
    let to = dst.join(format!("shard-{shard}"));
    fs::create_dir_all(&to).unwrap();
    for name in segments(src, shard) {
        fs::copy(seg_path(src, &name, shard), to.join(&name)).unwrap();
    }
}

/// The games a crash now would recover: a copy of the directory, reopened.
pub async fn recovered_keys(dir: &Path, options: &JournalOptions) -> Vec<GameId> {
    let copy = TempDir::new("copy");
    copy_shard(dir, copy.path(), options.shard);
    let j = open(JournalOptions { dir: copy.path().to_path_buf(), probe: None, ..options.clone() }).await;
    let mut keys: Vec<GameId> = j.recover().keys().copied().collect();
    j.close().await.unwrap();
    keys.sort_unstable();
    keys
}

/// `(kind, payload as text)` of records.
pub fn texts(records: &[Record]) -> Vec<(RecordKind, String)> {
    records.iter().map(|r| (r.kind, String::from_utf8_lossy(&r.payload).into_owned())).collect()
}

/// Sorted keys of a recovery.
pub fn keys(j: &Journal) -> Vec<GameId> {
    let mut keys: Vec<GameId> = j.recover().keys().copied().collect();
    keys.sort_unstable();
    keys
}

/// An I/O event the recorder keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Datasync,
    SyncDir,
    Unlink(String),
}

/// Records the fdatasyncs, directory fsyncs and deletions of a journal while `on`.
#[derive(Clone, Default)]
pub struct Recorder {
    pub events: Arc<Mutex<Vec<Event>>>,
    pub on: Arc<Mutex<bool>>,
}

impl Recorder {
    pub fn new() -> Recorder {
        let r = Recorder::default();
        *r.on.lock() = true;
        r
    }

    pub fn probe(&self) -> IoProbe {
        let r = self.clone();
        Arc::new(move |op| {
            if *r.on.lock() {
                let event = match op {
                    IoOp::Datasync(_) => Some(Event::Datasync),
                    IoOp::SyncDir => Some(Event::SyncDir),
                    IoOp::Unlink(seq) => Some(Event::Unlink(segment_name(seq))),
                    IoOp::Create(_) | IoOp::Write { .. } => None,
                };
                r.events.lock().extend(event);
            }
            Ok(())
        })
    }

    pub fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock())
    }
}

/// `Event::Unlink(segment_name(seq))`.
pub fn unlink(seq: u64) -> Event {
    Event::Unlink(segment_name(seq))
}

/// An I/O error of a kind, with a message.
pub fn io_error(kind: io::ErrorKind, message: &str) -> io::Error {
    io::Error::new(kind, message.to_string())
}

/// A write-batch override that fails once with `message`, then lets the batches through.
pub fn fail_once(kind: io::ErrorKind, message: &'static str) -> crate::journal::WriteBatchOverride {
    let mut failed = false;
    Box::new(move |_| {
        if failed {
            return Ok(());
        }
        failed = true;
        Err(io_error(kind, message))
    })
}

/// One record of a model game: kind, at, payload.
pub type ModelRecord = (RecordKind, f64, Vec<u8>);

/// The snapshot payload of a model game: every record so far, `u32 count` then per record
/// `u8 kind | u32 len | f64 at | payload`.
pub fn encode_snapshot(records: &[ModelRecord]) -> Vec<u8> {
    let mut out = (records.len() as u32).to_le_bytes().to_vec();
    for (kind, at, payload) in records {
        out.push(kind.as_u8());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(payload);
    }
    out
}

fn decode_snapshot(p: &[u8]) -> Vec<ModelRecord> {
    let n = u32::from_le_bytes(p[..4].try_into().unwrap()) as usize;
    let mut o = 4;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = RecordKind::from_u8(p[o]).unwrap();
        let len = u32::from_le_bytes(p[o + 1..o + 5].try_into().unwrap()) as usize;
        let at = f64::from_le_bytes(p[o + 5..o + 13].try_into().unwrap());
        out.push((kind, at, p[o + 13..o + 13 + len].to_vec()));
        o += 13 + len;
    }
    assert_eq!(o, p.len(), "snapshot fully read");
    out
}

/// What a game module rebuilds from recovered records: a snapshot stands for every record it
/// holds, the latest one winning.
pub fn replay(records: &[Record]) -> Vec<ModelRecord> {
    let mut out = Vec::new();
    for r in records {
        if r.kind == RecordKind::Snapshot {
            out = decode_snapshot(&r.payload);
        } else {
            out.push((r.kind, r.at, r.payload.clone()));
        }
    }
    out
}

/// A model of the host: the games not committed and the records it appended for each.
#[derive(Debug, Default, Clone)]
pub struct Model {
    pub games: BTreeMap<GameId, Vec<ModelRecord>>,
    pub ended: Vec<GameId>,
    next_id: GameId,
    pub t: f64,
    pub snapshots: u64,
    /// Every record appended, in order, `committed` ones included (as an older server would have
    /// journaled them, without snapshots).
    pub tape: Vec<(RecordKind, GameId, f64, Vec<u8>)>,
}

impl Model {
    pub fn new(first_id: GameId) -> Model {
        Model { next_id: first_id, t: 1_800_000_000_000.0, ..Model::default() }
    }

    /// Rebuilds the model from a recovery (as a host does at start).
    pub fn recovered(j: &Journal, next_id: GameId) -> Model {
        let mut m = Model::new(next_id);
        for (id, records) in j.recover() {
            m.games.insert(*id, replay(records));
            if replay(records).iter().any(|r| r.0 == RecordKind::Ended) {
                m.ended.push(*id);
            }
        }
        m
    }

    fn append(&mut self, j: &Journal, id: GameId, kind: RecordKind, payload: Vec<u8>) {
        self.t += 1.0;
        j.append(kind, id, &payload, self.t).unwrap();
        self.tape.push((kind, id, self.t, payload.clone()));
        self.games.get_mut(&id).unwrap().push((kind, self.t, payload));
    }

    /// Creates a game.
    pub fn create(&mut self, j: &Journal) -> GameId {
        let id = self.next_id;
        self.next_id += 1;
        self.games.insert(id, Vec::new());
        self.append(j, id, RecordKind::Created, format!("{{\"game\":{id}}}").into_bytes());
        id
    }

    /// One move of a game (a payload of `pad` bytes besides its number).
    pub fn play(&mut self, j: &Journal, id: GameId, pad: usize) {
        let n = self.games[&id].len() as u32;
        let mut payload = n.to_le_bytes().to_vec();
        payload.resize(4 + pad, (n & 0xff) as u8);
        self.append(j, id, RecordKind::Move, payload);
    }

    /// An event of a game.
    pub fn event(&mut self, j: &Journal, id: GameId) {
        self.append(j, id, RecordKind::Event, b"event".to_vec());
    }

    /// Ends a game (it waits for its commit).
    pub fn end(&mut self, j: &Journal, id: GameId) {
        self.append(j, id, RecordKind::Ended, Vec::new());
        self.ended.push(id);
    }

    /// Commits an ended game: the host forgets it.
    pub fn commit(&mut self, j: &Journal, id: GameId) {
        j.committed(id).unwrap();
        self.t += 1.0;
        self.tape.push((RecordKind::Committed, id, self.t, Vec::new()));
        self.games.remove(&id);
        self.ended.retain(|g| *g != id);
    }

    /// Appends the snapshots the journal asks for (at most `max`); returns how many.
    pub fn compact(&mut self, j: &Journal, max: usize) -> usize {
        let mut n = 0;
        for id in j.compaction_candidates(max) {
            let Some(records) = self.games.get(&id) else { continue };
            let payload = encode_snapshot(records);
            self.t += 1.0;
            j.append(RecordKind::Snapshot, id, &payload, self.t).unwrap();
            n += 1;
        }
        self.snapshots += n as u64;
        n
    }

    /// Checks that a recovery from `dir` rebuilds exactly the games of `expected`.
    pub async fn verify(
        dir: &Path,
        options: &JournalOptions,
        expected: &BTreeMap<GameId, Vec<ModelRecord>>,
        label: &str,
    ) {
        let j = open(JournalOptions { dir: dir.to_path_buf(), probe: None, ..options.clone() }).await;
        let got: BTreeMap<GameId, Vec<ModelRecord>> =
            j.recover().iter().map(|(id, r)| (*id, replay(r))).collect();
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            expected.keys().collect::<Vec<_>>(),
            "{label}: the games not committed, and only them"
        );
        for (id, want) in expected {
            assert!(got[id] == *want, "{label}: game {id} rebuilt exactly");
        }
        j.close().await.unwrap();
    }
}
