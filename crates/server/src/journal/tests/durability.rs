//! Port of the durability tests of store.journal-compaction.test.js: the order of the
//! fdatasyncs, directory fsyncs and deletions (recorded through the I/O probe), and failed
//! rotations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::support::*;
use crate::journal::{IoOp, JournalOptions, RecordKind};

use Event::{Datasync, SyncDir};
use RecordKind::{Committed, Created, Ended, Move, Snapshot};

#[tokio::test]
async fn fsync_off_a_batch_holding_a_snapshot_is_fdatasynced_before_it_releases_anything() {
    let dir = TempDir::new("durability");
    let rec = Recorder::new();
    let options = JournalOptions {
        segment_bytes: 400,
        compact_segments: 1,
        probe: Some(rec.probe()),
        ..opts(dir.path())
    };
    let j = open(options).await;
    let pad = vec![b'x'; 300];
    j.append(Created, 1, b"c1", 1.0).unwrap();
    j.flush().await.unwrap(); // segment 1 (created without a directory fsync)
    j.append(Move, 1, b"m1", 2.0).unwrap();
    j.flush().await.unwrap();
    assert_eq!(rec.take(), vec![], "batches without a snapshot are not synced");
    j.append(Snapshot, 1, b"s1", 3.0).unwrap();
    j.flush().await.unwrap(); // still segment 1: created since the last directory fsync
    assert_eq!(rec.take(), vec![Datasync, SyncDir]);
    j.append(Snapshot, 1, b"s2", 4.0).unwrap();
    j.flush().await.unwrap(); // segment 1 again: its entry is durable now
    assert_eq!(rec.take(), vec![Datasync]);
    j.append(Move, 1, &pad, 5.0).unwrap();
    j.flush().await.unwrap(); // segment 1 is full after this one
    j.append(Move, 1, b"m3", 6.0).unwrap();
    j.flush().await.unwrap(); // segment 2
    assert_eq!(rec.take(), vec![]);
    j.append(Snapshot, 1, b"s3", 7.0).unwrap();
    j.flush().await.unwrap(); // segment 2: new, its snapshot releases segment 1
    assert_eq!(rec.take(), vec![Datasync, SyncDir, unlink(1)]);
    assert_eq!(segments(dir.path(), 0), names(&[2]));
    j.close().await.unwrap();
    let c = open(opts(dir.path())).await;
    assert_eq!(texts(&c.recover()[&1]), vec![(Snapshot, "s3".into())]);
    c.close().await.unwrap();
}

#[tokio::test]
async fn fsync_on_a_segment_holding_a_committed_record_is_deleted_last_after_a_directory_fsync() {
    for fsync in [true, false] {
        let dir = TempDir::new("durability");
        let rec = Recorder::new();
        let options =
            JournalOptions { fsync, segment_bytes: 1, probe: Some(rec.probe()), ..opts(dir.path()) };
        let j = open(options).await;
        j.append(Created, 1, b"g1", 1.0).unwrap();
        j.append(Move, 1, b"m1", 2.0).unwrap();
        j.flush().await.unwrap(); // 1: game 1
        j.append(Created, 2, b"g2", 3.0).unwrap();
        j.append(Ended, 1, b"", 4.0).unwrap();
        j.flush().await.unwrap(); // 2: games 1 and 2
        j.committed(1).unwrap();
        j.flush().await.unwrap(); // 3: game 1 committed; segment 1 goes
        assert_eq!(segments(dir.path(), 0), names(&[2, 3]));
        j.append(Ended, 2, b"", 5.0).unwrap();
        j.flush().await.unwrap(); // 4
        rec.take();
        j.committed(2).unwrap();
        j.flush().await.unwrap(); // 5: game 2 committed
        assert_eq!(segments(dir.path(), 0), names(&[5]));
        // Segment 3 holds game 1's committed record: it goes after segment 2 (game 1's older
        // records), and with fsync on only once that deletion is durable. With fsync on, the
        // first directory fsync is the rotation's (segment 5 created), and the batch is synced.
        let deletions: Vec<Event> = rec.take().into_iter().filter(|e| *e != Datasync).collect();
        let expected = if fsync {
            vec![SyncDir, unlink(2), unlink(4), SyncDir, unlink(3)]
        } else {
            vec![unlink(2), unlink(4), unlink(3)]
        };
        assert_eq!(deletions, expected, "fsync {fsync}");
        j.close().await.unwrap();
    }
}

#[tokio::test]
async fn open_makes_what_it_read_durable_before_it_deletes_anything_on_its_strength() {
    let dir = TempDir::new("durability");
    let write = || {
        write_segment(dir.path(), 0, 1, &[(Created, 1, 1.0, b"c1"), (Move, 1, 2.0, b"m1")]);
        write_segment(dir.path(), 0, 2, &[(Snapshot, 1, 3.0, b"s1"), (Created, 2, 4.0, b"c2")]);
        write_segment(dir.path(), 0, 3, &[(Ended, 2, 5.0, b""), (Committed, 2, 6.0, b"")]);
    };
    write();
    for fsync in [true, false] {
        let rec = Recorder::new();
        let j = open(JournalOptions { fsync, probe: Some(rec.probe()), ..opts(dir.path()) }).await;
        // Game 1's snapshot (segment 2) releases segment 1; segment 3 (game 2 committed) outlives
        // segment 2, which game 1 still needs. With fsync off, the segment holding the snapshot
        // is made durable all the same before segment 1 goes.
        let expected = if fsync {
            vec![Datasync, Datasync, Datasync, SyncDir, unlink(1)]
        } else {
            vec![Datasync, SyncDir, unlink(1)]
        };
        assert_eq!(rec.take(), expected, "fsync {fsync}");
        assert_eq!(keys(&j), vec![1]);
        assert_eq!(texts(&j.recover()[&1]), vec![(Snapshot, "s1".into())]);
        assert_eq!(segments(dir.path(), 0), names(&[2, 3]));
        j.close().await.unwrap();
        write();
    }
}

#[tokio::test]
async fn a_failed_rotation_does_not_leave_the_previous_segment_active() {
    let dir = TempDir::new("durability");
    let refuse = Arc::new(AtomicBool::new(false));
    let r = refuse.clone();
    let probe: crate::journal::IoProbe = Arc::new(move |op| match op {
        IoOp::Create(_) if r.load(Ordering::SeqCst) => {
            Err(io_error(std::io::ErrorKind::Other, "EMFILE: too many open files"))
        }
        _ => Ok(()),
    });
    let j = open(JournalOptions { segment_bytes: 1, probe: Some(probe), ..opts(dir.path()) }).await;
    j.append(Created, 1, b"g1", 1.0).unwrap();
    j.append(Created, 2, b"g2", 1.0).unwrap();
    j.flush().await.unwrap(); // 1
    refuse.store(true, Ordering::SeqCst);
    j.append(Move, 2, b"lost", 2.0).unwrap();
    let err = j.flush().await.unwrap_err(); // segment 1 closed, segment 2 not created
    assert!(err.to_string().contains("EMFILE"), "{err}");
    refuse.store(false, Ordering::SeqCst);
    assert_eq!(j.stats().seq, 1, "the number is not used");
    j.append(Ended, 1, b"", 3.0).unwrap();
    j.committed(1).unwrap();
    j.flush().await.unwrap(); // 2
    j.append(Ended, 2, b"", 4.0).unwrap();
    j.committed(2).unwrap();
    j.flush().await.unwrap(); // 3
    assert_eq!(segments(dir.path(), 0), names(&[3]));
    assert_eq!(j.stats().disk_bytes, bytes_on_disk(dir.path(), 0));
    assert_eq!(j.failed_writes(), 1);
    j.close().await.unwrap();
}

#[tokio::test]
async fn a_failed_write_or_fsync_after_the_rotation_forces_the_next_batch_into_a_new_segment() {
    for failing in ["write", "datasync"] {
        let dir = TempDir::new("durability");
        let fail = Arc::new(AtomicBool::new(false));
        let f = fail.clone();
        let probe: crate::journal::IoProbe = Arc::new(move |op| {
            let hit =
                matches!((failing, op), ("write", IoOp::Write { .. }) | ("datasync", IoOp::Datasync(_)));
            if hit && f.swap(false, Ordering::SeqCst) {
                return Err(io_error(std::io::ErrorKind::Other, "EIO: i/o error"));
            }
            Ok(())
        });
        let options = JournalOptions { fsync: true, probe: Some(probe), ..opts(dir.path()) };
        let j = open(options.clone()).await;
        j.append(Created, 1, b"c1", 1.0).unwrap();
        j.flush().await.unwrap(); // segment 1
        fail.store(true, Ordering::SeqCst);
        j.append(Move, 1, b"m1", 2.0).unwrap();
        assert!(j.flush().await.is_err(), "{failing}");
        assert_eq!(j.failed_writes(), 1);
        assert_eq!(j.stats().heal_queue, 1, "{failing}: game 1 lost a record");
        j.append(Move, 1, b"m2", 3.0).unwrap();
        j.flush().await.unwrap();
        assert_eq!(segments(dir.path(), 0), names(&[1, 2]), "{failing}: never after a possibly torn write");
        assert_eq!(j.stats().disk_bytes, bytes_on_disk(dir.path(), 0), "{failing}");
        j.close().await.unwrap();
        // The failed fsync's record was written all the same; a failed write left nothing.
        let c = open(options).await;
        let want: Vec<(RecordKind, String)> = match failing {
            "write" => vec![(Created, "c1".into()), (Move, "m2".into())],
            _ => vec![(Created, "c1".into()), (Move, "m1".into()), (Move, "m2".into())],
        };
        assert_eq!(texts(&c.recover()[&1]), want, "{failing}");
        c.close().await.unwrap();
    }
}
