//! Crash simulations: a child process (this test binary, running one ignored test) journals
//! continuously and is killed with SIGKILL. Every record acknowledged by a resolved flush must
//! be recovered, and what is recovered must be an exact prefix of what was appended (no gap, no
//! corrupt record), across segment rotations; with compaction, each long game must come back from
//! its latest durable snapshot followed by every later record, no committed game may come back,
//! and the journal must stay small.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::support::*;
use crate::journal::{JournalOptions, RecordKind};

use RecordKind::{Created, Ended, Move, Snapshot};

const DIR_VAR: &str = "SCACELITH_JOURNAL_CRASH_DIR";
const FSYNC_VAR: &str = "SCACELITH_JOURNAL_CRASH_FSYNC";
const PLAIN_SHARD: u32 = 3;
const COMPACT_SHARD: u32 = 4;
const SHORT0: u64 = 100_000;
const SNAPSHOT_MARK: u32 = 0x00ab_cdef;

fn plain_options(dir: &Path, fsync: bool) -> JournalOptions {
    JournalOptions { flush_ms: 2, fsync, segment_bytes: 64 * 1024, ..JournalOptions::new(dir, PLAIN_SHARD) }
}

fn compacting_options(dir: &Path, fsync: bool) -> JournalOptions {
    JournalOptions {
        flush_ms: 2,
        fsync,
        segment_bytes: 16 * 1024,
        compact_segments: 2,
        compact_per_flush: 4,
        ..JournalOptions::new(dir, COMPACT_SHARD)
    }
}

/// The child's settings, when this process is a crash test's child.
fn child_env() -> Option<(String, bool)> {
    let dir = std::env::var(DIR_VAR).ok()?;
    Some((dir, std::env::var(FSYNC_VAR).is_ok_and(|v| v == "1")))
}

/// The payload of record `i`: its number, then its low byte repeated.
fn payload(i: u32) -> [u8; 24] {
    let mut p = [(i & 0xff) as u8; 24];
    p[..4].copy_from_slice(&i.to_le_bytes());
    p
}

/// Checks a recovered record of the crash runs; returns its number.
fn check_record(game: u64, base: u64, r: &crate::journal::Record) -> u32 {
    assert_eq!(r.kind, Move);
    let i = u32::from_le_bytes(r.payload[..4].try_into().unwrap());
    assert_eq!(game, base + u64::from(i % 5), "record {i} in its game");
    assert!(r.payload[4..].iter().all(|b| *b == (i & 0xff) as u8), "payload of record {i} intact");
    assert_eq!(r.at, f64::from(i));
    i
}

#[tokio::test]
#[ignore = "child process of the crash tests"]
async fn crash_child_plain() {
    let Some((dir, fsync)) = child_env() else { return };
    let j = open(plain_options(Path::new(&dir), fsync)).await;
    let t0 = Instant::now();
    let mut i = 0u32;
    while t0.elapsed() < Duration::from_secs(120) {
        for _ in 0..25 {
            j.append(Move, 5000 + u64::from(i % 5), &payload(i), f64::from(i)).unwrap();
            i += 1;
        }
        let (upto, segs) = (i - 1, j.stats().segments);
        let flush = j.flush();
        tokio::spawn(async move {
            if flush.await.is_ok() {
                println!("ack {upto} {segs}");
            }
        });
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
#[ignore = "child process of the crash tests"]
async fn crash_child_compacting() {
    let Some((dir, fsync)) = child_env() else { return };
    let j = open(compacting_options(Path::new(&dir), fsync)).await;
    let t0 = Instant::now();
    let mut last: HashMap<u64, u32> = HashMap::new();
    let (mut i, mut tick) = (0u32, 0u64);
    while t0.elapsed() < Duration::from_secs(120) {
        tick += 1;
        for _ in 0..25 {
            let game = 7000 + u64::from(i % 5);
            j.append(Move, game, &payload(i), f64::from(i)).unwrap();
            last.insert(game, i);
            i += 1;
        }
        j.append(Created, SHORT0 + tick, b"short", -1.0).unwrap();
        let mut committed = 0;
        if tick > 2 {
            j.append(Ended, SHORT0 + tick - 2, b"", -1.0).unwrap();
            j.committed(SHORT0 + tick - 2).unwrap();
            committed = SHORT0 + tick - 2;
        }
        for game in j.compaction_candidates(usize::MAX) {
            // A short game is committed soon: not snapshotted.
            let Some(l) = last.get(&game) else { continue };
            let mut s = [0u8; 8];
            s[..4].copy_from_slice(&l.to_le_bytes());
            s[4..].copy_from_slice(&SNAPSHOT_MARK.to_le_bytes());
            j.append(Snapshot, game, &s, -2.0).unwrap();
        }
        let (upto, stats) = (i - 1, j.stats());
        let (segs, snapshots) = (stats.segments, stats.snapshots);
        let flush = j.flush();
        tokio::spawn(async move {
            if flush.await.is_ok() {
                println!("ack {upto} {committed} {segs} {snapshots}");
            }
        });
        tokio::task::yield_now().await;
    }
}

/// Runs a child test and kills it with SIGKILL once `enough` says so about the fields of an
/// `ack` line.
fn kill_when(test: &str, dir: &Path, fsync: bool, mut enough: impl FnMut(&[u64]) -> bool) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(DIR_VAR, dir)
        .env(FSYNC_VAR, if fsync { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                let Some(rest) = line.strip_prefix("ack ") else { continue };
                let fields: Vec<u64> = rest.split(' ').map(|f| f.parse().unwrap()).collect();
                if enough(&fields) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                panic!("the child produced too few acks in time");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the child exited: {:?}", child.wait()),
        }
    }
    child.kill().unwrap(); // SIGKILL
    child.wait().unwrap();
    reader.join().unwrap();
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// One plain crash run; returns the segments left on disk.
fn crash_run(fsync: bool) -> usize {
    let dir = TempDir::new("crash");
    let (mut last_ack, mut segs) = (0, 0);
    // Killed once it has rotated a few times (a slow I/O thread makes large batches).
    kill_when("journal::tests::crash::crash_child_plain", dir.path(), fsync, |f| {
        last_ack = last_ack.max(f[0]);
        segs = segs.max(f[1]);
        last_ack >= 6000 && segs >= 3
    });
    runtime().block_on(async {
        let j =
            open(JournalOptions { flush_ms: 1000, fsync: false, ..plain_options(dir.path(), false) }).await;
        let mut seen = Vec::new();
        for (game, records) in j.recover() {
            let mut prev = None;
            for r in records {
                let i = check_record(*game, 5000, r);
                assert!(prev.is_none_or(|p| i > p), "records of a game in order");
                prev = Some(i);
                seen.push(i);
            }
        }
        seen.sort_unstable();
        assert!(
            seen.len() as u64 > last_ack,
            "every acknowledged record recovered ({} > {last_ack})",
            seen.len()
        );
        for (k, i) in seen.iter().enumerate() {
            assert_eq!(*i as usize, k, "the recovered records form an exact prefix");
        }
        let segs = segments(dir.path(), PLAIN_SHARD).len();
        j.close().await.unwrap();
        segs
    })
}

#[test]
fn crash_simulation_sigkill_mid_writes_fsync_off() {
    for _ in 0..3 {
        assert!(crash_run(false) > 1, "the run rotated segments");
    }
}

#[test]
fn crash_simulation_sigkill_mid_writes_fsync_on() {
    crash_run(true);
}

/// One compacting crash run.
fn compacting_crash_run(fsync: bool) {
    let dir = TempDir::new("crash-compacting");
    let (mut last_ack, mut last_commit, mut max_segs, mut written_snapshots) = (0, 0, 0, 0);
    // Killed once it has compacted a few times (a slow I/O thread makes large batches, and so
    // fewer rotations).
    kill_when("journal::tests::crash::crash_child_compacting", dir.path(), fsync, |f| {
        last_ack = last_ack.max(f[0]);
        last_commit = last_commit.max(f[1]);
        max_segs = max_segs.max(f[2]);
        written_snapshots = written_snapshots.max(f[3]);
        last_ack >= 15_000 && written_snapshots >= 10
    });
    runtime().block_on(async {
        let j =
            open(JournalOptions { flush_ms: 1000, fsync: false, ..compacting_options(dir.path(), false) })
                .await;
        let rec = j.recover();
        let mut snapshots = 0;
        for k in 0..5u64 {
            let game = 7000 + k;
            let list = &rec[&game];
            // The number its next record must have.
            let mut next = k;
            let mut from = 0;
            if list[0].kind == Snapshot {
                let p = &list[0].payload;
                assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), SNAPSHOT_MARK, "snapshot intact");
                next = u64::from(u32::from_le_bytes(p[..4].try_into().unwrap())) + 5;
                from = 1;
                snapshots += 1;
            }
            for r in &list[from..] {
                let i = check_record(game, 7000, r);
                assert_eq!(
                    u64::from(i),
                    next,
                    "game {game}: the records after its snapshot, in order, no gap"
                );
                next += 5;
            }
            assert!(
                next - 5 + 4 >= last_ack,
                "game {game}: every acknowledged record recovered ({} >= {last_ack} - 4)",
                next - 5
            );
        }
        for id in rec.keys() {
            if *id >= SHORT0 {
                assert!(
                    *id > last_commit,
                    "short game {id} committed (acknowledged up to {last_commit}) is not resurrected"
                );
            }
        }
        assert!(snapshots >= 3, "the long games were compacted ({snapshots} start from a snapshot)");
        let segs = segments(dir.path(), COMPACT_SHARD).len();
        // 15,000 records of 49 bytes are about 45 segments of 16 KiB; a handful stay on disk.
        assert!(max_segs <= 8 && segs <= 8, "journal bounded (at most {max_segs} segments, {segs} left)");
        j.close().await.unwrap();
    });
}

#[test]
fn crash_simulation_with_compaction_sigkill_mid_writes_and_deletions_fsync_off() {
    for _ in 0..3 {
        compacting_crash_run(false);
    }
}

#[test]
fn crash_simulation_with_compaction_fsync_on() {
    compacting_crash_run(true);
}
