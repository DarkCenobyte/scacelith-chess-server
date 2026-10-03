//! The host with the real modules around it and the commit gate, ported from the reference
//! `game.persistence` suite: the server chess rules, the SQLite store and the on-disk journal; then
//! a journal whose writes are held or fail (fault injection on its I/O thread), with a crash copy
//! of the journal taken at each database commit: the database never has a finished game whose
//! journal still says it runs, and a journal that keeps failing stops holding the commits.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use parking_lot::Mutex;
use scacelith_chess::ChessGame;
use scacelith_protocol::{ClientMsg, EndReason as ER, GameStatus as GS, Move, PlayerInfo, ServerMsg};

use super::{B, Ep, JournalAt, Opts, Rig, T0, TempDir, copy_dir, eio, journal_options, new_game, result};
use crate::clock::ManualClock;
use crate::config::test_config;
use crate::events::NewGame;
use crate::game::host::{GameStore, JOURNAL_GATE_TRIES, RulesFactory};
use crate::game::room::{GameRecord, GameRoom, RecordKind};
use crate::game::testing::{FakeRules, Script};
use crate::ids::{GameId, UserId};
use crate::journal::Journal;
use crate::log::{Capture, Level, Logger, capture_logs};
use crate::store::{GameOutcome, NewUser, RatingFn, RatingRecord, SideOutcome, Store, StoreOptions};

/// Elo with K 40 for everyone (the reference suite's players are in their first games).
pub(super) fn k40() -> RatingFn {
    Arc::new(|w: &RatingRecord, b: &RatingRecord, score: f64| {
        let expected = 1.0 / (1.0 + 10f64.powf((b.rating - w.rating) as f64 / 400.0));
        let side = |r: &RatingRecord, s: f64, e: f64| {
            let after = r.rating + (40.0 * (s - e)).round() as i64;
            let mut rec = *r;
            rec.rating = after;
            rec.games += 1;
            rec.counted_games += 1;
            rec.wins += i64::from(s > 0.75);
            rec.draws += i64::from((0.25..=0.75).contains(&s));
            rec.losses += i64::from(s < 0.25);
            rec.peak = rec.peak.max(after);
            rec.rated = true;
            SideOutcome { before: r.rating, after, k: Some(40), record: rec }
        };
        GameOutcome { white: side(w, score, expected), black: side(b, 1.0 - score, 1.0 - expected) }
    })
}

/// A migrated in-memory store with the K-40 rating, and accounts for `names`.
pub(super) async fn real_store(names: &[&str]) -> (Store, Vec<UserId>) {
    let config = test_config(&[]).expect("configuration");
    let options =
        StoreOptions { path: Some(":memory:".into()), rating: Some(k40()), ..StoreOptions::default() };
    let store = Store::open(&config, options).await.expect("store");
    store.migrate().await.expect("migrations");
    let mut users = Vec::new();
    for name in names {
        let user = NewUser {
            username: (*name).to_owned(),
            email: Some(format!("{name}@example.org")),
            password_hash: None,
            email_verified: true,
            accept_challenges: true,
            created_at: 0,
        };
        users.push(store.users().create(user).await.expect("account"));
    }
    (store, users)
}

/// A player shown with a provisional 1500.
pub(super) fn shown(user_id: UserId, name: &str) -> PlayerInfo {
    PlayerInfo { user_id, name: name.to_owned(), rating: 1500, provisional: true }
}

/// The side to move plays these moves (UCI), one second apart, from its connection if any.
fn play_uci(h: &mut Rig, id: GameId, moves: &[&str], eps: [Option<&Ep>; 2]) {
    for m in moves {
        let room = h.room(id);
        let side = room.side_to_move();
        let msg = Move {
            seq: room.ply() as u32 + 1,
            game: id,
            ply: room.ply() as u16,
            r#move: scacelith_protocol::uci_to_move(m).expect("a UCI move"),
            pos_hash: room.digest(),
            think_ms: 900,
            draw_offer: false,
        };
        let user = room.player(side).user_id;
        h.advance(1000);
        h.send(user, ClientMsg::Move(msg), eps[side.index()]);
    }
}

#[tokio::test]
async fn real_store_journal_and_rules_a_rated_game_is_committed_with_ratings_an_open_one_survives_a_restart()
{
    let dir = TempDir::new("persistence");
    let (store, users) = real_store(&["alice", "bob", "carol"]).await;
    let [alice, bob, carol] = users[..] else { panic!("three accounts") };
    let rules: RulesFactory = Arc::new(|| Box::new(ChessGame::default()));
    let game_store: Arc<dyn GameStore> = Arc::new(store.clone());
    let opts = |t: i64| Opts {
        shard: 0,
        journal: JournalAt::Dir(dir.path().to_owned()),
        rules: Some(rules.clone()),
        store: Some(game_store.clone()),
        t,
        ..Opts::default()
    };
    let mut s = Rig::with(opts(T0)).await;

    // 1. Fool's mate in 3+2, rated.
    let g1 = s.create(NewGame { white: shown(alice, "alice"), black: shown(bob, "bob"), ..new_game(0, 0) });
    let (ew, eb) = (Ep::new(1, alice), Ep::new(2, bob));
    s.shard.attach(g1, alice, ew.endpoint());
    s.shard.attach(g1, bob, eb.endpoint());
    play_uci(&mut s, g1, &["f2f3", "e7e5", "g2g4", "d8h4"], [Some(&ew), Some(&eb)]);
    let end = eb.msgs().into_iter().find_map(|m| match m {
        ServerMsg::GameEnd(e) => Some(e),
        _ => None,
    });
    assert_eq!(end.map(|e| (e.status, e.reason)), Some((GS::BlackWins, ER::Checkmate)));
    s.advance(100); // DB_COMMIT_MS later
    assert!(s.journal().has_unwritten(), "the journal still holds the game's last records");
    let t = s.t();
    assert!(s.shard.poll_commits(t));
    assert!(s.shard.commit_in_flight(), "the commit waits for their flush");
    assert!(s.shard.settle_commit().await);
    let rated = eb.msgs().into_iter().find_map(|m| match m {
        ServerMsg::RatingUpdate(u) => Some(u),
        _ => None,
    });
    let rated = rated.expect("RatingUpdate sent after the commit");
    assert_eq!((rated.black.before, rated.black.after, rated.white.after), (1500, 1520, 1480));
    let row = store.games().by_id(g1).await.expect("read").expect("the committed game");
    assert_eq!(row.summary.status, GS::BlackWins.to_u8());
    assert_eq!(store.ratings().get(bob, "3+2".into()).await.expect("read").rating, 1520);
    assert_eq!(store.ratings().get(alice, "3+2".into()).await.expect("read").games, 1);
    assert_eq!(s.events.ended().iter().map(|e| e.game).collect::<Vec<_>>(), [g1]);

    // 2. A game in progress, then the process goes away (journal written, nothing committed).
    let g2 = s.create(NewGame {
        category: "5+3".into(),
        base_ms: 300_000,
        inc_ms: 3000,
        white: shown(carol, "carol"),
        black: shown(bob, "bob"),
        ..new_game(0, 0)
    });
    play_uci(&mut s, g2, &["e2e4", "c7c5", "g1f3"], [None, None]);
    let before = s.room(g2).snapshot(B, s.t());
    let digest = s.room(g2).digest();
    s.crash().await;

    // 3. Restart on the same directory: g1 is committed (not replayed), g2 is restored.
    let mut s = Rig::with(opts(before.server_time as i64 + 2000)).await;
    assert_eq!(s.shard.recover(), 1);
    assert!(s.journal().recover().is_empty(), "the journal lets the replayed records go");
    assert!(s.shard.room(g1).is_none());
    let room = s.room(g2);
    assert_eq!((room.ply(), room.digest()), (3, digest));
    assert_eq!(s.events.recovered(), [(g2, carol, bob)]);
    let after = room.snapshot(B, s.t());
    assert_eq!(after.white_ms, before.white_ms);
    assert!(after.black_ms <= before.black_ms, "Black's clock did not gain time over the restart");
    // Black reconnects and keeps playing.
    let eb2 = Ep::new(3, bob);
    assert!(s.shard.attach(g2, bob, eb2.endpoint()));
    play_uci(&mut s, g2, &["d7d6"], [None, Some(&eb2)]);
    assert_eq!(s.room(g2).ply(), 4);
    s.crash().await;
    store.close().await;
}

/// Fault injection on a journal's writes, run on its I/O thread before each batch is written.
#[derive(Clone, Default)]
struct Faults(Arc<Mutex<FaultState>>);

#[derive(Default)]
struct FaultState {
    /// Writes still to fail.
    fail_next: u32,
    /// Every write fails.
    fail_all: bool,
    /// The next write waits for the test to say whether it fails.
    gate: Option<mpsc::Receiver<bool>>,
}

impl Faults {
    fn install(&self, journal: &Journal) {
        let state = self.0.clone();
        journal.set_write_batch_override(Some(Box::new(move |_batch: &[u8]| {
            let gate = state.lock().gate.take();
            if let Some(gate) = gate
                && gate.recv().unwrap_or(false)
            {
                return Err(eio());
            }
            let mut st = state.lock();
            if st.fail_all {
                return Err(eio());
            }
            if st.fail_next > 0 {
                st.fail_next -= 1;
                return Err(eio());
            }
            Ok(())
        })));
    }

    fn fail_next(&self) {
        self.0.lock().fail_next += 1;
    }

    fn fail_all(&self, on: bool) {
        self.0.lock().fail_all = on;
    }

    /// The next write waits until the returned sender says whether it fails (`true`) or not.
    fn gate(&self) -> mpsc::Sender<bool> {
        let (tx, rx) = mpsc::channel();
        self.0.lock().gate = Some(rx);
        tx
    }

    fn heal(&self) {
        *self.0.lock() = FaultState::default();
    }
}

/// A copy of the journal directory taken when a batch reached the store, and the batch's games.
type CrashCopy = (PathBuf, Vec<GameId>);

/// A host on a real journal (shard 0) with fault injection, whose store copies the journal
/// directory when a batch reaches it: what a crash right after the database commit would find.
struct CommitRig {
    h: Rig,
    faults: Faults,
    copies: Arc<Mutex<Vec<CrashCopy>>>,
    scratch: Arc<TempDir>,
}

impl CommitRig {
    async fn new(logger: Option<Logger>) -> CommitRig {
        let h = Rig::with(Opts { shard: 0, logger, ..Opts::default() }).await;
        let faults = Faults::default();
        faults.install(h.journal());
        let scratch = Arc::new(TempDir::new("crash"));
        let copies = Arc::new(Mutex::new(Vec::new()));
        let root = h.dir.clone().expect("a journal directory");
        let (to, list) = (scratch.clone(), copies.clone());
        h.store.set_hook(Some(Box::new(move |records: &[GameRecord]| {
            let mut list = list.lock();
            let dir = to.path().join(format!("crash-{}", list.len()));
            copy_dir(&root, &dir);
            list.push((dir, records.iter().map(|r| r.id).collect()));
        })));
        CommitRig { h, faults, copies, scratch }
    }

    fn new_game(&mut self, white: UserId) -> GameId {
        self.h.new_game(white, white + 1)
    }

    fn play(&mut self, id: GameId, plies: usize) {
        for _ in 0..plies {
            self.h.play(id, 500);
        }
    }

    fn resign(&mut self, id: GameId) {
        self.h.resign_white(id);
    }

    async fn flush(&self) {
        self.h.journal().flush().await.expect("journal written");
    }

    /// At `max(100, backoff)` later, the commit due then and its result.
    async fn retry(&mut self) -> Option<bool> {
        let t = self.h.t() + self.h.shard.backoff_ms().max(100);
        self.h.poll(t).await
    }

    fn committed(&self) -> Vec<GameId> {
        self.h.store.committed_ids()
    }

    fn copies(&self) -> Vec<CrashCopy> {
        self.copies.lock().clone()
    }

    /// The games a restart from a copy of the journal brings back as running.
    async fn running(&self, dir: &Path) -> Vec<GameId> {
        let clock = ManualClock::new(T0 as f64, T0);
        let journal = Journal::open(journal_options(dir, 0, &clock)).await.expect("journal copy");
        let settings = self.h.settings();
        let running = journal
            .recover()
            .iter()
            .filter(|(_, records)| {
                GameRoom::from_journal(
                    records.as_slice(),
                    settings,
                    FakeRules::boxed(Script::default()),
                    false,
                )
                .is_ok_and(|room| !room.is_over())
            })
            .map(|(id, _)| *id)
            .collect();
        journal.close().await.expect("journal copy closed");
        running
    }

    /// The games a restart from a copy of the journal taken now would take back.
    async fn recovered_ids(&self, tag: &str) -> Vec<GameId> {
        let to = self.scratch.path().join(format!("copy-{tag}"));
        copy_dir(self.h.dir.as_deref().expect("a journal directory"), &to);
        let clock = ManualClock::new(T0 as f64, T0);
        let journal = Journal::open(journal_options(&to, 0, &clock)).await.expect("journal copy");
        let ids = journal.recover().keys().copied().collect();
        journal.close().await.expect("journal copy closed");
        ids
    }

    /// A crash right after each database commit: no committed game comes back running.
    async fn check_crash_copies(&self) {
        let mut in_db = HashSet::new();
        for (dir, ids) in self.copies() {
            in_db.extend(ids.iter().copied());
            let back: Vec<GameId> =
                self.running(&dir).await.into_iter().filter(|id| in_db.contains(id)).collect();
            assert!(back.is_empty(), "a crash after committing {ids:?} brings back {back:?}");
        }
    }

    /// Closes the journal, even when a failed assertion left a fault behind.
    async fn close(self) {
        self.faults.heal();
        if let Some(journal) = self.h.shard.journal() {
            let _ = tokio::time::timeout(Duration::from_secs(2), journal.close()).await;
        }
    }
}

#[tokio::test]
async fn the_database_never_has_a_finished_game_before_the_journal_has_its_ended_record() {
    let mut r = CommitRig::new(None).await;
    let (a, b) = (r.new_game(1), r.new_game(3));
    r.play(a, 6);
    r.play(b, 6);
    r.flush().await;
    // A ends: its ended record is still in the journal's buffer when the commit is due.
    r.resign(a);
    assert!(r.h.journal().has_unwritten());
    r.h.advance(100);
    let t = r.h.t();
    assert!(r.h.shard.poll_commits(t));
    assert!(r.h.store.batches().is_empty(), "the commit waits for the journal");
    assert!(r.h.shard.settle_commit().await);
    assert_eq!(r.committed(), [a]);

    // B ends while the batch holding its ended record is being written.
    let release = r.faults.gate();
    r.resign(b);
    let flushing = r.h.journal().flush();
    let stats = r.h.journal().stats();
    assert_eq!((stats.pending_bytes, r.h.journal().has_unwritten()), (0, true), "the write is in flight");
    r.h.advance(100);
    let t = r.h.t();
    assert!(r.h.shard.poll_commits(t));
    let waited = tokio::time::timeout(Duration::from_millis(50), r.h.shard.next_task()).await;
    assert!(waited.is_err(), "the commit waits for the write in flight");
    assert_eq!(r.h.store.batches().len(), 1);
    release.send(false).expect("the write waits");
    flushing.await.expect("written");
    assert!(r.h.shard.settle_commit().await);
    assert_eq!(r.committed(), [a, b]);
    r.check_crash_copies().await;

    // With nothing left to write, the commit does not wait.
    let c = r.new_game(5);
    r.play(c, 2);
    r.resign(c);
    r.flush().await;
    r.h.advance(100);
    let t = r.h.t();
    assert!(r.h.shard.poll_commits(t));
    assert_eq!(r.committed(), [a, b, c], "handed to the store at once");
    assert!(r.h.shard.settle_commit().await);
    r.close().await;
}

#[tokio::test]
async fn a_journal_write_that_fails_before_the_commit_the_games_are_journaled_again_and_committed_once_written()
 {
    let mut r = CommitRig::new(None).await;
    // 1. The flush the commit waits for fails.
    let a = r.new_game(1);
    r.play(a, 6);
    r.flush().await;
    r.resign(a);
    r.faults.fail_next();
    r.h.advance(100);
    let t = r.h.t();
    assert_eq!(r.h.poll(t).await, Some(false));
    assert!(r.h.store.batches().is_empty(), "not committed");
    assert!(r.h.journal_kinds(a).contains(&RecordKind::Snapshot), "the game is journaled again (a snapshot)");
    assert_eq!(r.h.poll(t + 50).await, None, "backing off");
    assert_eq!(r.retry().await, Some(true));
    assert_eq!(r.committed(), [a]);
    let copies = r.copies();
    assert!(r.running(&copies[0].0).await.is_empty(), "the crash copy has the game over (its snapshot)");

    // 2. The write in flight fails, the next one (which the commit waits for) succeeds.
    let (b, c) = (r.new_game(3), r.new_game(5));
    r.play(b, 6);
    r.play(c, 2);
    r.flush().await;
    r.resign(b);
    let fail = r.faults.gate();
    let lost = r.h.journal().flush();
    r.play(c, 1); // the next batch
    r.h.advance(100);
    let t = r.h.t();
    assert!(r.h.shard.poll_commits(t));
    fail.send(true).expect("the write waits");
    assert!(lost.await.is_err());
    assert!(!r.h.shard.settle_commit().await, "a write failed meanwhile: not committed");
    assert_eq!(r.committed(), [a]);
    assert_eq!(r.retry().await, Some(true));
    assert_eq!(r.committed(), [a, b]);
    let last = r.copies().pop().expect("a crash copy").0;
    assert_eq!(r.running(&last).await, [c], "only the running game comes back running");

    // 3. The write holding the ended record failed before the commit was due, and nothing is left
    //    to write when it is: the game is journaled again first.
    let d = r.new_game(7);
    r.play(d, 6);
    r.flush().await;
    r.resign(d);
    r.faults.fail_next();
    assert!(r.h.journal().flush().await.is_err());
    assert!(!r.h.journal().has_unwritten());
    r.h.advance(100);
    let t = r.h.t();
    assert!(r.h.shard.poll_commits(t));
    assert_eq!(r.committed(), [a, b], "the commit waits for the snapshot");
    assert!(r.h.shard.settle_commit().await);
    assert_eq!(r.committed(), [a, b, d]);
    let last = r.copies().pop().expect("a crash copy").0;
    assert_eq!(r.running(&last).await, [c], "the crash copy has the game over");
    r.check_crash_copies().await;
    r.close().await;
}

#[tokio::test]
async fn a_failed_flush_journals_again_only_the_batch_the_other_games_get_their_snapshot_before_their_own_commit()
 {
    let mut r = CommitRig::new(None).await;
    r.h.shard.settings.commit_batch_max = 1;
    let ids = [r.new_game(1), r.new_game(3), r.new_game(5)];
    for id in ids {
        r.play(id, 6);
    }
    r.flush().await;
    for id in ids {
        r.resign(id); // three ended records in the buffer
    }
    let snapshots = |r: &CommitRig| -> Vec<GameId> {
        r.h.shard
            .journal_log
            .iter()
            .filter(|(_, rec)| rec.kind == RecordKind::Snapshot)
            .map(|(g, _)| *g)
            .collect()
    };
    // The write holding the three ended records fails.
    r.faults.fail_next();
    r.h.advance(100);
    let t = r.h.t();
    assert_eq!(r.h.poll(t).await, Some(false));
    assert_eq!(snapshots(&r), [ids[0]], "only the batch is journaled again at once");
    for _ in ids {
        assert_eq!(r.retry().await, Some(true));
    }
    assert_eq!(r.committed(), ids);
    assert_eq!(snapshots(&r), ids, "each game is journaled again right before its own commit");
    r.check_crash_copies().await;
    r.close().await;
}

/// The log records of a component, at a level, whose message contains `text`.
fn count(cap: &Capture, component: &str, level: &str, text: &str) -> usize {
    cap.records_of(component)
        .iter()
        .filter(|r| r["level"] == level && r["msg"].as_str().is_some_and(|m| m.contains(text)))
        .count()
}

#[tokio::test]
async fn a_journal_whose_writes_keep_failing_stops_holding_the_commits_until_a_flush_writes_again() {
    const LOG: &str = "game-test-unjournaled";
    const UNJOURNALED: &str = "without waiting for the journal";
    let cap = capture_logs(Level::Info);
    let mut r = CommitRig::new(Some(Logger::root().child(LOG))).await;
    let a = r.new_game(1);
    let (ew, eb) = (Ep::new(1, 1), Ep::new(2, 2));
    r.h.shard.attach(a, 1, ew.endpoint());
    r.h.shard.attach(a, 2, eb.endpoint());
    r.play(a, 6);
    r.flush().await;
    // From now on every journal write fails (a full disk, a read-only volume...).
    r.faults.fail_all(true);
    r.resign(a);
    for i in 1..JOURNAL_GATE_TRIES {
        assert_eq!(r.retry().await, Some(false), "attempt {i} waits for the journal");
    }
    assert!(r.committed().is_empty() && r.h.events.ended().is_empty());
    assert_eq!(r.retry().await, Some(true), "the last attempt commits without the journal");
    assert_eq!(r.committed(), [a]);
    assert_eq!(r.h.events.ended().iter().map(|e| (e.game, e.rated)).collect::<Vec<_>>(), [(a, true)]);
    for ep in [&ew, &eb] {
        let update = ep.msgs().into_iter().any(|m| matches!(m, ServerMsg::RatingUpdate(u) if u.game == a));
        assert!(update, "a RatingUpdate for each player");
    }
    assert_eq!(r.h.shard.counters().unjournaled, 1);

    // While it lasts, a commit does not wait for the journal; one error for the episode.
    let b = r.new_game(3);
    r.play(b, 2);
    r.resign(b);
    r.h.advance(100);
    let t = r.h.t();
    assert!(r.h.shard.poll_commits(t));
    assert_eq!(r.committed(), [a, b], "handed to the store at once");
    assert!(r.h.shard.settle_commit().await);
    assert_eq!(r.h.shard.counters().unjournaled, 2);
    assert_eq!(count(&cap, LOG, "error", UNJOURNALED), 1);
    while r.h.shard.probe_in_flight() {
        r.h.shard.next_task().await; // its flush failed too
    }
    assert!(r.h.shard.unjournaled());

    // The journal writes again: the flush started by the next commit ends the episode, and later
    // commits wait for the journal again.
    r.faults.fail_all(false);
    let c = r.new_game(5);
    r.play(c, 2);
    r.resign(c);
    r.h.advance(100);
    let t = r.h.t();
    assert!(r.h.shard.poll_commits(t));
    assert!(r.h.shard.settle_commit().await);
    while r.h.shard.probe_in_flight() {
        r.h.shard.next_task().await;
    }
    assert!(!r.h.shard.unjournaled());
    assert_eq!(count(&cap, LOG, "info", "journal writes succeed again"), 1);
    let d = r.new_game(7);
    r.play(d, 2);
    r.resign(d);
    r.h.advance(100);
    let t = r.h.t();
    assert!(r.h.shard.poll_commits(t));
    assert_eq!(r.committed(), [a, b, c], "the commit waits for the journal again");
    assert!(r.h.shard.settle_commit().await);
    assert_eq!(r.committed(), [a, b, c, d]);
    assert_eq!(r.h.shard.counters().unjournaled, 3);
    assert_eq!(count(&cap, LOG, "error", UNJOURNALED), 1);
    // Their `committed` records reached the journal: a restart takes none of them back.
    r.flush().await;
    assert!(r.recovered_ids("end").await.is_empty());

    // A new episode is logged again.
    let e = r.new_game(9);
    r.play(e, 2);
    r.resign(e);
    r.faults.fail_all(true);
    for _ in 0..JOURNAL_GATE_TRIES {
        r.retry().await;
    }
    assert_eq!(r.committed(), [a, b, c, d, e]);
    assert_eq!(count(&cap, LOG, "error", UNJOURNALED), 2);
    r.close().await;
}

#[tokio::test]
async fn shutdown_with_a_failing_journal_commits_the_finished_games_without_it_and_logs_the_failed_final_flush()
 {
    const LOG: &str = "game-test-shutdown";
    for in_flight in [false, true] {
        let cap = capture_logs(Level::Info);
        let mut r = CommitRig::new(Some(Logger::root().child(LOG))).await;
        let (a, b) = (r.new_game(1), r.new_game(3));
        let ew = Ep::new(1, 1);
        r.h.shard.attach(a, 1, ew.endpoint());
        r.play(a, 6);
        r.play(b, 2);
        r.flush().await;
        r.faults.fail_all(true);
        r.resign(a);
        if in_flight {
            r.h.advance(100);
            let t = r.h.t();
            assert!(r.h.shard.poll_commits(t));
        }
        r.h.shard.shutdown().await;
        assert_eq!(r.committed(), [a], "in flight: {in_flight}");
        assert_eq!(r.h.events.ended().iter().map(|e| e.game).collect::<Vec<_>>(), [a]);
        assert!(ew.msgs().iter().any(|m| matches!(m, ServerMsg::RatingUpdate(_))));
        assert_eq!(r.h.shard.counters().unjournaled, 1);
        assert_eq!(
            count(&cap, LOG, "error", "journal flush failed at shutdown"),
            1,
            "in flight: {in_flight}"
        );
        assert_eq!(r.h.shard.pending_commits(), 0);
        assert!(!r.h.room(b).is_over(), "a running game is left to the journal");
        assert_eq!(result(r.h.room(a)).1, ER::Resignation);
        r.close().await;
        drop(cap);
    }
}
