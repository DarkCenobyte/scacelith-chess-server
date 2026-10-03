//! Port of the long-game tests of store.journal-compaction.test.js at the journal level, with a
//! model host: the journal stays bounded while long games run, every crash point of a compaction
//! (snapshots appended, written, each deletion, torn snapshot batch) recovers each game exactly,
//! and a journal reopened in place again and again (clean stops, torn tails) never loses a game
//! nor brings a committed one back.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use super::support::*;
use crate::journal::format::segment_name;
use crate::journal::{IoOp, IoProbe, JournalOptions, RecordKind};

/// Copies of the journal directory taken by the probe at the crash points of one compaction.
#[derive(Default)]
struct CrashPoints {
    armed: bool,
    /// Copies made so far: label and directory.
    copies: Vec<(String, PathBuf)>,
    /// The copy taken once the snapshot batch is written, before anything is released.
    written: Option<PathBuf>,
}

#[tokio::test]
async fn long_games_stay_bounded_and_every_crash_point_of_a_compaction_recovers_each_game() {
    let dir = TempDir::new("long");
    let scratch = TempDir::new("long-copies");
    let copies = Arc::new(AtomicU64::new(0));
    let copy_dir = {
        let (src, scratch, copies) = (dir.path().to_path_buf(), scratch.path().to_path_buf(), copies.clone());
        move || {
            let dst = scratch.join(format!("c{}", copies.fetch_add(1, Ordering::SeqCst)));
            copy_shard(&src, &dst, 0);
            dst
        }
    };
    let points = Arc::new(Mutex::new(CrashPoints::default()));
    let probe: IoProbe = {
        let (points, copy_dir) = (points.clone(), copy_dir.clone());
        Arc::new(move |op| {
            let mut p = points.lock();
            if p.armed {
                match op {
                    // Fsync off: only a batch holding a snapshot is fdatasynced, right after its
                    // write and before its bookkeeping releases anything.
                    IoOp::Datasync(_) if p.written.is_none() => p.written = Some(copy_dir()),
                    IoOp::Unlink(seq) => {
                        let label = format!("before deleting {}", segment_name(seq));
                        p.copies.push((label, copy_dir()));
                    }
                    _ => {}
                }
            }
            Ok(())
        })
    };
    let options = JournalOptions {
        segment_bytes: 16384,
        compact_segments: 2,
        compact_per_flush: 2,
        probe: Some(probe),
        ..opts(dir.path())
    };
    let j = open(options.clone()).await;
    let mut m = Model::new(1);
    let mut long = vec![m.create(&j), m.create(&j), m.create(&j)];
    let first_long = long.clone();
    let mut short: Vec<(u64, usize)> = Vec::new();
    let (mut created, mut stuck) = (0usize, 0u64);
    let (mut deep_probes, mut deletion_points, mut verified, mut max_segs) = (0, 0, 0, 0);
    const STEPS: usize = 900;
    for step in 1..=STEPS {
        if step % 2 == 0 {
            for id in &long {
                m.play(&j, *id, 20);
            }
        }
        let mut still = Vec::new();
        for (id, plies) in short {
            if m.games[&id].len() < plies {
                m.play(&j, id, 20);
                still.push((id, plies));
            } else {
                m.end(&j, id);
                still.push((id, 0));
            }
        }
        // Ended games are committed after a journal flush (the commit gate), except one whose
        // database commit keeps failing: it stays in the journal.
        short = still.into_iter().filter(|(_, plies)| *plies > 0).collect();
        let ended: Vec<u64> = m.ended.iter().copied().filter(|id| *id != stuck).collect();
        if !ended.is_empty() {
            j.flush().await.unwrap();
            for id in ended {
                m.commit(&j, id);
            }
        }
        while short.len() < 4 {
            let id = m.create(&j);
            short.push((id, 8 + (created * 7) % 33));
            created += 1;
            if created == 3 {
                stuck = id;
            }
        }
        // Events only a snapshot carries for a while.
        if step % 50 == 25 {
            m.event(&j, long[0]);
        }
        if step == 300 {
            long.push(m.create(&j)); // a younger long game
        }
        if step == 600 {
            let id = long.remove(2);
            m.end(&j, id);
        }

        if j.stats().compact_queue > 0 && step % 2 == 0 {
            // Crash points of one compaction: the batch holds only snapshots (everything else
            // was flushed before), so recovery must rebuild the current state at every point.
            j.flush().await.unwrap();
            if m.compact(&j, 2) > 0 {
                deep_probes += 1;
                let expected = m.games.clone();
                let batch = j.stats().pending_bytes as u64;
                let mut all = vec![("snapshots appended, not written".to_string(), copy_dir())];
                points.lock().armed = true;
                j.flush().await.unwrap();
                let (written, deletions) = {
                    let mut p = points.lock();
                    p.armed = false;
                    (
                        p.written.take().expect("the snapshot batch was fdatasynced"),
                        std::mem::take(&mut p.copies),
                    )
                };
                deletion_points += deletions.len();
                all.extend(deletions);
                all.push(("after the deletions".into(), copy_dir()));
                // Torn snapshot batch (a power loss before its fsync completed): cut inside it.
                let last = segments(&written, 0).pop().unwrap();
                let size = fs::metadata(seg_path(&written, &last, 0)).unwrap().len();
                let start = size - batch;
                for cut in [start + 3, start + batch / 2, size - 1] {
                    let d = scratch.path().join(format!("c{}", copies.fetch_add(1, Ordering::SeqCst)));
                    copy_shard(&written, &d, 0);
                    fs::OpenOptions::new()
                        .write(true)
                        .open(seg_path(&d, &last, 0))
                        .unwrap()
                        .set_len(cut)
                        .unwrap();
                    all.push((format!("snapshot batch torn at {}/{batch}", cut - start), d));
                }
                all.push(("snapshots written, nothing released".into(), written));
                for (label, d) in all {
                    Model::verify(&d, &options, &expected, &format!("step {step}, {label}")).await;
                    fs::remove_dir_all(&d).unwrap();
                    verified += 1;
                }
            }
        } else {
            m.compact(&j, 2); // snapshots in the same batch as the moves
            j.flush().await.unwrap();
            if step % 3 == 0 {
                let d = copy_dir();
                Model::verify(&d, &options, &m.games, &format!("step {step}")).await;
                fs::remove_dir_all(&d).unwrap();
                verified += 1;
            }
        }
        let st = j.stats();
        max_segs = max_segs.max(st.segments);
        assert_eq!(st.segments, segments(dir.path(), 0).len(), "step {step}: segments");
        assert_eq!(st.disk_bytes, bytes_on_disk(dir.path(), 0), "step {step}: disk bytes");
    }

    let st = j.stats();
    assert!(
        deep_probes >= 10 && deletion_points >= 10,
        "crash points ({deep_probes} compactions, {deletion_points} deletions)"
    );
    assert!(st.snapshots >= deep_probes as u64 && st.snapshots == m.snapshots);
    assert!(st.seq >= 4 * max_segs as u64, "{} segments written, at most {max_segs} on disk", st.seq);
    assert!(max_segs <= 2 + 4, "at most {max_segs} segments on disk");
    assert!(
        !segments(dir.path(), 0).contains(&segment_name(1)),
        "the long games no longer pin their first segment"
    );
    assert!(
        m.games.contains_key(&stuck) && m.ended.contains(&stuck),
        "the game whose commit fails is still pending"
    );
    for id in &first_long[..2] {
        assert!(m.games[id].len() > 400, "long games spanning the whole run");
    }
    assert!(verified > 100, "{verified} journal states verified");

    // A clean restart on the directory: the games not committed come back, from their snapshots.
    m.play(&j, long[0], 20);
    j.flush().await.unwrap();
    let expected = m.games.clone();
    j.close().await.unwrap();
    let j2 = open(JournalOptions { probe: None, ..options.clone() }).await;
    for id in &first_long[..2] {
        assert_eq!(j2.recover()[id][0].kind, RecordKind::Snapshot, "a long game starts from its snapshot");
    }
    j2.close().await.unwrap();
    Model::verify(dir.path(), &options, &expected, "clean restart").await;
}

#[tokio::test]
async fn a_journal_reopened_in_place_again_and_again_keeps_every_game_and_never_brings_a_committed_one_back()
{
    let dir = TempDir::new("restart");
    let options =
        JournalOptions { segment_bytes: 8192, compact_segments: 2, compact_per_flush: 4, ..opts(dir.path()) };
    let mut expected = std::collections::BTreeMap::new();
    let mut committed = BTreeSet::new();
    let (mut max_segs, mut from_snapshot, mut snapshots) = (0, 0, 0);
    let mut long: Vec<u64> = Vec::new();
    let mut next_id = 1;
    const LIVES: usize = 8;
    const STEPS: usize = 120;
    for life in 0..LIVES {
        let j = open(options.clone()).await;
        assert_eq!(
            keys(&j),
            expected.keys().copied().collect::<Vec<_>>(),
            "life {life}: the games not committed, and only them"
        );
        for id in &committed {
            assert!(!j.recover().contains_key(id), "life {life}: committed game {id} not resurrected");
        }
        for (id, records) in j.recover() {
            if records[0].kind == RecordKind::Snapshot {
                from_snapshot += 1;
            }
            assert!(replay(records) == expected[id], "life {life}: game {id} rebuilt exactly");
        }
        let mut m = Model::recovered(&j, next_id);
        if life == 0 {
            long = vec![m.create(&j), m.create(&j), m.create(&j)];
        }
        let mut short: Vec<(u64, usize)> = m
            .games
            .iter()
            .filter(|(id, _)| !long.contains(id) && !m.ended.contains(id))
            .map(|(id, r)| (*id, r.len() + 2))
            .collect();
        for step in 1..=STEPS {
            for id in &long {
                m.play(&j, *id, 16);
            }
            let mut still = Vec::new();
            for (id, plies) in short {
                if m.games[&id].len() < plies {
                    m.play(&j, id, 16);
                    still.push((id, plies));
                } else {
                    m.end(&j, id);
                }
            }
            short = still;
            while short.len() < 3 {
                short.push((m.create(&j), 4 + step % 9));
            }
            let ended = std::mem::take(&mut m.ended);
            if !ended.is_empty() {
                j.flush().await.unwrap();
                for id in ended {
                    m.commit(&j, id);
                    committed.insert(id);
                }
            }
            m.compact(&j, usize::MAX);
            j.flush().await.unwrap();
            max_segs = max_segs.max(segments(dir.path(), 0).len());
        }
        snapshots += j.stats().snapshots;
        expected = m.games.clone();
        next_id = m.games.keys().chain(&committed).max().copied().unwrap_or(0) + 1;
        if life % 2 == 1 {
            // A crash in the middle of the next write: its only record is torn.
            let before = j.stats();
            let size0 = fs::metadata(seg_path(dir.path(), &segment_name(before.seq), 0))
                .map(|m| m.len())
                .unwrap_or(0);
            m.play(&j, long[0], 16);
            j.flush().await.unwrap();
            let after = j.stats();
            let file = seg_path(dir.path(), &segment_name(after.seq), 0);
            let start = if after.seq == before.seq { size0 } else { 0 };
            let size = fs::metadata(&file).unwrap().len();
            fs::OpenOptions::new()
                .write(true)
                .open(&file)
                .unwrap()
                .set_len(start + (size - start) / 2)
                .unwrap();
        }
        j.close().await.unwrap();
    }
    assert!(expected[&long[0]].len() >= LIVES * STEPS / 2, "the long games ran through every life");
    assert!(from_snapshot >= LIVES, "recoveries started from snapshots ({from_snapshot})");
    assert!(snapshots > 0);
    assert!(max_segs <= 2 + 4, "at most {max_segs} segments on disk");
}
