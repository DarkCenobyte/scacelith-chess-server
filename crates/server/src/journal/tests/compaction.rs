//! Port of the journal bookkeeping tests of store.journal-compaction.test.js: snapshots supersede
//! records and release segments, stale games are queued, compaction candidates are handed out a
//! few per batch, and a committed game is never brought back whatever the order of its records.

use super::support::*;
use crate::journal::metrics::{DISK_BYTES, SEGMENTS, SNAPSHOTS};
use crate::journal::{JournalOptions, RecordKind};

use RecordKind::{Created, Ended, Move, Snapshot};

/// segment_bytes 1: every flushed batch goes to its own segment.
fn one_batch_per_segment(dir: &std::path::Path, compact_segments: u64) -> JournalOptions {
    JournalOptions { segment_bytes: 1, compact_segments, ..opts(dir) }
}

#[tokio::test]
async fn a_snapshot_supersedes_earlier_records_and_releases_their_segments_and_stale_games_are_queued_at_open()
 {
    let dir = TempDir::new("compaction");
    // Its own shard: the gauges are per shard.
    let options = JournalOptions { shard: 7, ..one_batch_per_segment(dir.path(), 2) };
    let snapshots_before = SNAPSHOTS.get();
    let j = open(options.clone()).await;
    j.append(Created, 1, b"c1", 1.0).unwrap();
    j.append(Created, 2, b"c2", 1.0).unwrap();
    j.flush().await.unwrap(); // segment 1: games 1, 2
    j.append(Move, 1, b"m1", 2.0).unwrap();
    j.flush().await.unwrap(); // segment 2
    assert!(j.compaction_candidates(usize::MAX).is_empty(), "nothing is 2 segments old yet");
    j.append(Move, 1, b"m2", 3.0).unwrap();
    j.flush().await.unwrap(); // segment 3: both games are 2 segments old
    let mut c = j.compaction_candidates(usize::MAX);
    c.sort_unstable();
    assert_eq!(c, vec![1, 2]);
    assert!(j.compaction_candidates(usize::MAX).is_empty(), "handed out once");
    j.append(Snapshot, 1, b"s1", 4.0).unwrap(); // only game 1 is snapshotted
    j.append(Move, 1, b"m3", 5.0).unwrap();
    j.flush().await.unwrap(); // segment 4
    assert_eq!(
        segments(dir.path(), 7),
        names(&[1, 4]),
        "segments 2 and 3 only held game 1; segment 1 still holds game 2"
    );
    let stats = j.stats();
    assert_eq!(stats.snapshots, 1);
    assert_eq!(stats.disk_bytes, bytes_on_disk(dir.path(), 7));
    assert_eq!(SEGMENTS.with(&["7"]).get(), 2.0);
    assert_eq!(DISK_BYTES.with(&["7"]).get(), bytes_on_disk(dir.path(), 7) as f64);
    assert!(SNAPSHOTS.get() > snapshots_before);
    j.close().await.unwrap();

    let j = open(options.clone()).await;
    let rec = j.recover();
    assert_eq!(texts(&rec[&1]), vec![(Snapshot, "s1".into()), (Move, "m3".into())]);
    assert_eq!(texts(&rec[&2]), vec![(Created, "c2".into())]);
    assert_eq!(j.stats().recovery.snapshots, 1);
    assert_eq!(
        j.compaction_candidates(usize::MAX),
        vec![2],
        "game 2 is queued at open (segment 1 is 3 behind)"
    );
    j.append(Snapshot, 2, b"s2", 6.0).unwrap();
    j.flush().await.unwrap(); // segment 5
    assert_eq!(segments(dir.path(), 7), names(&[4, 5]));
    j.close().await.unwrap();
    let j = open(options).await;
    assert_eq!(texts(&j.recover()[&2]), vec![(Snapshot, "s2".into())]);
    assert!(j.compaction_candidates(usize::MAX).is_empty());
    j.close().await.unwrap();
}

#[tokio::test]
async fn the_committed_record_outlives_the_old_segments_a_snapshotted_game_left_records_in() {
    let dir = TempDir::new("compaction");
    let options = one_batch_per_segment(dir.path(), 2);
    let (a, b) = (10, 20);
    let j = open(options.clone()).await;
    j.append(Created, a, b"A", 1.0).unwrap();
    j.append(Created, b, b"B", 1.0).unwrap();
    j.flush().await.unwrap(); // 1: A and B created
    j.append(Move, a, b"a1", 2.0).unwrap();
    j.flush().await.unwrap(); // 2
    j.append(Move, a, b"a2", 3.0).unwrap();
    j.flush().await.unwrap(); // 3
    let mut c = j.compaction_candidates(usize::MAX);
    c.sort_unstable();
    assert_eq!(c, vec![a, b]);
    j.append(Snapshot, a, b"A@3", 4.0).unwrap();
    j.flush().await.unwrap(); // 4: A no longer needs 1-3
    assert_eq!(segments(dir.path(), 0), names(&[1, 4]));
    j.append(Ended, a, b"", 5.0).unwrap();
    j.committed(a).unwrap();
    j.flush().await.unwrap(); // 5: A committed
    assert_eq!(
        segments(dir.path(), 0),
        names(&[1, 5]),
        "segment 5 (A committed) outlives segment 1, which still holds A's created record"
    );
    // A crash now: A stays committed, B comes back.
    assert_eq!(recovered_keys(dir.path(), &options).await, vec![b]);

    // B's snapshot releases segment 1; then segment 5 has nothing left to protect.
    assert_eq!(j.compaction_candidates(usize::MAX), vec![b], "B was queued again at a later rotation");
    j.append(Snapshot, b, b"B@5", 6.0).unwrap();
    j.flush().await.unwrap(); // 6
    assert_eq!(segments(dir.path(), 0), names(&[6]));
    assert_eq!(j.stats().disk_bytes, bytes_on_disk(dir.path(), 0));
    j.close().await.unwrap();
}

#[tokio::test]
async fn compaction_candidates_a_few_per_batch_never_committed_or_pending_and_queued_again_after_a_failed_write()
 {
    let dir = TempDir::new("compaction");
    let options = JournalOptions { compact_per_flush: 3, ..one_batch_per_segment(dir.path(), 2) };
    let j = open(options).await;
    for g in 1..=7u64 {
        j.append(Created, g, format!("c{g}").as_bytes(), g as f64).unwrap();
    }
    j.flush().await.unwrap(); // 1
    j.append(Ended, 7, b"", 8.0).unwrap();
    j.committed(7).unwrap();
    j.flush().await.unwrap(); // 2: game 7 committed
    j.append(Move, 1, b"m", 9.0).unwrap();
    j.flush().await.unwrap(); // 3: the games of segment 1 are 2 segments old
    let mut c1 = j.compaction_candidates(2);
    assert_eq!(c1.len(), 2, "at most `max` per call");
    for g in &c1 {
        j.append(Snapshot, *g, b"snap", 10.0).unwrap();
    }
    c1.extend(j.compaction_candidates(usize::MAX));
    assert_eq!(
        c1.len(),
        3,
        "and at most compact_per_flush per batch, counting the snapshots already appended"
    );
    j.append(Snapshot, c1[2], b"snap", 10.0).unwrap();
    assert!(j.compaction_candidates(usize::MAX).is_empty(), "the batch already holds 3 snapshots");
    j.flush().await.unwrap(); // 4
    let c2 = j.compaction_candidates(usize::MAX);
    assert_eq!(c2.len(), 3);
    let mut all: Vec<u64> = c1.iter().chain(&c2).copied().collect();
    all.sort_unstable();
    assert_eq!(all, vec![1, 2, 3, 4, 5, 6], "never the committed game 7");

    // A failed write: the snapshots are not known to be durable, the games keep their segments.
    for g in &c2 {
        j.append(Snapshot, *g, b"snap", 11.0).unwrap();
    }
    j.set_write_batch_override(Some(Box::new(|_| {
        Err(io_error(std::io::ErrorKind::Other, "EIO: i/o error"))
    })));
    let err = j.flush().await.unwrap_err();
    assert!(err.to_string().contains("EIO"), "{err}");
    j.set_write_batch_override(None);
    assert!(segments(dir.path(), 0).contains(&names(&[1])[0]), "segment 1 is still needed");
    let mut again = j.compaction_candidates(usize::MAX);
    again.sort_unstable();
    let mut c2_sorted = c2.clone();
    c2_sorted.sort_unstable();
    assert_eq!(again, c2_sorted, "queued again");

    // A game handed out but not snapshotted (the host no longer has it) is queued again at the
    // next rotation; the games snapshotted in segment 4 are not stale yet.
    let skipped = again[0];
    for g in &again[1..] {
        j.append(Snapshot, *g, b"snap", 12.0).unwrap();
    }
    assert!(j.compaction_candidates(usize::MAX).is_empty(), "handed out already");
    j.flush().await.unwrap(); // 5 (a new segment after the failure)
    assert!(segments(dir.path(), 0).contains(&names(&[1])[0]), "the skipped game still needs segment 1");
    assert_eq!(j.compaction_candidates(usize::MAX), vec![skipped]);
    j.append(Snapshot, skipped, b"snap", 14.0).unwrap();
    j.flush().await.unwrap(); // 6
    assert_eq!(segments(dir.path(), 0), names(&[4, 5, 6]));
    let closing = j.close();
    assert!(j.compaction_candidates(usize::MAX).is_empty(), "nothing while closing");
    closing.await.unwrap();
}

#[tokio::test]
async fn the_games_of_a_failed_write_are_snapshotted_first_whatever_their_age_never_a_committed_one() {
    let dir = TempDir::new("compaction");
    let options = JournalOptions { compact_segments: 4, compact_per_flush: 2, ..opts(dir.path()) };
    let j = open(options.clone()).await;
    j.append(Created, 1, b"c1", 1.0).unwrap();
    j.append(Created, 5, b"c5", 1.0).unwrap();
    j.append(Ended, 5, b"", 2.0).unwrap();
    j.committed(5).unwrap();
    j.append(Created, 6, b"c6", 2.0).unwrap();
    j.flush().await.unwrap(); // 1: game 5 committed
    // The failing batch: moves of games 1 and 6 (young: nothing to compact), the created records
    // of games 2 and 3 (not tracked yet), game 4 committed in the same batch, a late record of
    // the committed game 5.
    j.append(Move, 1, b"m1", 3.0).unwrap();
    j.append(Move, 6, b"m6", 3.0).unwrap();
    j.append(Created, 2, b"c2", 3.0).unwrap();
    j.append(Created, 3, b"c3", 3.0).unwrap();
    j.append(Created, 4, b"c4", 3.0).unwrap();
    j.append(Ended, 4, b"", 4.0).unwrap();
    j.committed(4).unwrap();
    j.append(Move, 5, b"late", 4.0).unwrap();
    j.set_write_batch_override(Some(fail_once(std::io::ErrorKind::Other, "EIO: i/o error")));
    assert!(j.flush().await.unwrap_err().to_string().contains("EIO"));
    assert_eq!(
        j.stats().heal_queue,
        4,
        "games 1, 6, 2 and 3; not 4, whose committed record is appended again, nor the committed game 5"
    );
    // Game 6 ends and is committed meanwhile.
    j.append(Ended, 6, b"", 5.0).unwrap();
    j.committed(6).unwrap();
    j.flush().await.unwrap(); // 2 (a new segment after the failure)
    assert_eq!(j.compaction_candidates(1), vec![1], "at most `max` per call, first the lost records");
    j.append(Snapshot, 1, b"snap1", 5.0).unwrap();
    assert_eq!(
        j.compaction_candidates(usize::MAX),
        vec![2],
        "at most compact_per_flush per batch; never a committed game; game 2 is not tracked"
    );
    j.append(Snapshot, 2, b"snap2", 5.0).unwrap();
    assert!(j.compaction_candidates(usize::MAX).is_empty(), "the batch already holds 2 snapshots");
    j.flush().await.unwrap(); // 3
    assert_eq!(j.compaction_candidates(usize::MAX), vec![3]);
    j.append(Snapshot, 3, b"snap3", 6.0).unwrap();
    j.flush().await.unwrap();
    assert_eq!(j.stats().heal_queue, 0);
    assert!(j.compaction_candidates(usize::MAX).is_empty());
    j.close().await.unwrap();
    // A restart replays each of them from its snapshot, and not the committed ones.
    let c = open(options).await;
    assert_eq!(keys(&c), vec![1, 2, 3]);
    for g in [1u64, 2, 3] {
        assert_eq!(texts(&c.recover()[&g]), vec![(Snapshot, format!("snap{g}"))]);
    }
    c.close().await.unwrap();
}

#[tokio::test]
async fn a_failed_write_holding_committed_records_appends_them_again_and_the_segments_still_go() {
    let dir = TempDir::new("compaction");
    let options = one_batch_per_segment(dir.path(), 2);
    let j = open(options.clone()).await;
    j.append(Created, 1, b"c1", 1.0).unwrap();
    j.flush().await.unwrap(); // 1
    j.append(Move, 1, b"m1", 2.0).unwrap();
    j.append(Ended, 1, b"", 3.0).unwrap();
    j.flush().await.unwrap(); // 2
    j.committed(1).unwrap(); // the database has game 1: the host forgets it
    j.set_write_batch_override(Some(fail_once(
        std::io::ErrorKind::StorageFull,
        "ENOSPC: no space left on device",
    )));
    let failures = j.failed_writes();
    let err = j.flush().await.unwrap_err();
    assert!(err.to_string().contains("ENOSPC"), "{err}");
    assert_eq!(j.failed_writes(), failures + 1);
    assert!(j.has_unwritten(), "the committed record waits for the next write");
    assert_eq!(segments(dir.path(), 0), names(&[1, 2]));
    j.flush().await.unwrap(); // 3 (a new segment after the failure)
    assert_eq!(segments(dir.path(), 0), names(&[3]), "game 1's segments are deleted");
    assert!(j.shared.state.lock().books.games[&1].committed);
    assert_eq!(j.stats().disk_bytes, bytes_on_disk(dir.path(), 0));
    // A restart does not bring game 1 back.
    assert!(recovered_keys(dir.path(), &options).await.is_empty());
    j.close().await.unwrap();
}

#[tokio::test]
async fn a_game_committed_while_its_snapshot_is_written_is_never_brought_back_whatever_the_order() {
    for snapshot_first in [true, false] {
        let order =
            if snapshot_first { "snapshot, then commit in the next batch" } else { "commit, then snapshot" };
        let dir = TempDir::new("compaction");
        let options = one_batch_per_segment(dir.path(), 1);
        let j = open(options.clone()).await;
        j.append(Created, 1, b"c1", 1.0).unwrap();
        j.append(Created, 2, b"c2", 1.0).unwrap();
        j.flush().await.unwrap(); // 1
        j.append(Move, 1, b"m", 2.0).unwrap();
        j.flush().await.unwrap(); // 2: both games are stale
        let mut c = j.compaction_candidates(usize::MAX);
        c.sort_unstable();
        assert_eq!(c, vec![1, 2], "{order}");
        if snapshot_first {
            j.append(Snapshot, 1, b"s1", 3.0).unwrap();
            let writing = j.flush(); // 3: the snapshot is being written...
            j.append(Ended, 1, b"", 4.0).unwrap(); // ... while the game ends and is committed
            j.committed(1).unwrap();
            writing.await.unwrap();
            assert_eq!(
                recovered_keys(dir.path(), &options).await,
                vec![1, 2],
                "{order}: game 1 from its snapshot"
            );
            j.flush().await.unwrap(); // 4
        } else {
            j.append(Ended, 1, b"", 4.0).unwrap();
            j.committed(1).unwrap();
            j.append(Snapshot, 1, b"s1", 5.0).unwrap();
            j.flush().await.unwrap(); // 3
        }
        assert_eq!(recovered_keys(dir.path(), &options).await, vec![2], "{order}: game 1 committed");
        j.append(Snapshot, 2, b"s2", 6.0).unwrap();
        j.flush().await.unwrap();
        assert_eq!(recovered_keys(dir.path(), &options).await, vec![2], "{order}");
        assert_eq!(
            segments(dir.path(), 0),
            names(&[j.stats().seq]),
            "{order}: only the newest segment is left"
        );
        j.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_journal_written_without_snapshots_recovers_exactly_then_is_compacted() {
    // Many games over many segments and no snapshot record, written directly 60 records per
    // segment: what a long run without compaction leaves. The model's tape keeps every record.
    let tape_dir = TempDir::new("tape");
    let tj = open(opts(tape_dir.path())).await;
    let mut m = Model::new(1);
    let long = [m.create(&tj), m.create(&tj), m.create(&tj)];
    let mut short: Vec<(u64, usize)> = Vec::new();
    let (mut created, mut stuck) = (0, 0);
    for step in 0..400usize {
        for id in long {
            m.play(&tj, id, 12);
        }
        let mut still = Vec::new();
        for (id, plies) in short {
            if m.games[&id].len() < plies {
                m.play(&tj, id, 12);
                still.push((id, plies));
            } else {
                m.end(&tj, id);
                if id != stuck {
                    m.commit(&tj, id);
                }
            }
        }
        short = still;
        while short.len() < 3 {
            let id = m.create(&tj);
            short.push((id, 4 + (created % 9)));
            created += 1;
            if created == 5 {
                stuck = id; // its commit keeps failing: it stays in the journal
            }
        }
        if step % 50 == 0 {
            tj.flush().await.unwrap();
        }
    }
    tj.close().await.unwrap();
    let tape = std::mem::take(&mut m.tape);
    let dir = TempDir::new("compaction");
    const PER: usize = 60;
    let n_segs = tape.len().div_ceil(PER) as u64;
    for (s, chunk) in tape.chunks(PER).enumerate() {
        let recs: Vec<(RecordKind, u64, f64, &[u8])> =
            chunk.iter().map(|(k, g, at, p)| (*k, *g, *at, p.as_slice())).collect();
        write_segment(dir.path(), 0, s as u64 + 1, &recs);
    }
    let options = JournalOptions { segment_bytes: 8192, compact_segments: 2, ..opts(dir.path()) };
    let j = open(options.clone()).await;
    let stats = j.stats();
    assert_eq!((stats.recovery.segments as u64, stats.recovery.snapshots), (n_segs, 0));
    let expected = m.games.clone();
    assert!(expected.contains_key(&stuck));
    assert_eq!(
        keys(&j),
        expected.keys().copied().collect::<Vec<_>>(),
        "the games not committed, and only them"
    );
    for (id, records) in j.recover() {
        let raw: Vec<_> =
            tape.iter().filter(|r| r.1 == *id).map(|(k, _, at, p)| (*k, *at, p.clone())).collect();
        let got: Vec<_> = records.iter().map(|r| (r.kind, r.at, r.payload.clone())).collect();
        assert_eq!(got, raw, "game {id}: its records as written");
        assert_eq!(replay(records), expected[id]);
    }
    assert!(stats.compact_queue >= long.len(), "the old games are queued at open");

    // The host takes the games back and compacts them, a few at a time.
    let mut m = Model::recovered(&j, 10_000);
    let mut rounds = 0;
    let old = |dir: &std::path::Path| {
        segments(dir, 0).iter().any(|n| crate::journal::format::segment_seq(n).unwrap() <= n_segs)
    };
    while old(dir.path()) && rounds < 300 {
        rounds += 1;
        let n = m.compact(&j, 2);
        assert!(n <= 2);
        m.play(&j, long[rounds % long.len()], 12);
        j.flush().await.unwrap();
    }
    assert!(!old(dir.path()), "every old segment deleted ({rounds} rounds)");
    assert!(segments(dir.path(), 0).len() <= 4, "{} segments left", segments(dir.path(), 0).len());
    assert_eq!(j.stats().disk_bytes, bytes_on_disk(dir.path(), 0));
    let now = m.games.clone();
    j.close().await.unwrap();
    let j = open(options.clone()).await;
    for id in long.iter().chain([&stuck]) {
        assert_eq!(j.recover()[id][0].kind, Snapshot, "game {id} starts from its snapshot");
    }
    j.close().await.unwrap();
    Model::verify(dir.path(), &options, &now, "after the compaction").await;
}
