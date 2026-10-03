//! Port of store.analysis-queue.test.js: the engine analysis backlog policy (DESIGN 6.5). Ordinary
//! games are sampled and capped by `ANALYSIS_QUEUE_MAX`; games with a suspicion signal, a report or
//! a moderator request are always queued and taken first; at most 20 flagged games of a player
//! wait. The configuration checks of the Node suite belong to the config module, the report
//! handling ones to the anti-cheat (its store calls, `analysis.request`, are tested here).
//!
//! The tests of this module run one at a time: they read deltas of the global skip metric.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::support::*;
use crate::config::Config;
use crate::ids::{GameId, UserId};
use crate::store::{
    Backlog, ErrorKind, GameRecord, IntegrityLevel, IntegrityUpdate, JobStatus, NewAnomaly, NewReport,
    Priority, QueueStats, RandomFn, ReportCategory, ReportStatus, Severity, SkipReason, Store, StoreOptions,
    status,
};

const DAY: i64 = 86_400_000;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn now() -> i64 {
    crate::clock::wall_ms()
}

fn skipped_metric(reason: &str) -> u64 {
    crate::store::metrics::ANALYSIS_SKIPPED.with(&[reason]).get()
}

struct Fixture {
    store: Store,
    ids: [UserId; 6],
    next_id: GameId,
}

impl Fixture {
    async fn new(edit: impl FnOnce(&mut Config), random: Option<RandomFn>) -> Fixture {
        let mut cfg = config();
        cfg.analysis_min_plies = 20;
        edit(&mut cfg);
        let store = store_with(&cfg, StoreOptions { random, ..options(None) }).await;
        let mut ids = [0; 6];
        for (i, name) in ["Ann", "Ben", "Cid", "Dee", "Eve", "Fay"].iter().enumerate() {
            let user = crate::store::NewUser {
                created_at: now() - 400 * DAY,
                ..new_user(name, Some(&format!("{name}@example.org")))
            };
            ids[i] = store.users().create(user).await.unwrap();
        }
        Fixture { store, ids, next_id: 5_000_000_000_000 }
    }

    /// A rated 5+0 game of 30 plies that ended now, white wins.
    fn game(&mut self, white: UserId, black: UserId) -> GameRecord {
        self.with(white, black, |_| {})
    }

    fn with(&mut self, white: UserId, black: UserId, edit: impl FnOnce(&mut GameRecord)) -> GameRecord {
        self.next_id += 1;
        let t = now();
        let mut r = GameRecord {
            id: self.next_id,
            category: "5+0".into(),
            rated: true,
            base_ms: 300_000,
            inc_ms: 0,
            white_id: white,
            black_id: black,
            white_name: "W".into(),
            black_name: "B".into(),
            white_rating: None,
            black_rating: None,
            started_at: Some(t - 600_000),
            ended_at: Some(t),
            status: status::WHITE_WINS,
            reason: 4,
            rematch_of: None,
            flags: 0,
            moves: vec![0; 30],
            spent_ms: Some(vec![0; 30]),
            clock_ms: Some(vec![0; 30]),
        };
        edit(&mut r);
        r
    }

    fn games(&mut self, n: usize, mut pick: impl FnMut(usize) -> (UserId, UserId)) -> Vec<GameRecord> {
        (0..n)
            .map(|i| {
                let (w, b) = pick(i);
                self.game(w, b)
            })
            .collect()
    }

    /// The skip reasons of a committed batch, in order.
    async fn commit(&self, games: &[GameRecord]) -> Vec<Option<SkipReason>> {
        let res = self.store.finish_batch(games.to_vec()).await.unwrap();
        assert!(
            res.iter().zip(games).all(|(r, g)| r.ratings.is_some()
                == (g.rated && g.status != status::ABORTED && g.category != "custom")),
            "skipped games are rated and stored all the same"
        );
        res.into_iter().map(|r| r.analysis_skipped).collect()
    }

    async fn flag(&self, user: UserId, level: IntegrityLevel) {
        let f = IntegrityUpdate { level: Some(level), score: Some(3.0), ..IntegrityUpdate::default() };
        self.store.integrity().set(user, f).await.unwrap();
    }

    async fn claim_all(&self) -> Vec<(GameId, Priority)> {
        let jobs = self.store.analysis().next(100, Some("w".into()), now()).await.unwrap();
        jobs.into_iter().map(|j| (j.game_id, j.priority)).collect()
    }

    async fn backlog(&self) -> Backlog {
        self.store.analysis().backlog().await.unwrap()
    }

    /// The waiting jobs of a player's games, oldest game first.
    async fn signal_jobs_of(&self, user: UserId) -> Vec<GameId> {
        let jobs = self.store.analysis().for_user(user, 1000, false).await.unwrap();
        let mut ids: Vec<GameId> =
            jobs.into_iter().filter(|j| j.status == JobStatus::Queued).map(|j| j.game_id).collect();
        ids.sort_unstable();
        ids
    }

    async fn anomaly(&self, user: UserId, game: GameId, severity: Severity) {
        let a = NewAnomaly {
            user_id: Some(user),
            game_id: Some(game),
            kind: "clock_implausible".into(),
            severity,
            at: Some(now() - 1000),
            detail: None,
        };
        self.store.anomalies().insert_batch(vec![a]).await.unwrap();
    }
}

fn ids(games: &[GameRecord]) -> Vec<GameId> {
    games.iter().map(|g| g.id).collect()
}

const BACKLOG: Option<SkipReason> = Some(SkipReason::Backlog);
const PLAYER: Option<SkipReason> = Some(SkipReason::Player);
const SAMPLE: Option<SkipReason> = Some(SkipReason::Sample);

#[tokio::test]
async fn analysis_queue_max_caps_the_ordinary_games_waiting_and_a_claimed_job_makes_room() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|c| c.analysis_queue_max = 3, None).await;
    let [a, b, ..] = f.ids;
    let before = skipped_metric("backlog");
    let games = f.games(5, |_| (a, b));
    assert_eq!(f.commit(&games).await, vec![None, None, None, BACKLOG, BACKLOG]);
    assert_eq!(skipped_metric("backlog") - before, 2);
    assert_eq!(f.backlog().await, Backlog { ordinary: 3, priority: 0 });
    // The next batch (its own transaction) counts again: still full.
    let g = f.game(a, b);
    assert_eq!(f.commit(&[g]).await, vec![BACKLOG]);
    // A job taken by the engine leaves the queue: one more game fits, then it is full again.
    let claimed = f.store.analysis().next(1, Some("w".into()), now()).await.unwrap();
    assert_eq!(claimed[0].game_id, games[0].id);
    let more = [f.game(a, b), f.game(a, b)];
    assert_eq!(f.commit(&more).await, vec![None, BACKLOG]);
    let stats = f.store.analysis().stats().await.unwrap();
    assert_eq!(stats, QueueStats { queued: 3, running: 1, done: 0, failed: 0 });
    // Games the automatic analysis never takes are not "skipped".
    let casual = [f.with(a, b, |g| g.rated = false), f.with(a, b, |g| g.moves = vec![0; 19])];
    assert_eq!(f.commit(&casual).await, vec![None, None]);
    f.store.close().await;
}

#[tokio::test]
async fn analysis_queue_max_zero_queues_only_prioritized_games() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|c| c.analysis_queue_max = 0, None).await;
    let [a, b, c, ..] = f.ids;
    f.flag(c, IntegrityLevel::Suspected).await;
    let games = [f.game(a, b), f.game(c, a)];
    assert_eq!(f.commit(&games).await, vec![BACKLOG, None]);
    assert_eq!(f.backlog().await, Backlog { ordinary: 0, priority: 1 });
    f.store.close().await;
}

#[tokio::test]
async fn priority_moderator_request_then_reports_then_suspicion_signals_then_ordinary_games_oldest_first() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|c| c.analysis_queue_max = 2, None).await;
    let [a, b, c, ..] = f.ids;
    let (o1, o2, skipped, analysed) = (f.game(a, b), f.game(b, a), f.game(a, b), f.game(b, a));
    f.commit(std::slice::from_ref(&analysed)).await;
    let q = f.store.analysis();
    let job = q.next(1, Some("w".into()), now()).await.unwrap().remove(0);
    q.complete(job.game_id, Some(serde_json::json!({"gameId": analysed.id})), now()).await.unwrap();
    assert_eq!(f.commit(&[o1.clone(), o2.clone(), skipped.clone()]).await[2], BACKLOG);
    // A suspected player: their new games are queued whatever the backlog, ahead of the ordinary ones.
    f.flag(c, IntegrityLevel::Suspected).await;
    let flagged = f.game(c, a);
    assert_eq!(f.commit(std::slice::from_ref(&flagged)).await, vec![None]);
    // A report on the skipped game queues it ahead of the flagged one; a moderator request on the
    // analysed game re-analyses it before everything else.
    assert!(q.request(skipped.id, Priority::Report, now()).await.unwrap());
    q.enqueue(analysed.id, now()).await.unwrap();
    assert_eq!(f.backlog().await, Backlog { ordinary: 2, priority: 3 });
    assert_eq!(
        f.claim_all().await,
        vec![
            (analysed.id, Priority::Manual),
            (skipped.id, Priority::Report),
            (flagged.id, Priority::Signal),
            (o1.id, Priority::Ordinary),
            (o2.id, Priority::Ordinary),
        ]
    );
    f.store.close().await;
}

#[tokio::test]
async fn the_claim_order_holds_across_several_claims_and_the_job_carries_its_priority() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|_| {}, None).await;
    let [a, b, c, ..] = f.ids;
    let games = f.games(3, |_| (a, b));
    f.commit(&games).await;
    f.flag(c, IntegrityLevel::HighConfidence).await;
    let late = f.game(b, c);
    f.commit(std::slice::from_ref(&late)).await;
    let q = f.store.analysis();
    let t = now();
    let first = q.next(1, Some("w".into()), t).await.unwrap();
    assert_eq!(
        first.iter().map(|j| (j.game_id, j.priority)).collect::<Vec<_>>(),
        vec![(late.id, Priority::Signal)]
    );
    let two = q.next(2, Some("w".into()), t).await.unwrap();
    assert_eq!(two.iter().map(|j| j.game_id).collect::<Vec<_>>(), vec![games[0].id, games[1].id]);
    f.store.close().await;
}

#[tokio::test]
async fn suspicion_signals_integrity_level_open_reports_and_anomalies_of_the_game() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|c| c.analysis_queue_max = 0, None).await;
    let [a, b, c, d, e, fay] = f.ids;
    let t = now();
    // An earlier game between the reporter and the reported player.
    let earlier = f.game(a, b);
    f.commit(std::slice::from_ref(&earlier)).await;
    let report = |from, to, category, weight, at| NewReport {
        reporter_id: from,
        reported_id: to,
        game_id: Some(earlier.id),
        category,
        comment: None,
        weight,
        at,
    };
    let reports = f.store.reports();
    // An open cheating report with weight, within 30 days: the reported player's next games are flagged.
    reports.create(report(a, b, ReportCategory::Cheating, 0.8, t - 2 * DAY)).await.unwrap();
    let g = f.game(b, c);
    assert_eq!(f.commit(&[g]).await, vec![None]);
    // Not a signal: an abuse report, reports of low credibility, a dismissed one, an old one.
    reports.create(report(a, c, ReportCategory::Abuse, 1.0, t - DAY)).await.unwrap();
    reports.create(report(b, c, ReportCategory::Cheating, 0.0, t - DAY)).await.unwrap();
    reports.create(report(fay, c, ReportCategory::Cheating, 0.49, t - DAY)).await.unwrap();
    let dismissed = reports.create(report(d, c, ReportCategory::Other, 1.0, t - DAY)).await.unwrap();
    reports.resolve(dismissed, ReportStatus::Dismissed, Some("mod".into()), t).await.unwrap();
    reports.create(report(e, c, ReportCategory::Cheating, 1.0, t - 31 * DAY)).await.unwrap();
    let g = f.game(c, d);
    assert_eq!(f.commit(&[g]).await, vec![BACKLOG]);
    // A suspicious anomaly recorded in this game flags it; an info one or one of another game does not.
    let g1 = f.game(d, e);
    f.anomaly(d, g1.id, Severity::Info).await;
    f.anomaly(e, 12_345, Severity::Suspicious).await;
    assert_eq!(f.commit(&[g1]).await, vec![BACKLOG]);
    let g2 = f.game(e, fay);
    f.anomaly(fay, g2.id, Severity::Suspicious).await;
    assert_eq!(f.commit(&[g2]).await, vec![None]);
    // An anomaly older than the game's start is not of this game.
    let g3 = f.with(e, fay, |g| g.started_at = Some(t + 10_000));
    f.anomaly(fay, g3.id, Severity::Suspicious).await;
    assert_eq!(f.commit(&[g3]).await, vec![BACKLOG]);
    // Any integrity level above 'none' (here confirmed, after a certain cheat and the ban).
    f.flag(a, IntegrityLevel::Confirmed).await;
    let g = f.game(d, a);
    assert_eq!(f.commit(&[g]).await, vec![None]);
    f.store.close().await;
}

#[tokio::test]
async fn analysis_sample_rate_draws_ordinary_games_only_and_flagged_games_are_never_sampled_out() {
    let _serial = SERIAL.lock().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let draws = [0.1, 0.7, 0.49, 0.5];
    let n = calls.clone();
    let random: RandomFn = Arc::new(move || draws[n.fetch_add(1, Ordering::SeqCst) % draws.len()]);
    let mut f = Fixture::new(|c| c.analysis_sample_rate = 0.5, Some(random)).await;
    let [a, b, c, ..] = f.ids;
    let before = skipped_metric("sample");
    let games = f.games(4, |_| (a, b));
    assert_eq!(f.commit(&games).await, vec![None, SAMPLE, None, SAMPLE]);
    assert_eq!(skipped_metric("sample") - before, 2);
    f.flag(c, IntegrityLevel::Suspected).await;
    let g = f.game(c, a);
    assert_eq!(f.commit(&[g]).await, vec![None]);
    assert_eq!(calls.load(Ordering::SeqCst), 4, "no draw for a flagged game");
    f.store.close().await;

    let mut none = Fixture::new(|c| c.analysis_sample_rate = 0.0, Some(Arc::new(|| 0.0))).await;
    let g = none.game(none.ids[0], none.ids[1]);
    assert_eq!(none.commit(&[g]).await, vec![SAMPLE]);
    none.store.close().await;

    let used = Arc::new(AtomicUsize::new(0));
    let u = used.clone();
    let mut all = Fixture::new(
        |_| {},
        Some(Arc::new(move || {
            u.fetch_add(1, Ordering::SeqCst);
            0.99
        })),
    )
    .await;
    let g = all.game(all.ids[0], all.ids[1]);
    assert_eq!(all.commit(&[g]).await, vec![None]);
    assert_eq!(used.load(Ordering::SeqCst), 0, "the default rate 1 draws nothing");
    all.store.close().await;
}

#[tokio::test]
async fn request_eligible_games_only_raises_a_waiting_job_requeues_a_failed_one_leaves_running_and_done_jobs()
{
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|c| c.analysis_queue_max = 1, None).await;
    let [a, b, ..] = f.ids;
    let five = f.games(5, |_| (a, b));
    let (waiting, skipped, running, done, failed) =
        (five[0].id, five[1].id, five[2].id, five[3].id, five[4].id);
    let casual = f.with(a, b, |g| g.rated = false);
    let custom = f.with(a, b, |g| g.category = "custom".into());
    let aborted = f.with(a, b, |g| (g.status, g.reason) = (status::ABORTED, 9));
    let short = f.with(a, b, |g| g.moves = vec![0; 19]);
    let mut all = five.clone();
    all.extend([casual.clone(), custom.clone(), aborted.clone(), short.clone()]);
    assert_eq!(f.commit(&all).await, vec![None, BACKLOG, BACKLOG, BACKLOG, BACKLOG, None, None, None, None]);
    let q = f.store.analysis();
    let t = now();
    for g in [&casual, &custom, &aborted, &short] {
        assert!(!q.request(g.id, Priority::Report, t).await.unwrap(), "game {}", g.id);
    }
    assert!(
        !q.request(424_242, Priority::Report, t).await.unwrap(),
        "unknown game: no job, no foreign key error"
    );
    assert_eq!(f.backlog().await.priority, 0);
    assert!(q.request(skipped, Priority::Report, t).await.unwrap());
    assert!(
        !q.request(skipped, Priority::Signal, t).await.unwrap(),
        "already waiting with a higher priority"
    );
    assert!(q.request(waiting, Priority::Signal, t).await.unwrap(), "priority raised");
    assert_eq!(f.backlog().await, Backlog { ordinary: 0, priority: 2 });

    // Running, done and failed jobs.
    for id in [running, done, failed] {
        q.enqueue(id, t).await.unwrap();
    }
    let claimed: Vec<GameId> =
        q.next(3, Some("w".into()), t).await.unwrap().iter().map(|j| j.game_id).collect();
    assert!(claimed.contains(&running) && claimed.contains(&done) && claimed.contains(&failed));
    q.complete(done, Some(serde_json::json!({"gameId": done})), t).await.unwrap();
    for i in 0..3 {
        let expected = if i < 2 { JobStatus::Queued } else { JobStatus::Failed };
        assert_eq!(q.fail(failed, Some("engine crashed".into()), t).await.unwrap(), Some(expected));
        if i < 2 {
            assert_eq!(q.next(1, Some("w".into()), t).await.unwrap()[0].game_id, failed);
        }
    }
    assert!(!q.request(running, Priority::Report, t).await.unwrap());
    assert!(!q.request(done, Priority::Report, t).await.unwrap());
    assert!(q.request(failed, Priority::Report, t).await.unwrap());
    let again = q.next(1, Some("w".into()), t).await.unwrap().remove(0);
    assert_eq!((again.game_id, again.attempts), (failed, 1), "a failed job gets its attempts back");
    assert_eq!(Priority::parse("urgent"), None);
    let e = q.request(waiting, Priority::Ordinary, t).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Invalid, "would bypass ANALYSIS_QUEUE_MAX");
    f.store.close().await;
}

#[tokio::test]
async fn low_credibility_requests_wait_in_the_signal_tier_behind_a_statistically_suspected_player() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|c| c.analysis_queue_max = 0, None).await;
    let [a, b, c, d, ..] = f.ids;
    // Two brand-new accounts play each other and report every game: the anti-cheat asks for their
    // analysis at 'signal' priority only.
    let theirs = f.games(5, |_| (c, d));
    f.commit(&theirs).await;
    f.flag(a, IntegrityLevel::Suspected).await;
    let suspect = f.game(a, b);
    f.commit(std::slice::from_ref(&suspect)).await;
    let later = now() + 60_000;
    for g in &theirs {
        assert!(f.store.analysis().request(g.id, Priority::Signal, later).await.unwrap());
    }
    let claimed = f.claim_all().await;
    assert_eq!(claimed.len(), 6);
    assert_eq!(
        claimed[0],
        (suspect.id, Priority::Signal),
        "the suspect's game first (queued first, same tier)"
    );
    assert!(claimed.iter().all(|(_, p)| *p == Priority::Signal));
    f.store.close().await;
}

#[tokio::test]
async fn every_fourth_claim_takes_the_oldest_ordinary_game_first() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|_| {}, None).await;
    let [a, b, c, d, ..] = f.ids;
    let ordinary = vec![f.game(a, b), f.game(b, a), f.game(a, b)];
    f.commit(&ordinary).await;
    f.flag(c, IntegrityLevel::Suspected).await;
    f.flag(d, IntegrityLevel::HighConfidence).await;
    let flagged = f.games(10, |i| if i % 2 == 1 { (c, a) } else { (d, b) });
    f.commit(&flagged).await;
    let mut claimed = Vec::new();
    for _ in 0..14 {
        claimed.push(f.store.analysis().next(1, Some("w".into()), now()).await.unwrap().pop());
    }
    let priorities: Vec<Option<i64>> =
        claimed.iter().map(|j| j.as_ref().map(|j| j.priority as i64)).collect();
    let s = Some(1);
    let o = Some(0);
    assert_eq!(priorities, vec![s, s, s, o, s, s, s, o, s, s, s, o, s, None]);
    let ordinary_claims: Vec<GameId> =
        claimed.iter().flatten().filter(|j| j.priority == Priority::Ordinary).map(|j| j.game_id).collect();
    assert_eq!(ordinary_claims, ids(&ordinary), "oldest first");
    f.store.close().await;

    // Without ordinary jobs the reserved claim takes the next prioritized job; several claims at
    // once get their share too.
    let mut two = Fixture::new(|_| {}, None).await;
    let [x, y, z, ..] = two.ids;
    two.flag(z, IntegrityLevel::Suspected).await;
    let zs = two.games(5, |_| (z, x));
    two.commit(&zs).await;
    for _ in 0..5 {
        assert_eq!(two.store.analysis().next(1, Some("w".into()), now()).await.unwrap().len(), 1);
    }
    let mut more = two.games(8, |_| (z, y));
    more.extend([two.game(x, y), two.game(y, x)]);
    two.commit(&more).await;
    let batch = two.store.analysis().next(8, Some("w".into()), now()).await.unwrap();
    assert_eq!(batch.len(), 8);
    let n = batch.iter().filter(|j| j.priority == Priority::Ordinary).count();
    assert_eq!(n, 2, "claims 6 to 13: two reserved turns (the 8th and the 12th)");
    two.store.close().await;
}

#[tokio::test]
async fn at_most_twenty_flagged_games_of_one_player_wait_and_more_are_skipped_until_the_engine_takes_some() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|_| {}, None).await;
    let [a, b, c, d, e, _] = f.ids;
    f.flag(c, IntegrityLevel::Suspected).await;
    let before = skipped_metric("player");
    let games = f.games(23, |i| if i % 2 == 1 { (c, a) } else { (b, c) });
    let mut expected = vec![None; 20];
    expected.extend([PLAYER; 3]);
    assert_eq!(f.commit(&games).await, expected);
    assert_eq!(skipped_metric("player") - before, 3);
    assert_eq!(f.backlog().await, Backlog { ordinary: 0, priority: 20 });
    // Never demoted to the ordinary sample (which feeds the population statistics).
    assert_eq!(f.store.analysis().stats().await.unwrap().queued, 20);
    // The cap is per player: another flagged player's games and ordinary games are still queued.
    f.flag(e, IntegrityLevel::Suspected).await;
    let g = f.game(e, d);
    assert_eq!(f.commit(&[g]).await, vec![None]);
    let g = f.game(a, d);
    assert_eq!(f.commit(&[g]).await, vec![None]);
    // A game of c taken by the engine makes room for one more.
    let job = f.store.analysis().next(1, Some("w".into()), now()).await.unwrap().remove(0);
    assert_eq!(job.game_id, games[0].id);
    let g = f.game(c, d);
    assert_eq!(f.commit(&[g]).await, vec![None]);
    let g = f.game(c, d);
    assert_eq!(f.commit(&[g]).await, vec![PLAYER]);
    // A low-credibility request ('signal') does not pass the cap; a credible one ('report') does.
    assert!(!f.store.analysis().request(games[21].id, Priority::Signal, now()).await.unwrap());
    assert!(f.store.analysis().request(games[21].id, Priority::Report, now()).await.unwrap());
    f.store.close().await;
}

#[tokio::test]
async fn signal_cap_a_game_with_an_anomaly_of_its_own_takes_over_the_oldest_waiting_job_without_one() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|_| {}, None).await;
    let [a, b, c, d, ..] = f.ids;
    f.flag(c, IntegrityLevel::Suspected).await;
    // c fills the cap with 20 quick games flagged only by its integrity level.
    let junk = f.games(20, |i| if i % 2 == 1 { (c, a) } else { (b, c) });
    f.commit(&junk).await;
    assert_eq!(f.signal_jobs_of(c).await, ids(&junk));
    // A plain flagged game is still skipped.
    let (player0, displaced0) = (skipped_metric("player"), skipped_metric("displaced"));
    let plain = f.game(c, d);
    assert_eq!(f.commit(&[plain]).await, vec![PLAYER]);
    // So is one with only an info anomaly, or an anomaly of another game.
    let info = f.game(c, d);
    f.anomaly(c, info.id, Severity::Info).await;
    f.anomaly(c, 777, Severity::Suspicious).await;
    assert_eq!(f.commit(&[info]).await, vec![PLAYER]);
    // The game with a suspicious anomaly of its own is queued, in place of the oldest junk job.
    let real = f.game(c, d);
    f.anomaly(c, real.id, Severity::Suspicious).await;
    let res = f.store.finish_batch(vec![real.clone()]).await.unwrap().remove(0);
    assert_eq!(res.analysis_skipped, None);
    assert_eq!(res.analysis_displaced, vec![junk[0].id]);
    assert!(res.ratings.is_some(), "rated as usual");
    let mut expected = ids(&junk[1..]);
    expected.push(real.id);
    assert_eq!(f.signal_jobs_of(c).await, expected, "still 20 waiting");
    assert_eq!(f.backlog().await, Backlog { ordinary: 0, priority: 20 });
    assert_eq!(skipped_metric("player") - player0, 2);
    assert_eq!(skipped_metric("displaced") - displaced0, 1);
    // A game whose anomaly is the opponent's counts too; the displaced job is always the oldest
    // one without an anomaly of its own, never one with.
    let by_opponent = f.game(d, c);
    f.anomaly(d, by_opponent.id, Severity::Suspicious).await;
    let res = f.store.finish_batch(vec![by_opponent]).await.unwrap().remove(0);
    assert_eq!(res.analysis_displaced, vec![junk[1].id]);
    f.store.close().await;
}

#[tokio::test]
async fn signal_cap_with_twenty_anomaly_games_waiting_a_21st_is_skipped_and_both_capped_players_give_a_job() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new(|_| {}, None).await;
    let [a, b, c, ..] = f.ids;
    f.flag(c, IntegrityLevel::Suspected).await;
    let with_anomaly = f.games(20, |_| (c, a));
    for g in &with_anomaly {
        f.anomaly(c, g.id, Severity::Suspicious).await;
    }
    f.commit(&with_anomaly).await;
    assert_eq!(f.signal_jobs_of(c).await.len(), 20);
    let next = f.game(c, b);
    f.anomaly(c, next.id, Severity::Suspicious).await;
    assert_eq!(f.commit(&[next]).await, vec![PLAYER], "every waiting job has evidence");
    assert_eq!(f.signal_jobs_of(c).await, ids(&with_anomaly));
    f.store.close().await;

    // 19 games with an anomaly, then a plain one: the plain one is replaced although it is the newest.
    let mut mix = Fixture::new(|_| {}, None).await;
    let [m, n, o, ..] = mix.ids;
    mix.flag(m, IntegrityLevel::Suspected).await;
    let evidence = mix.games(19, |_| (m, n));
    for g in &evidence {
        mix.anomaly(m, g.id, Severity::Suspicious).await;
    }
    let plain = mix.game(o, m);
    let mut batch = evidence.clone();
    batch.push(plain.clone());
    mix.commit(&batch).await;
    let last = mix.game(m, o);
    mix.anomaly(m, last.id, Severity::Suspicious).await;
    let res = mix.store.finish_batch(vec![last.clone()]).await.unwrap().remove(0);
    assert_eq!(res.analysis_displaced, vec![plain.id]);
    let mut expected = ids(&evidence);
    expected.push(last.id);
    assert_eq!(mix.signal_jobs_of(m).await, expected);
    mix.store.close().await;

    // Both players capped: one job of each is replaced (the oldest without an anomaly of its own).
    let mut two = Fixture::new(|_| {}, None).await;
    let [x, y, z, w, ..] = two.ids;
    two.flag(x, IntegrityLevel::Suspected).await;
    two.flag(y, IntegrityLevel::Suspected).await;
    let xs = two.games(20, |_| (x, z));
    let ys = two.games(20, |_| (w, y));
    two.commit(&[xs.clone(), ys.clone()].concat()).await;
    let both = two.game(x, y);
    two.anomaly(y, both.id, Severity::Suspicious).await;
    let res = two.store.finish_batch(vec![both.clone()]).await.unwrap().remove(0);
    assert_eq!(res.analysis_displaced, vec![xs[0].id, ys[0].id]);
    assert_eq!((two.signal_jobs_of(x).await.len(), two.signal_jobs_of(y).await.len()), (20, 20));
    // One capped player has no job to give: nothing is removed, the game is skipped.
    let ys2 = two.signal_jobs_of(y).await;
    let xs2 = two.signal_jobs_of(x).await;
    for &id in &ys2 {
        if id != both.id {
            two.anomaly(y, id, Severity::Suspicious).await;
        }
    }
    let third = two.game(x, y);
    two.anomaly(x, third.id, Severity::Suspicious).await;
    assert_eq!(two.commit(&[third]).await, vec![PLAYER]);
    assert_eq!((two.signal_jobs_of(x).await, two.signal_jobs_of(y).await), (xs2, ys2), "no job removed");
    two.store.close().await;

    // Both players capped by the same 20 games between them: one job replaced serves both.
    let mut pair = Fixture::new(|_| {}, None).await;
    let [p, q, ..] = pair.ids;
    pair.flag(p, IntegrityLevel::Suspected).await;
    let pq = pair.games(20, |i| if i % 2 == 1 { (p, q) } else { (q, p) });
    pair.commit(&pq).await;
    let ev = pair.game(p, q);
    pair.anomaly(p, ev.id, Severity::Suspicious).await;
    let res = pair.store.finish_batch(vec![ev.clone()]).await.unwrap().remove(0);
    assert_eq!(res.analysis_displaced, vec![pq[0].id]);
    let mut expected = ids(&pq[1..]);
    expected.push(ev.id);
    assert_eq!(pair.signal_jobs_of(p).await, expected);
    pair.store.close().await;
}

#[tokio::test]
async fn the_backlog_counts_at_most_100000_jobs_per_tier() {
    let _serial = SERIAL.lock().await;
    let dir = TempDir::new("backlog");
    let store = file_store(&dir).await;
    // Jobs without games rows: a raw connection without foreign keys.
    let raw = rusqlite::Connection::open(dir.file("scacelith.db")).unwrap();
    raw.execute_batch(
        "PRAGMA foreign_keys = OFF;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200010)
         INSERT INTO analysis_jobs (game_id, queued_at, priority) SELECT i, i, i % 2 FROM n",
    )
    .unwrap();
    drop(raw);
    assert_eq!(store.analysis().backlog().await.unwrap(), Backlog { ordinary: 100_000, priority: 100_000 });
    store.close().await;
}
