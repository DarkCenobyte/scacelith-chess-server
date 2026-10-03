//! Port of store.journal.test.js: append, flush and recovery, group commit, torn tails, CRC
//! mismatches, rotation, deletion of committed games' segments.

use std::fs;
use std::time::Duration;

use super::support::*;
use crate::journal::format::{RECORD_OVERHEAD, parse_segment};
use crate::journal::{JournalError, JournalOptions, MAX_PAYLOAD, ParseError, RecordKind};

use RecordKind::{Committed, Created, Ended, Event, Move};

#[tokio::test]
async fn append_flush_reopen_brings_back_the_uncommitted_games_with_their_records_in_order() {
    let dir = TempDir::new("basic");
    let j = open(opts(dir.path())).await;
    assert!(j.recover().is_empty());
    let big = (1u64 << 53) - 1;
    let recs: [(RecordKind, u64, &[u8], f64); 8] = [
        (Created, 11, b"{\"white\":1}", 1000.5),
        (Created, big, b"{\"white\":2}", 1001.0),
        (Move, 11, &[1, 2], 1002.0),
        (Move, big, &[3, 4], 1003.0),
        (Event, 11, &[], 1004.0),
        (Created, 12, b"{}", 1005.0),
        (Move, 11, &[5], 1006.0),
        (Ended, 12, &[9], 1007.0),
    ];
    for (kind, game, payload, at) in recs {
        j.append(kind, game, payload, at).unwrap();
    }
    j.append(Move, 12, b"text payload", 1008.0).unwrap();
    j.committed(12).unwrap();
    j.flush().await.unwrap();
    j.close().await.unwrap();

    let j = open(opts(dir.path())).await;
    let rec = j.recover();
    assert_eq!(rec.keys().copied().collect::<Vec<_>>(), vec![11, big], "first-appearance order");
    let of = |id| rec[&id].iter().map(|r| (r.kind, r.at, r.payload.clone())).collect::<Vec<_>>();
    assert_eq!(
        of(11),
        vec![
            (Created, 1000.5, b"{\"white\":1}".to_vec()),
            (Move, 1002.0, vec![1, 2]),
            (Event, 1004.0, vec![]),
            (Move, 1006.0, vec![5]),
        ]
    );
    assert_eq!(rec[&big].iter().map(|r| r.kind).collect::<Vec<_>>(), vec![Created, Move]);
    let stats = j.stats();
    assert_eq!(stats.recovery.records, 10);
    assert_eq!((stats.recovery.segments, stats.recovery.games), (1, 2));
    assert!(stats.recovery.problems.is_empty());
    assert_eq!(j.append(Move, 1 << 53, &[], 0.0), Err(JournalError::BadGameId(1 << 53)));
    let huge = vec![0u8; MAX_PAYLOAD + 1];
    assert_eq!(j.append(Move, 1, &huge, 0.0), Err(JournalError::PayloadTooLarge(MAX_PAYLOAD + 1)));
    j.append(Move, 1, &huge[..MAX_PAYLOAD], 0.0).unwrap();
    j.close().await.unwrap();
    assert_eq!(j.append(Move, 1, &[], 0.0), Err(JournalError::Closed));
    assert_eq!(j.committed(1), Err(JournalError::Closed));
    assert_eq!(j.append(Move, 1, &[], 0.0).unwrap_err().to_string(), "journal closed");
    j.flush().await.unwrap();
    j.close().await.unwrap();
    // The largest payload went to disk with the close.
    let j = open(opts(dir.path())).await;
    assert_eq!(j.recover()[&1][0].payload.len(), MAX_PAYLOAD);
    j.close().await.unwrap();
}

#[tokio::test]
async fn group_commit_a_flush_cuts_the_batch_and_appends_during_a_write_join_the_next_one() {
    let dir = TempDir::new("basic");
    // A long timer: the I/O thread must not write the next batch before the checks below.
    let j = open(JournalOptions { shard: 1, flush_ms: 2000, fsync: true, ..opts(dir.path()) }).await;
    for i in 0..100u8 {
        j.append(Move, 5, &[i; 20], f64::from(i)).unwrap();
    }
    assert!(j.has_unwritten());
    let first = j.flush();
    assert!(j.stats().writing, "the batch is cut at the call");
    j.append(Move, 5, &[200; 20], 200.0).unwrap(); // lands in the next batch
    assert!(j.stats().pending_bytes > 0);
    first.await.unwrap();
    let file = seg_path(dir.path(), &segments(dir.path(), 1)[0], 1);
    let size = |extra: u64| (100 + extra) * (RECORD_OVERHEAD as u64 + 20);
    assert_eq!(fs::metadata(&file).unwrap().len(), size(0));
    j.flush().await.unwrap();
    assert_eq!(fs::metadata(&file).unwrap().len(), size(1));
    assert_eq!(j.stats().pending_bytes, 0);
    assert!(!j.has_unwritten());
    j.flush().await.unwrap(); // nothing pending: resolves at once

    // Without a flush, the timer writes the batch (see also the_timer_writes_a_batch_flush_ms_after_its_first_record).
    j.append(Move, 5, &[1; 20], 300.0).unwrap();
    let t0 = std::time::Instant::now();
    while j.has_unwritten() {
        assert!(t0.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(t0.elapsed() >= Duration::from_millis(1900), "not before flush_ms");
    assert_eq!(fs::metadata(&file).unwrap().len(), size(2));

    // A flush during a write with an empty buffer waits for that write.
    j.append(Move, 5, &[2; 20], 400.0).unwrap();
    let (p1, p2) = (j.flush(), j.flush());
    p2.await.unwrap();
    assert_eq!(fs::metadata(&file).unwrap().len(), size(3));
    p1.await.unwrap();
    // A dropped flush future does not cancel the write.
    j.append(Move, 5, &[3; 20], 500.0).unwrap();
    drop(j.flush());
    j.flush().await.unwrap();
    assert_eq!(fs::metadata(&file).unwrap().len(), size(4));
    // close() flushes what is pending.
    j.append(Move, 5, &[4; 20], 600.0).unwrap();
    j.close().await.unwrap();
    assert_eq!(fs::metadata(&file).unwrap().len(), size(5));
    assert_eq!(j.failed_writes(), 0);
}

#[tokio::test]
async fn a_torn_tail_is_ignored_and_new_records_go_to_a_new_segment() {
    let dir = TempDir::new("basic");
    let j = open(opts(dir.path())).await;
    for i in 0..10u8 {
        j.append(Move, 7, &[i, i, i], f64::from(i)).unwrap();
    }
    j.close().await.unwrap();
    let file = seg_path(dir.path(), &segments(dir.path(), 0)[0], 0);
    let len = fs::metadata(&file).unwrap().len();
    fs::OpenOptions::new().write(true).open(&file).unwrap().set_len(len - 3).unwrap(); // the last record cut

    // Its own logger component: other tests log as "journal" meanwhile.
    let component = "journal-torn-tail-test";
    let logged = JournalOptions { logger: crate::log::Logger::root().child(component), ..opts(dir.path()) };
    let logs = super::LogCapture::start();
    let j = open(logged.clone()).await;
    let r = &j.recover()[&7];
    assert_eq!(r.len(), 9);
    assert_eq!(r[8].payload, vec![8, 8, 8]);
    let problem = &j.stats().recovery.problems[0];
    assert_eq!((problem.segment, problem.error, problem.last), (1, ParseError::Torn, true));
    assert_eq!(problem.offset, 9 * (RECORD_OVERHEAD as u64 + 3));
    assert_eq!(problem.bytes_ignored, RECORD_OVERHEAD as u64);
    let lines = logs.records(component);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["level"], "info", "a torn end of the newest segment is expected after a crash");
    assert_eq!(lines[0]["msg"], "journal segment tail ignored");
    assert_eq!(lines[0]["error"], "torn");
    assert_eq!(lines[0]["bytesIgnored"], RECORD_OVERHEAD as u64);
    drop(logs);
    j.append(Move, 7, &[42], 42.0).unwrap();
    j.close().await.unwrap();
    assert_eq!(segments(dir.path(), 0), names(&[1, 2]), "the torn segment is never appended to");

    // Garbage after valid records (zero-filled blocks after a power loss) is ignored too.
    let second = seg_path(dir.path(), &names(&[2])[0], 0);
    let mut bytes = fs::read(&second).unwrap();
    bytes.extend_from_slice(&[0; 100]);
    fs::write(&second, bytes).unwrap();
    let logs = super::LogCapture::start();
    let j = open(logged).await;
    let ats: Vec<f64> = j.recover()[&7].iter().map(|r| r.at).collect();
    assert_eq!(ats, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 42.0]);
    let problems = &j.stats().recovery.problems;
    assert_eq!(problems.len(), 2);
    assert_eq!((problems[0].error, problems[0].last), (ParseError::Torn, false));
    assert_eq!((problems[1].segment, problems[1].error), (2, ParseError::BadLength));
    let levels: Vec<String> =
        logs.records(component).iter().map(|l| l["level"].as_str().unwrap().to_string()).collect();
    assert_eq!(
        levels,
        vec!["warn", "warn"],
        "a torn tail before the newest segment, or garbage, is a warning"
    );
    j.close().await.unwrap();
}

#[tokio::test]
async fn a_crc_mismatch_stops_the_segment_at_the_corrupt_record() {
    let dir = TempDir::new("basic");
    let j = open(opts(dir.path())).await;
    for i in 0..10u8 {
        j.append(Move, 8, &[i; 10], f64::from(i)).unwrap();
    }
    j.close().await.unwrap();
    let file = seg_path(dir.path(), &segments(dir.path(), 0)[0], 0);
    let mut buf = fs::read(&file).unwrap();
    let rec = RECORD_OVERHEAD + 10;
    buf[6 * rec + 25] ^= 0x40; // one bit of record 6's payload
    fs::write(&file, &buf).unwrap();
    let mut seen = Vec::new();
    let res = parse_segment(&buf, |r| seen.push(r.at));
    assert_eq!((res.end, res.error), (6 * rec, Some(ParseError::Crc)));
    assert_eq!(seen, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    let j = open(opts(dir.path())).await;
    let ats: Vec<f64> = j.recover()[&8].iter().map(|r| r.at).collect();
    assert_eq!(ats, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    let problem = &j.stats().recovery.problems[0];
    assert_eq!(problem.error, ParseError::Crc);
    assert_eq!(problem.bytes_ignored, 4 * rec as u64);
    j.close().await.unwrap();
}

#[tokio::test]
async fn segments_rotate_at_the_size_limit_and_are_read_back_in_order() {
    let dir = TempDir::new("basic");
    let options = JournalOptions { shard: 2, segment_bytes: 4096, ..opts(dir.path()) };
    let j = open(options.clone()).await;
    for i in 0..1000u32 {
        j.append(Move, 100 + u64::from(i % 3), &[(i & 0xff) as u8; 40], f64::from(i)).unwrap();
        if i % 50 == 49 {
            j.flush().await.unwrap();
        }
    }
    j.close().await.unwrap();
    let segs = segments(dir.path(), 2);
    assert!(segs.len() >= 10, "{} segments", segs.len());
    let batch = 50 * (RECORD_OVERHEAD as u64 + 40);
    for s in &segs[..segs.len() - 1] {
        let size = fs::metadata(seg_path(dir.path(), s, 2)).unwrap().len();
        assert!(
            (4096..4096 + batch).contains(&size),
            "a segment exceeds the limit by less than one batch ({size})"
        );
    }
    let j = open(options).await;
    for g in 0..3u32 {
        let ats: Vec<f64> = j.recover()[&(100 + u64::from(g))].iter().map(|r| r.at).collect();
        let want: Vec<f64> = (0..ats.len() as u32).map(|k| f64::from(g + 3 * k)).collect();
        assert_eq!(ats, want);
    }
    assert_eq!(j.stats().recovery.records, 1000);
    j.close().await.unwrap();
}

#[tokio::test]
async fn segments_are_deleted_once_all_their_games_are_committed_the_commit_record_outliving_older_ones() {
    let dir = TempDir::new("basic");
    // segment_bytes 1: every flushed batch goes to its own segment.
    let options = JournalOptions { segment_bytes: 1, ..opts(dir.path()) };
    let j = open(options.clone()).await;
    j.append(Created, 1, b"g1", 1.0).unwrap();
    j.append(Move, 1, b"m1", 2.0).unwrap();
    j.flush().await.unwrap(); // segment 1: game 1
    j.append(Created, 2, b"g2", 3.0).unwrap();
    j.append(Move, 1, b"m2", 4.0).unwrap();
    j.append(Ended, 1, b"", 5.0).unwrap();
    j.flush().await.unwrap(); // segment 2: games 1, 2
    j.committed(1).unwrap();
    j.flush().await.unwrap(); // segment 3: commit of game 1
    assert_eq!(
        segments(dir.path(), 0),
        names(&[2, 3]),
        "segment 1 only held the committed game; segment 3 must outlive segment 2"
    );

    // A crash now: game 1 must not come back (its commit record still exists), game 2 must.
    j.close().await.unwrap();
    let j = open(options.clone()).await;
    assert_eq!(j.recover().keys().copied().collect::<Vec<_>>(), vec![2]);
    assert_eq!(j.recover()[&2].iter().map(|r| r.kind).collect::<Vec<_>>(), vec![Created]);
    assert_eq!(segments(dir.path(), 0), names(&[2, 3]));

    j.append(Move, 2, b"m3", 6.0).unwrap();
    j.append(Ended, 2, b"", 7.0).unwrap();
    j.flush().await.unwrap(); // segment 4
    assert_eq!(segments(dir.path(), 0).len(), 3);
    j.committed(2).unwrap();
    j.flush().await.unwrap(); // segment 5
    assert_eq!(segments(dir.path(), 0), names(&[5]), "only the active segment remains");
    assert_eq!(j.stats().disk_bytes, bytes_on_disk(dir.path(), 0));
    j.close().await.unwrap();
    let j = open(options.clone()).await;
    assert!(j.recover().is_empty());
    assert_eq!(segments(dir.path(), 0), Vec::<String>::new(), "fully committed segments are deleted at open");
    j.append(Created, 3, b"g3", 8.0).unwrap();
    j.close().await.unwrap();
    assert_eq!(segments(dir.path(), 0), names(&[6]), "numbering continues");
}

#[tokio::test]
async fn commit_bookkeeping_across_rotations_with_many_games() {
    let dir = TempDir::new("basic");
    let options = JournalOptions { segment_bytes: 2048, ..opts(dir.path()) };
    let j = open(options.clone()).await;
    let mut live = std::collections::BTreeSet::new();
    for round in 0..60u64 {
        for g in round..round + 5 {
            j.append(Move, 1000 + g, &[g as u8; 30], round as f64).unwrap();
            live.insert(1000 + g);
        }
        let done = 1000 + round; // game `round` ends and is committed
        j.append(Ended, done, b"", round as f64).unwrap();
        j.committed(done).unwrap();
        live.remove(&done);
        j.flush().await.unwrap();
    }
    j.close().await.unwrap();
    let j = open(options).await;
    assert_eq!(keys(&j), live.into_iter().collect::<Vec<_>>());
    assert!(
        segments(dir.path(), 0).len() <= 4,
        "old segments deleted ({} left)",
        segments(dir.path(), 0).len()
    );
    assert_eq!(j.stats().disk_bytes, bytes_on_disk(dir.path(), 0));
    j.close().await.unwrap();
}

#[tokio::test]
async fn committed_records_are_never_recovered_whatever_comes_after_them() {
    let dir = TempDir::new("basic");
    // Records of a committed game after its commit (a late event), and a game committed twice.
    write_segment(
        dir.path(),
        0,
        1,
        &[
            (Created, 1, 1.0, b"c1"),
            (Created, 2, 1.0, b"c2"),
            (Committed, 1, 2.0, b""),
            (Event, 1, 3.0, b"late"),
        ],
    );
    write_segment(
        dir.path(),
        0,
        2,
        &[(Move, 2, 4.0, b"m2"), (Committed, 2, 5.0, b""), (Committed, 2, 6.0, b"")],
    );
    write_segment(dir.path(), 0, 3, &[(Created, 3, 7.0, b"c3")]);
    let j = open(opts(dir.path())).await;
    assert_eq!(keys(&j), vec![3]);
    assert_eq!(j.stats().recovery.records, 8);
    j.close().await.unwrap();
}

#[tokio::test]
async fn take_and_release_recovered_free_the_records() {
    let dir = TempDir::new("basic");
    write_segment(dir.path(), 0, 1, &[(Created, 1, 1.0, b"c1"), (Created, 2, 1.0, b"c2")]);
    let mut j = open(opts(dir.path())).await;
    assert_eq!(j.take_recovered().len(), 2);
    assert!(j.recover().is_empty());
    j.close().await.unwrap();
    let mut j = open(opts(dir.path())).await;
    j.release_recovered();
    assert!(j.recover().is_empty());
    assert_eq!(j.stats().recovery.games, 2, "the numbers of the scan stay");
    j.close().await.unwrap();
}

#[tokio::test]
async fn files_that_are_not_segments_are_left_alone() {
    let dir = TempDir::new("basic");
    write_segment(dir.path(), 0, 3, &[(Created, 1, 1.0, b"c1")]);
    let shard = dir.path().join("shard-0");
    for name in ["segment-3.log", "notes.txt", "segment-0000000004.log.tmp"] {
        fs::write(shard.join(name), b"junk").unwrap();
    }
    let j = open(opts(dir.path())).await;
    assert_eq!(keys(&j), vec![1]);
    assert_eq!(j.stats().recovery.segments, 1);
    j.append(Move, 1, b"m", 2.0).unwrap();
    j.close().await.unwrap();
    assert_eq!(segments(dir.path(), 0), names(&[3, 4]));
    for name in ["segment-3.log", "notes.txt", "segment-0000000004.log.tmp"] {
        assert!(shard.join(name).exists(), "{name} kept");
    }
}

#[tokio::test]
async fn open_fails_cleanly_when_the_directory_cannot_be_created() {
    let dir = TempDir::new("basic");
    let file = dir.path().join("plain-file");
    fs::write(&file, b"x").unwrap();
    let err = crate::journal::Journal::open(opts(&file)).await.unwrap_err();
    assert!(matches!(err, JournalError::Io { .. }), "{err}");
}
