//! Tests of the reports, ported from anticheat.reports (the body checks and the HTTP answers are
//! the route's, tested with it), and of the service as the routes reach it ([`ReportDesk`]).

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::anticheat::testing::*;
use crate::clock::{Clock, ManualClock};
use crate::http::routes::reports::validate_report;
use crate::ids::GameId;
use crate::store::status::WHITE_WINS;
use crate::store::tests::support::LogCapture;
use crate::store::{GameRecord, IntegrityUpdate, NewUser, Report};

struct World {
    store: Store,
    clock: Arc<ManualClock>,
    reports: Reports,
    alice: UserId,
    bob: UserId,
    carol: UserId,
    dave: UserId,
}

/// An account created at `created_at`.
async fn account(store: &Store, name: &str, created_at: i64) -> UserId {
    let user = NewUser {
        username: name.into(),
        email: Some(format!("{name}@example.org")),
        password_hash: Some("hash".into()),
        email_verified: true,
        accept_challenges: true,
        created_at,
    };
    store.users().create(user).await.unwrap()
}

/// Four players a year old with 200 games each.
async fn world(per_day: i64) -> World {
    let config = config(&[("REPORTS_PER_DAY", &per_day.to_string())]);
    let clock = ManualClock::new(0.0, NOW);
    let store = store(&config, &clock).await;
    let mut ids = Vec::new();
    for name in ["alice", "bob", "carol", "dave"] {
        let id = account(&store, name, NOW - 365 * DAY).await;
        seed_rating(&store, id, "5+0", 1500, 200).await;
        ids.push(id);
    }
    let reports = Reports::new(&config, store.clone());
    World { store, clock, reports, alice: ids[0], bob: ids[1], carol: ids[2], dave: ids[3] }
}

impl World {
    /// A finished rated game between two players, ended at `ended_at`; returns its id.
    async fn game_at(&self, white: UserId, black: UserId, ended_at: i64) -> GameId {
        let name = |id: UserId| {
            let s = self.store.clone();
            async move { s.users().by_id(id).await.unwrap().unwrap().username }
        };
        let mut g: GameRecord = game(white, black, WHITE_WINS, ended_at);
        g.white_name = name(white).await;
        g.black_name = name(black).await;
        let id = g.id;
        self.store.finish_batch(vec![g]).await.unwrap();
        id
    }

    async fn game(&self, white: UserId, black: UserId) -> GameId {
        self.game_at(white, black, NOW - HOUR).await
    }

    /// Files a report as the route does: the body checked, then the service at the clock's time.
    async fn try_file(&self, reporter: UserId, body: Value) -> Result<ReportOutcome, StoreError> {
        let req = validate_report(&body).expect("a valid report");
        self.reports.file(reporter, req, self.clock.wall_ms()).await
    }

    async fn file(&self, reporter: UserId, body: Value) -> ReportOutcome {
        self.try_file(reporter, body).await.expect("the store works")
    }

    async fn can(&self, user: UserId, game: &GameSummary) -> bool {
        self.reports.can_report(user, game, self.clock.wall_ms()).await
    }

    /// Every report, oldest first.
    async fn all(&self) -> Vec<Report> {
        let mut out = Vec::new();
        for u in [self.alice, self.bob, self.carol, self.dave] {
            out.extend(self.store.reports().for_reported(u, 1000).await.unwrap());
        }
        out.sort_by_key(|r| r.id);
        out
    }

    async fn summary(&self, game: GameId) -> GameSummary {
        self.store.games().by_id(game).await.unwrap().unwrap().summary
    }
}

#[tokio::test]
async fn a_player_reports_their_opponent_and_the_report_is_stored_with_a_weight() {
    let w = world(5).await;
    let g = w.game(w.alice, w.bob).await;
    let logs = LogCapture::start();
    let res = w.file(w.alice, json!({ "gameId": g, "reported": "BOB", "category": "cheating", "comment": "  too perfect\u{7} " })).await;
    assert_eq!(res, ReportOutcome::Received);
    let all = w.all().await;
    assert_eq!(all.len(), 1);
    let r = &all[0];
    assert_eq!((r.reporter_id, r.reported_id, r.game_id), (w.alice, w.bob, Some(g)));
    assert_eq!(r.comment.as_deref(), Some("too perfect"));
    assert_eq!(r.category, ReportCategory::Cheating);
    assert!(r.weight > 0.9 && r.weight <= 1.0, "weight {}", r.weight);
    // Other tests log too: the game id is unique to this one.
    let filed: Vec<Value> = logs
        .records("anticheat")
        .into_iter()
        .filter(|r| r["msg"] == "report.filed" && r["gameId"] == g)
        .collect();
    assert_eq!(filed.len(), 1);
    assert_eq!(filed[0]["level"], "security");
    assert_eq!(filed[0]["reportedId"], w.bob);
    drop(logs);
    // Game ids may come as strings (53-bit ids in JSON).
    let g2 = w.game(w.bob, w.alice).await;
    assert_eq!(
        w.file(w.alice, json!({ "gameId": g2.to_string(), "reported": "bob", "category": "abuse" })).await,
        ReportOutcome::Received
    );
}

#[tokio::test]
async fn duplicates_get_the_same_answer_and_nothing_about_the_reported_account_leaks() {
    let w = world(5).await;
    let g = w.game(w.alice, w.bob).await;
    let a = w.file(w.alice, json!({ "gameId": g, "reported": "bob", "category": "cheating" })).await;
    let b = w
        .file(w.alice, json!({ "gameId": g, "reported": "bob", "category": "other", "comment": "again" }))
        .await;
    assert_eq!((a, b), (ReportOutcome::Received, ReportOutcome::Received));
    assert_eq!(w.all().await.len(), 1);
    // A flagged or unflagged reported player gets exactly the same answer.
    w.store
        .integrity()
        .set(
            w.carol,
            IntegrityUpdate {
                level: Some(crate::store::IntegrityLevel::HighConfidence),
                score: Some(6.0),
                ..IntegrityUpdate::default()
            },
        )
        .await
        .unwrap();
    let g2 = w.game(w.carol, w.alice).await;
    let c = w.file(w.alice, json!({ "gameId": g2, "reported": "carol", "category": "cheating" })).await;
    assert_eq!(c, a);
}

#[tokio::test]
async fn a_store_failure_is_returned() {
    let w = world(5).await;
    let g = w.game(w.alice, w.bob).await;
    exec(&w.store, "CREATE TRIGGER no_reports BEFORE INSERT ON reports BEGIN SELECT RAISE(ABORT, 'database is locked'); END;")
        .await;
    let res = w.try_file(w.alice, json!({ "gameId": g, "reported": "bob", "category": "cheating" })).await;
    assert!(matches!(res, Err(ref e) if e.message().contains("database is locked")), "{res:?}");
    w.store.close().await;
    assert!(
        w.try_file(w.alice, json!({ "gameId": g, "reported": "bob", "category": "cheating" })).await.is_err()
    );
}

#[tokio::test]
async fn only_the_opponent_of_a_recent_own_game_can_be_reported() {
    let w = world(5).await;
    let g = w.game(w.alice, w.bob).await;
    let not_allowed = |res: ReportOutcome| res == ReportOutcome::NotAllowed;
    assert!(
        not_allowed(w.file(w.carol, json!({ "gameId": g, "reported": "bob", "category": "cheating" })).await),
        "not a player"
    );
    assert!(
        not_allowed(
            w.file(w.alice, json!({ "gameId": g, "reported": "carol", "category": "cheating" })).await
        ),
        "not the opponent"
    );
    assert!(
        not_allowed(
            w.file(w.alice, json!({ "gameId": g, "reported": "alice", "category": "cheating" })).await
        ),
        "oneself"
    );
    assert!(
        not_allowed(
            w.file(w.alice, json!({ "gameId": 999_999, "reported": "bob", "category": "cheating" })).await
        ),
        "unknown game"
    );
    let old = w.game_at(w.alice, w.bob, NOW - 8 * DAY).await;
    assert!(
        not_allowed(
            w.file(w.alice, json!({ "gameId": old, "reported": "bob", "category": "cheating" })).await
        ),
        "too old"
    );
    let recent = w.game_at(w.alice, w.bob, NOW - 6 * DAY).await;
    assert_eq!(
        w.file(w.alice, json!({ "gameId": recent, "reported": "bob", "category": "cheating" })).await,
        ReportOutcome::Received
    );
    // A renamed opponent is still found through the account.
    let g3 = w.game(w.alice, w.dave).await;
    w.store
        .write(move |db| {
            db.connection()
                .execute("UPDATE users SET username = 'dave2' WHERE username = 'dave'", [])
                .map_err(StoreError::from)
        })
        .await
        .unwrap();
    assert_eq!(
        w.file(w.alice, json!({ "gameId": g3, "reported": "dave2", "category": "cheating" })).await,
        ReportOutcome::Received
    );
    assert_eq!(w.all().await.len(), 2);
}

#[tokio::test]
async fn reports_per_day_per_reporter() {
    let w = world(2).await;
    let ids = [w.game(w.alice, w.bob).await, w.game(w.alice, w.carol).await, w.game(w.alice, w.dave).await];
    assert_eq!(
        w.file(w.alice, json!({ "gameId": ids[0], "reported": "bob", "category": "cheating" })).await,
        ReportOutcome::Received
    );
    assert_eq!(
        w.file(w.alice, json!({ "gameId": ids[1], "reported": "carol", "category": "cheating" })).await,
        ReportOutcome::Received
    );
    let res = w.file(w.alice, json!({ "gameId": ids[2], "reported": "dave", "category": "cheating" })).await;
    assert_eq!(res, ReportOutcome::LimitReached);
    w.clock.advance((DAY + 1) as f64);
    assert_eq!(
        w.file(w.alice, json!({ "gameId": ids[2], "reported": "dave", "category": "cheating" })).await,
        ReportOutcome::Received
    );
}

#[test]
fn reporter_weight_depends_on_account_age_games_played_and_track_record() {
    let base = Reporter {
        created_at: Some(NOW - 365 * DAY),
        games_played: 500,
        actioned: 0,
        dismissed: 0,
        level: IntegrityLevel::None,
        now: NOW,
    };
    assert_eq!(reporter_weight(&base), ReporterWeight { weight: 1.0, base: 1.0, trust: 1.0 });
    assert!(reporter_weight(&Reporter { created_at: Some(NOW - HOUR), ..base }).weight < 0.2, "an hour old");
    assert!(reporter_weight(&Reporter { games_played: 2, ..base }).weight < 0.3, "hardly played");
    assert!(reporter_weight(&Reporter { actioned: 5, ..base }).weight > 1.5, "useful reporter");
    assert!(reporter_weight(&Reporter { dismissed: 6, ..base }).weight <= 0.25, "dismissed again and again");
    assert!(reporter_weight(&Reporter { level: IntegrityLevel::Confirmed, ..base }).weight <= 0.2);
    assert_eq!(reporter_weight(&Reporter { level: IntegrityLevel::HighConfidence, ..base }).weight, 0.5);
    assert!(
        reporter_weight(&Reporter { created_at: Some(NOW - HOUR), games_played: 0, ..base }).weight >= 0.02
    );
    assert_eq!(
        reporter_weight(&Reporter { created_at: None, ..base }).weight,
        0.1,
        "unknown age: created now"
    );
}

#[tokio::test]
async fn past_outcomes_feed_the_weight_of_new_reports() {
    let w = world(5).await;
    let g1 = w.game(w.alice, w.bob).await;
    w.file(w.alice, json!({ "gameId": g1, "reported": "bob", "category": "cheating" })).await;
    let first = w.all().await[0].clone();
    assert!(
        w.store.reports().resolve(first.id, ReportStatus::Dismissed, Some("mod".into()), NOW).await.unwrap()
    );
    let g2 = w.game(w.alice, w.carol).await;
    w.file(w.alice, json!({ "gameId": g2, "reported": "carol", "category": "cheating" })).await;
    let second = w.all().await.into_iter().find(|r| r.reported_id == w.carol).unwrap();
    assert!(second.weight < first.weight, "a dismissed report lowers credibility");
}

#[test]
fn reports_against_one_player_do_not_add_up_linearly() {
    let r = |weight, at| Received { weight, at };
    assert_eq!(capped_weight(1.0, &[], NOW), 1.0);
    assert_eq!(capped_weight(1.0, &[r(1.0, NOW - 1000), r(0.8, NOW - 2000)], NOW), 0.2);
    assert_eq!(capped_weight(1.0, &[r(2.0, NOW - 2 * DAY)], NOW), 1.0, "yesterday does not count");
    // Low-credibility reports share a small budget.
    assert_eq!(capped_weight(0.3, &[r(0.3, NOW)], NOW), 0.2);
    assert_eq!(capped_weight(0.3, &[r(0.3, NOW), r(0.2, NOW)], NOW), 0.0);
    assert_eq!(recent_report_weight(&[r(0.5, NOW - 29 * DAY), r(0.25, NOW - 31 * DAY)], NOW, 30), 0.5);
}

#[tokio::test]
async fn sock_puppets_weigh_little_and_a_credible_reporter_still_counts() {
    let w = world(5).await;
    // Ten brand-new accounts that each played the target once.
    for i in 0..10 {
        let sock = account(&w.store, &format!("sock{i}"), NOW - HOUR).await;
        let g = w.game(sock, w.bob).await;
        assert_eq!(
            w.file(sock, json!({ "gameId": g, "reported": "bob", "category": "cheating" })).await,
            ReportOutcome::Received
        );
    }
    let all = w.all().await;
    let total: f64 = all.iter().map(|r| r.weight).sum();
    assert!(total <= LOW_CRED_DAILY_CAP + 1e-9, "ten sock puppets weigh {total}");
    assert_eq!(all.len(), 10, "all reports are kept for moderators");
    let g = w.game(w.alice, w.bob).await;
    w.file(w.alice, json!({ "gameId": g, "reported": "bob", "category": "cheating" })).await;
    assert!(w.all().await[10].weight >= 0.9);
}

#[tokio::test]
async fn the_daily_cap_counts_every_report_received() {
    let w = world(5).await;
    let mut reporters = Vec::new();
    for i in 0..4 {
        reporters.push(account(&w.store, &format!("r{i}"), NOW - 365 * DAY).await);
    }
    let puppet = account(&w.store, "puppet", NOW - 365 * DAY).await;
    let g = w.game(w.alice, w.bob).await;
    // 2.0 already received today (the daily cap), then 200 newer reports the cap brought to 0.
    for (i, &r) in reporters.iter().enumerate() {
        let report = NewReport {
            reporter_id: r,
            reported_id: w.bob,
            game_id: Some(10 + i as u64),
            category: ReportCategory::Cheating,
            comment: None,
            weight: 0.5,
            at: NOW - 20 * HOUR,
        };
        w.store.reports().create(report).await.unwrap();
    }
    for i in 0..200 {
        let report = NewReport {
            reporter_id: puppet,
            reported_id: w.bob,
            game_id: Some(1000 + i),
            category: ReportCategory::Cheating,
            comment: None,
            weight: 0.0,
            at: NOW - 10 * HOUR + i as i64,
        };
        w.store.reports().create(report).await.unwrap();
    }
    let sums = w.store.reports().weight_since(w.bob, NOW - DAY, LOW_CREDIBILITY).await.unwrap();
    assert_eq!((sums.total, sums.low), (2.0, 0.0));
    let newest: Vec<Received> = w
        .store
        .reports()
        .for_reported(w.bob, 200)
        .await
        .unwrap()
        .iter()
        .map(|r| Received { weight: r.weight, at: r.created_at })
        .collect();
    assert_eq!(capped_weight(1.0, &newest, NOW), 1.0, "the newest 200 alone miss the cap");
    assert_eq!(
        w.file(w.alice, json!({ "gameId": g, "reported": "bob", "category": "cheating" })).await,
        ReportOutcome::Received
    );
    let filed = &w.store.reports().for_reported(w.bob, 1).await.unwrap()[0];
    assert_eq!(filed.reporter_id, w.alice);
    assert_eq!(filed.weight, 0.0, "the daily cap is reached");
}

#[tokio::test]
async fn only_a_credible_report_asks_for_the_analysis_at_report_priority() {
    let w = world(5).await;
    let priority = |g: GameId| {
        let s = w.store.clone();
        async move { s.analysis().job(g).await.unwrap().map(|j| j.priority) }
    };
    // Credible reporters: weight about 1 each, up to the daily cap of 2.
    let g1 = w.game(w.alice, w.bob).await;
    w.file(w.alice, json!({ "gameId": g1, "reported": "bob", "category": "cheating" })).await;
    let g2 = w.game(w.carol, w.bob).await;
    w.file(w.carol, json!({ "gameId": g2, "reported": "bob", "category": "other" })).await;
    // Beyond the cap the stored weight is 0: no longer credible.
    let g3 = w.game(w.dave, w.bob).await;
    w.file(w.dave, json!({ "gameId": g3, "reported": "bob", "category": "cheating" })).await;
    // A brand-new account.
    let fresh = account(&w.store, "fresh", NOW - HOUR).await;
    let g4 = w.game(fresh, w.carol).await;
    w.file(fresh, json!({ "gameId": g4, "reported": "carol", "category": "cheating" })).await;
    // An abuse report asks for nothing.
    let g5 = w.game(w.carol, w.dave).await;
    let before = priority(g5).await;
    w.file(w.carol, json!({ "gameId": g5, "reported": "dave", "category": "abuse" })).await;

    let credible: Vec<bool> = w.all().await.iter().map(|r| r.weight >= LOW_CREDIBILITY).collect();
    assert_eq!(credible, [true, true, false, false, true]);
    assert_eq!(priority(g1).await, Some(Priority::Report));
    assert_eq!(priority(g2).await, Some(Priority::Report));
    assert_eq!(priority(g3).await, Some(Priority::Signal));
    assert_eq!(priority(g4).await, Some(Priority::Signal));
    assert_eq!(priority(g5).await, before);
}

#[tokio::test]
async fn can_report_answers_the_rules_without_filing_anything() {
    let w = world(2).await;
    let g = w.summary(w.game(w.alice, w.bob).await).await;
    assert!(w.can(w.alice, &g).await);
    assert!(w.can(w.bob, &g).await, "either player");
    assert!(!w.can(w.carol, &g).await, "not a player of that game");
    let old = w.summary(w.game_at(w.alice, w.bob, NOW - 8 * DAY).await).await;
    assert!(!w.can(w.alice, &old).await, "too old");
    let future = w.summary(w.game_at(w.alice, w.bob, NOW + HOUR).await).await;
    assert!(!w.can(w.alice, &future).await, "not ended yet");
    let mut own = g.clone();
    own.black_id = w.alice;
    assert!(!w.can(w.alice, &own).await, "against oneself");
    assert!(w.all().await.is_empty(), "nothing filed");
    assert_eq!(
        w.file(w.alice, json!({ "gameId": g.id, "reported": "bob", "category": "cheating" })).await,
        ReportOutcome::Received
    );
    assert!(!w.can(w.alice, &g).await, "already reported");
    assert!(w.can(w.bob, &g).await, "the opponent still may");
    let g2 = w.game(w.alice, w.carol).await;
    let g3 = w.summary(w.game(w.alice, w.dave).await).await;
    assert_eq!(
        w.file(w.alice, json!({ "gameId": g2, "reported": "carol", "category": "abuse" })).await,
        ReportOutcome::Received
    );
    assert!(!w.can(w.alice, &g3).await, "REPORTS_PER_DAY reached");
    assert_eq!(
        w.file(w.alice, json!({ "gameId": g3.id, "reported": "dave", "category": "abuse" })).await,
        ReportOutcome::LimitReached
    );
    w.clock.advance((DAY + 1) as f64);
    assert!(w.can(w.alice, &g3).await, "the next day");
    w.store.close().await;
    assert!(!w.can(w.alice, &g3).await, "a store failure answers false");
}

#[test]
fn review_priority_grows_with_level_score_and_logarithmically_with_reports() {
    assert_eq!(review_priority(IntegrityLevel::None, 0.0, 0.0), 0);
    let one = review_priority(IntegrityLevel::None, 0.0, 1.0);
    let ten = review_priority(IntegrityLevel::None, 0.0, 10.0);
    assert!(one > 0 && ten > one && ten < 10 * one);
    assert!(
        review_priority(IntegrityLevel::HighConfidence, 4.0, 0.0)
            > review_priority(IntegrityLevel::Suspected, 4.0, 0.0)
    );
    assert!(
        review_priority(IntegrityLevel::Suspected, 3.6, 0.0)
            > review_priority(IntegrityLevel::None, 0.0, 2.0)
    );
    assert_eq!(review_priority(IntegrityLevel::Confirmed, f64::NAN, -3.0), 5);
}

// ---- through the routes' interface -------------------------------------------------------------

fn auth(user: UserId, username: &str) -> AuthInfo {
    AuthInfo {
        user_id: user,
        username: username.into(),
        session_id: 1,
        email_verified: true,
        token_hash: None,
    }
}

fn request(game_id: GameId, reported: &str, category: ReportCategory) -> ReportRequest {
    ReportRequest { game_id, reported: reported.into(), category, comment: String::new() }
}

#[tokio::test]
async fn the_routes_file_reports_and_ask_reportable_through_the_report_desk() {
    let w = world(1).await;
    let desk: Arc<dyn ReportDesk> = Arc::new(w.reports.clone());
    let g = w.game(w.alice, w.bob).await;
    let summary = w.summary(g).await;
    // The routes give their own clock: the time of the request.
    let at = NOW + 5 * 60_000;
    assert!(desk.can_report(w.alice, summary.clone(), at).await);
    let filed = desk.file(auth(w.alice, "alice"), request(g, "Bob", ReportCategory::Cheating), at).await;
    assert_eq!(filed.unwrap(), ReportOutcome::Received);
    let all = w.all().await;
    assert_eq!((all.len(), all[0].reported_id, all[0].created_at), (1, w.bob, at));
    assert!(!desk.can_report(w.alice, summary.clone(), at).await, "already reported");
    let g2 = w.game(w.alice, w.carol).await;
    let limit = desk.file(auth(w.alice, "alice"), request(g2, "carol", ReportCategory::Abuse), at).await;
    assert_eq!(limit.unwrap(), ReportOutcome::LimitReached, "REPORTS_PER_DAY is 1");
    let other = desk.file(auth(w.carol, "carol"), request(g, "bob", ReportCategory::Cheating), at).await;
    assert_eq!(other.unwrap(), ReportOutcome::NotAllowed, "not a player of that game");
    assert!(!desk.can_report(w.alice, summary, at + 8 * DAY).await, "too old by then");
}

#[tokio::test]
async fn a_store_failure_reaches_the_routes_as_an_internal_error() {
    let w = world(5).await;
    let desk: Arc<dyn ReportDesk> = Arc::new(w.reports.clone());
    let g = w.game(w.alice, w.bob).await;
    let summary = w.summary(g).await;
    w.store.close().await;
    let err = desk
        .file(auth(w.alice, "alice"), request(g, "bob", ReportCategory::Cheating), NOW)
        .await
        .unwrap_err();
    assert_eq!((err.status, err.code.as_ref()), (500, "internal_error"));
    assert!(err.internal.is_some(), "the store error is kept for the log");
    assert!(!desk.can_report(w.alice, summary, NOW).await);
}
