//! Tests of the refund notices, ported from anticheat.refund-notices: a fake lobby (presence,
//! games, connections past their Welcome) drives the state machine as the real one does.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::*;
use crate::anticheat::testing::*;
use crate::clock::ManualClock;
use crate::store::RefundScope;
use crate::store::status::WHITE_WINS;

/// A lobby: who is online, who plays, which connections are past their Welcome.
#[derive(Default)]
struct Lobby {
    online: HashSet<UserId>,
    playing: HashSet<UserId>,
    welcomed: HashSet<UserId>,
    /// Answers of `send_notice` to give first (then: welcomed or not).
    answers: VecDeque<bool>,
    sent: Vec<(UserId, i64)>,
    tries: usize,
}

impl Lobby {
    fn connect(&mut self, user: UserId, welcomed: bool) {
        self.online.insert(user);
        if welcomed {
            self.welcomed.insert(user);
        }
    }

    fn notices(&self, user: UserId) -> Vec<i64> {
        self.sent.iter().filter(|(u, _)| *u == user).map(|(_, p)| *p).collect()
    }
}

impl RefundHost for Lobby {
    fn can_notify(&self, user: UserId) -> bool {
        self.online.contains(&user) && !self.playing.contains(&user)
    }

    fn send_notice(&mut self, user: UserId, points: i64) -> bool {
        self.tries += 1;
        let ok = self.answers.pop_front().unwrap_or_else(|| self.welcomed.contains(&user));
        if ok {
            self.sent.push((user, points));
        }
        ok
    }
}

struct Harness {
    notices: RefundNotices,
    events: mpsc::UnboundedReceiver<RefundEvent>,
    lobby: Lobby,
    store: Store,
    ids: Vec<UserId>,
}

async fn harness(retry_ms: u64) -> Harness {
    let config = config(&[]);
    let clock = ManualClock::new(0.0, NOW);
    let store = store(&config, &clock).await;
    let ids = users(&store, &["cheat", "u1", "u2", "u3", "u4", "u5", "u6", "u7"]).await;
    let (tx, events) = mpsc::unbounded_channel();
    let post: RefundPost = Arc::new(move |e| {
        let _ = tx.send(e);
    });
    let notices = RefundNotices::new(store.clone(), clock, post)
        .with_timing(Duration::from_millis(60_000), Duration::from_millis(retry_ms));
    Harness { notices, events, lobby: Lobby::default(), store, ids }
}

impl Harness {
    /// Handles the events until none comes for 50 ms.
    async fn settle(&mut self) {
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(50), self.events.recv()).await {
            self.notices.handle(e, &mut self.lobby);
        }
    }

    /// Handles the events until `cond` holds (5 s at most), then settles.
    async fn until(&mut self, cond: impl Fn(&Lobby) -> bool) {
        let end = tokio::time::Instant::now() + Duration::from_secs(5);
        while !cond(&self.lobby) {
            match tokio::time::timeout_at(end, self.events.recv()).await {
                Ok(Some(e)) => self.notices.handle(e, &mut self.lobby),
                _ => break,
            }
        }
        self.settle().await;
    }

    /// A refund of `points` to `victim` (the game is stored first).
    async fn refund(&self, victim: UserId, points: i64) {
        let mut g = game(self.ids[0], victim, WHITE_WINS, NOW - DAY);
        g.rated = false;
        let id = g.id;
        self.store.finish_batch(vec![g]).await.unwrap();
        let cheater = self.ids[0];
        self.store
            .write(move |db| {
                db.connection()
                    .execute(
                        "INSERT INTO rating_refunds (game_id, victim_id, cheater_id, category, points, created_at,
                         sanction_id, source) VALUES (?1, ?2, ?3, '3+2', ?4, ?5, 1, 'auto')",
                        rusqlite::params![id as i64, victim, cheater, points, NOW],
                    )
                    .map_err(StoreError::from)
            })
            .await
            .unwrap();
    }

    async fn notified(&self, victim: UserId) -> Vec<Option<i64>> {
        let mut rows = self.store.refunds().list(RefundScope::Victim(victim), 100).await.unwrap();
        rows.reverse();
        rows.into_iter().map(|r| r.notified_at).collect()
    }
}

#[tokio::test]
async fn a_connected_idle_victim_is_told_at_once_with_the_total_then_marked_notified() {
    let mut h = harness(RETRY_MS).await;
    let (u1, u2) = (h.ids[1], h.ids[2]);
    h.lobby.connect(u1, true);
    h.refund(u1, 10).await;
    h.refund(u1, 5).await;
    h.refund(u2, 7).await; // offline victim
    // What the lobby does on SanctionEvents::refunds_pending.
    h.notices.poll();
    h.settle().await;
    assert_eq!(h.lobby.notices(u1), [15]);
    assert_eq!(h.notified(u1).await, [Some(NOW), Some(NOW)]);
    assert_eq!(h.notified(u2).await, [None], "the offline victim waits");
    assert_eq!(h.notices.waiting(), 1);
    // Nothing is sent twice.
    h.notices.poll();
    h.settle().await;
    assert_eq!(h.lobby.notices(u1), [15]);
}

#[tokio::test]
async fn a_victim_in_a_game_is_told_after_it_ends() {
    let mut h = harness(RETRY_MS).await;
    let (u1, u2) = (h.ids[1], h.ids[2]);
    h.lobby.connect(u1, true);
    h.lobby.connect(u2, true);
    h.lobby.playing.extend([u1, u2]);
    h.refund(u1, 12).await;
    h.notices.poll();
    h.settle().await;
    assert!(h.lobby.notices(u1).is_empty(), "no notice during the game");
    assert_eq!(h.notified(u1).await, [None]);
    h.lobby.playing.clear();
    h.notices.game_ended(&[u1, u2], &mut h.lobby);
    h.settle().await;
    assert_eq!(h.lobby.notices(u1), [12]);
    assert!(h.lobby.notices(u2).is_empty());
    assert_eq!(h.notified(u1).await, [Some(NOW)]);
}

#[tokio::test]
async fn an_offline_victim_is_told_after_welcome_and_a_resumed_game_waits_for_its_end() {
    // Retries slower than a settle: the series is not used up while the victim is not welcomed.
    let mut h = harness(100).await;
    let (u3, u4, u5) = (h.ids[3], h.ids[4], h.ids[5]);
    h.refund(u3, 8).await;
    h.refund(u4, 9).await;
    // Reads the refunds already waiting (an admin command, a restart).
    h.notices.start();
    h.settle().await;
    assert_eq!(h.notices.waiting(), 2);

    // Victim 3 connects: admitted before Welcome, the notice follows once it is welcomed.
    h.lobby.connect(u3, false);
    h.notices.connected(u3, false);
    h.until(|l| l.tries >= 2).await;
    assert!(h.lobby.notices(u3).is_empty());
    assert_eq!(h.notified(u3).await, [None], "not marked while not queued");
    h.lobby.welcomed.insert(u3);
    h.until(|l| !l.notices(u3).is_empty()).await;
    assert_eq!(h.lobby.notices(u3), [8]);
    assert_eq!(h.notified(u3).await, [Some(NOW)]);

    // Victim 4 comes back to a game in progress (after a restart, for example).
    h.lobby.connect(u5, true);
    h.lobby.connect(u4, true);
    h.lobby.playing.extend([u4, u5]);
    h.notices.connected(u4, true);
    h.settle().await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    h.settle().await;
    assert!(h.lobby.notices(u4).is_empty());
    h.lobby.playing.clear();
    h.notices.game_ended(&[u4, u5], &mut h.lobby);
    h.settle().await;
    assert_eq!(h.lobby.notices(u4), [9]);
    assert!(h.lobby.notices(u5).is_empty());
    h.notices.stop();
}

#[tokio::test]
async fn a_notice_never_queued_is_tried_retries_times_then_once_per_poll() {
    let mut h = harness(1).await;
    let u7 = h.ids[7];
    h.refund(u7, 4).await;
    h.lobby.online.insert(u7); // online, never welcomed
    h.notices.poll();
    h.until(|l| l.tries >= 21).await;
    assert_eq!(h.lobby.tries, 21, "the first try and RETRIES (20) more");
    h.notices.poll();
    h.until(|l| l.tries >= 22).await;
    assert_eq!(h.lobby.tries, 22, "one try per poll once the series is used up");
    h.notices.poll();
    h.until(|l| l.tries >= 23).await;
    assert_eq!(h.lobby.tries, 23);
    h.notices.connected(u7, false);
    h.until(|l| l.tries >= 43).await;
    assert_eq!(h.lobby.tries, 43, "a new series of RETRIES after a new connection");
    assert_eq!(h.notified(u7).await, [None]);
    h.notices.stop();
}

#[tokio::test]
async fn a_notice_not_queued_is_not_marked_and_is_tried_again() {
    let mut h = harness(5).await;
    let u7 = h.ids[7];
    h.refund(u7, 4).await;
    h.lobby.connect(u7, true);
    h.lobby.answers.extend([false, false, true]);
    h.notices.poll();
    h.until(|l| l.sent.len() == 1).await;
    assert_eq!(h.lobby.tries, 3);
    assert_eq!(h.lobby.notices(u7), [4]);
    assert_eq!(h.notified(u7).await, [Some(NOW)]);
    // A later refund gets a notice of its own.
    h.refund(u7, 3).await;
    h.notices.poll();
    h.until(|l| l.sent.len() == 2).await;
    assert_eq!(h.lobby.notices(u7), [4, 3]);
    assert_eq!(h.notified(u7).await, [Some(NOW), Some(NOW)]);
}

#[tokio::test]
async fn a_victim_who_starts_a_game_while_the_refunds_are_read_is_not_interrupted() {
    let mut h = harness(RETRY_MS).await;
    let u1 = h.ids[1];
    h.lobby.connect(u1, true);
    h.refund(u1, 6).await;
    h.notices.poll();
    // The poll's answer makes the notices read the victim's refunds...
    let polled = h.events.recv().await.unwrap();
    h.notices.handle(polled, &mut h.lobby);
    // ...and the victim starts a game before that read answers.
    h.lobby.playing.insert(u1);
    h.settle().await;
    assert!(h.lobby.notices(u1).is_empty());
    assert_eq!(h.notices.waiting(), 1);
    h.lobby.playing.clear();
    h.notices.game_ended(&[u1], &mut h.lobby);
    h.settle().await;
    assert_eq!(h.lobby.notices(u1), [6]);
}

#[tokio::test]
async fn refunds_found_during_a_notice_get_a_second_one() {
    let mut h = harness(RETRY_MS).await;
    let u1 = h.ids[1];
    h.lobby.connect(u1, true);
    h.refund(u1, 6).await;
    h.notices.poll();
    let polled = h.events.recv().await.unwrap();
    h.notices.handle(polled, &mut h.lobby);
    // A new refund is found while the first notice is in flight.
    h.refund(u1, 2).await;
    h.notices.poll();
    h.settle().await;
    let total: i64 = h.lobby.notices(u1).iter().sum();
    assert_eq!(total, 8, "{:?}", h.lobby.notices(u1));
    assert_eq!(h.notified(u1).await, [Some(NOW), Some(NOW)]);
    assert_eq!(h.notices.waiting(), 0);
}
