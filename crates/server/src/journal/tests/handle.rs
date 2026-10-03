//! The handle: options, the flush timer, concurrent appends, closing and dropping, stats.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::support::*;
use crate::config::Config;
use crate::journal::format::RECORD_OVERHEAD;
use crate::journal::{COMPACT_PER_FLUSH, JournalError, JournalOptions, RecordKind, SEGMENT_BYTES};

use RecordKind::{Created, Move};

#[test]
fn options_from_the_configuration() {
    let mut config = Config::for_tests();
    config.journal_dir = "/var/lib/scacelith/journal".into();
    config.journal_flush_ms = 25;
    config.journal_fsync = false;
    config.journal_compact_segments = 0;
    let o = JournalOptions::from_config(&config, 3);
    assert_eq!(o.dir, std::path::PathBuf::from("/var/lib/scacelith/journal"));
    assert_eq!((o.shard, o.flush_ms, o.fsync, o.compact_segments), (3, 25, false, 1));
    assert_eq!((o.segment_bytes, o.compact_per_flush), (SEGMENT_BYTES, COMPACT_PER_FLUSH));
    assert!(o.probe.is_none());
    assert!(format!("{o:?}").contains("shard: 3"));
}

#[tokio::test]
async fn the_timer_writes_a_batch_flush_ms_after_its_first_record() {
    let dir = TempDir::new("handle");
    let j = open(JournalOptions { flush_ms: 300, ..opts(dir.path()) }).await;
    assert_eq!(j.dir(), dir.path().join("shard-0"));
    let t0 = Instant::now();
    j.append(Created, 1, b"c1", 1.0).unwrap();
    // Later records do not push the deadline back.
    for i in 0..5 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        j.append(Move, 1, &[i], 2.0).unwrap();
    }
    while j.has_unwritten() {
        assert!(t0.elapsed() < Duration::from_secs(5), "written by the timer");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let took = t0.elapsed();
    assert!(took >= Duration::from_millis(290), "not before flush_ms ({took:?})");
    assert!(took < Duration::from_millis(290 + 1500), "about flush_ms after the first record ({took:?})");
    assert_eq!(j.stats().segment_bytes, 6 * RECORD_OVERHEAD as u64 + 2 + 5);
    j.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn appends_and_flushes_from_many_tasks_all_land_in_order_per_game() {
    let dir = TempDir::new("handle");
    let options = JournalOptions { flush_ms: 2, fsync: true, segment_bytes: 8192, ..opts(dir.path()) };
    let j = Arc::new(open(options.clone()).await);
    let mut tasks = Vec::new();
    for t in 0..8u64 {
        let j = j.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..200u32 {
                j.append(Move, 100 + t, &i.to_le_bytes(), f64::from(i)).unwrap();
                if i % 16 == 15 {
                    j.flush().await.unwrap();
                }
            }
            j.flush().await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert!(!j.has_unwritten());
    let stats = j.stats();
    assert_eq!(stats.disk_bytes, bytes_on_disk(dir.path(), 0));
    assert_eq!(stats.segments, segments(dir.path(), 0).len());
    j.close().await.unwrap();
    let c = open(options).await;
    for t in 0..8u64 {
        let got: Vec<u32> = c.recover()[&(100 + t)]
            .iter()
            .map(|r| u32::from_le_bytes(r.payload[..4].try_into().unwrap()))
            .collect();
        assert_eq!(got, (0..200).collect::<Vec<u32>>(), "game {}", 100 + t);
    }
    c.close().await.unwrap();
}

#[tokio::test]
async fn dropping_the_handle_writes_what_is_pending() {
    let dir = TempDir::new("handle");
    let j = open(opts(dir.path())).await;
    j.append(Created, 1, b"c1", 1.0).unwrap();
    j.append(Move, 1, b"m1", 2.0).unwrap();
    let flush = j.flush();
    drop(j);
    flush.await.unwrap();
    let file = seg_path(dir.path(), &names(&[1])[0], 0);
    let want = 2 * RECORD_OVERHEAD as u64 + 4;
    let t0 = Instant::now();
    while std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) < want {
        assert!(t0.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Let the thread exit before reopening the directory.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let c = open(opts(dir.path())).await;
    assert_eq!(texts(&c.recover()[&1]), vec![(Created, "c1".into()), (Move, "m1".into())]);
    c.close().await.unwrap();
}

#[tokio::test]
async fn a_dropped_handle_still_writes_records_appended_without_a_flush() {
    let dir = TempDir::new("handle");
    let j = open(JournalOptions { flush_ms: 60_000, ..opts(dir.path()) }).await;
    j.append(Created, 1, b"c1", 1.0).unwrap();
    drop(j);
    let file = seg_path(dir.path(), &names(&[1])[0], 0);
    let t0 = Instant::now();
    while std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) == 0 {
        assert!(t0.elapsed() < Duration::from_secs(5), "written at once, not after flush_ms");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn close_is_idempotent_and_a_closed_journal_refuses_records() {
    let dir = TempDir::new("handle");
    let j = open(opts(dir.path())).await;
    j.append(Created, 1, b"c1", 1.0).unwrap();
    let (a, b) = (j.close(), j.close());
    a.await.unwrap();
    b.await.unwrap();
    j.close().await.unwrap();
    assert_eq!(j.append(Move, 1, b"m", 2.0), Err(JournalError::Closed));
    j.flush().await.unwrap();
    assert!(!j.has_unwritten());
    assert!(j.compaction_candidates(usize::MAX).is_empty());
    let c = open(opts(dir.path())).await;
    assert_eq!(keys(&c), vec![1]);
    c.close().await.unwrap();
}

#[tokio::test]
async fn stats_describe_the_journal() {
    let dir = TempDir::new("handle");
    write_segment(dir.path(), 0, 4, &[(Created, 1, 1.0, b"c1"), (Created, 2, 1.0, b"c2")]);
    let j = open(JournalOptions { compact_segments: 2, ..opts(dir.path()) }).await;
    let s = j.stats();
    assert_eq!((s.segments, s.seq, s.segment_bytes, s.pending_bytes), (1, 4, 0, 0));
    assert_eq!(
        (s.disk_bytes, s.snapshots, s.compact_queue, s.heal_queue),
        (2 * RECORD_OVERHEAD as u64 + 4, 0, 0, 0)
    );
    assert_eq!((s.tracked_games, s.writing), (2, false));
    assert_eq!((s.recovery.segments, s.recovery.records, s.recovery.games), (1, 2, 2));
    j.append(Move, 1, b"m", 2.0).unwrap();
    assert_eq!(j.stats().pending_bytes, RECORD_OVERHEAD + 1);
    j.flush().await.unwrap();
    let s = j.stats();
    assert_eq!((s.segments, s.seq, s.segment_bytes, s.pending_bytes), (2, 5, RECORD_OVERHEAD as u64 + 1, 0));
    assert_eq!(s.disk_bytes, bytes_on_disk(dir.path(), 0));
    j.close().await.unwrap();
}
