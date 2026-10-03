//! `POST /reports`, ported from the route parts of the Node suites `anticheat.reports`
//! (registration, body validation, answers) and `http.quotas` (the `reports` limit). The
//! eligibility rules, weights and caps are the reports service's, tested with it.

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::super::read_support::*;
use super::*;
use crate::store::{GameRecord, Store};

const NOW: i64 = 1_800_000_000_000;

/// A reports service answering `outcome` and keeping what it was given.
struct RecordingDesk {
    outcome: Mutex<Result<ReportOutcome, ApiError>>,
    filed: Mutex<Vec<(UserId, ReportRequest, i64)>>,
}

impl RecordingDesk {
    fn new() -> Arc<RecordingDesk> {
        Arc::new(RecordingDesk { outcome: Mutex::new(Ok(ReportOutcome::Received)), filed: Mutex::default() })
    }
}

impl ReportDesk for RecordingDesk {
    fn file(
        &self,
        reporter: AuthInfo,
        report: ReportRequest,
        now_ms: i64,
    ) -> BoxFuture<Result<ReportOutcome, ApiError>> {
        self.filed.lock().push((reporter.user_id, report, now_ms));
        let outcome = self.outcome.lock().clone();
        Box::pin(async move { outcome })
    }

    fn can_report(&self, _user: UserId, _game: GameSummary, _now_ms: i64) -> BoxFuture<bool> {
        Box::pin(async { false })
    }
}

async fn start_with(desk: Arc<dyn ReportDesk>, store: Store, config: Config) -> Server {
    Server::start(config, store, NOW, move |router, config| {
        register(router, ReportsDeps { config: config.clone(), desk });
    })
}

async fn recording() -> (Server, Arc<RecordingDesk>) {
    let config = config(&[("REPORTS_PER_DAY", "2"), ("USER_RATE_PER_MIN", "100000")]);
    let store = memory_store(&config).await;
    let desk = RecordingDesk::new();
    (start_with(desk.clone(), store, config).await, desk)
}

#[tokio::test]
async fn registration_auth_required_own_body_validation_and_the_reports_rate() {
    let config = Arc::new(Config::for_tests());
    let mut router = Router::new();
    register(&mut router, ReportsDeps { config, desk: RecordingDesk::new() });
    let route = &router.routes()[0];
    assert_eq!(route.label(), "POST /api/v1/reports");
    assert_eq!(route.opts().auth, AuthMode::Required);
    assert!(route.opts().own_body_validation);
    let rate = &route.opts().rates[0];
    assert_eq!(
        (rate.key.as_ref(), rate.limit, rate.window_ms, rate.by_user, rate.shared),
        ("reports", 30.0, 3_600_000, true, false)
    );
}

#[tokio::test]
async fn the_answers_of_the_service_outcomes() {
    let (s, desk) = recording().await;
    let alice = s.user("Alice").await;
    let post = |body: Value| s.t.post("/api/v1/reports").bearer(&alice.token).json(&body).send();
    let body = json!({ "gameId": "4100000000007", "reported": " BOB ", "category": "cheating", "comment": "  too perfect\u{7} " });
    let r = post(body.clone()).await;
    assert_eq!((r.status, r.json()), (202, json!({ "status": "received" })));
    let filed = desk.filed.lock().clone();
    assert_eq!(
        filed,
        [(
            alice.id,
            ReportRequest {
                game_id: 4_100_000_000_007,
                reported: "BOB".into(),
                category: ReportCategory::Cheating,
                comment: "too perfect".into()
            },
            NOW
        )]
    );

    *desk.outcome.lock() = Ok(ReportOutcome::LimitReached);
    let r = post(body.clone()).await;
    assert_eq!(r.status, 429);
    assert_eq!(
        r.json(),
        json!({ "error": "report_limit", "message": "At most 2 reports per day.", "retryAfter": 3600 })
    );
    assert_eq!(r.headers.get_all("retry-after").iter().collect::<Vec<_>>(), ["3600"]);

    *desk.outcome.lock() = Ok(ReportOutcome::NotAllowed);
    let r = post(body.clone()).await;
    assert_eq!(
        (r.status, r.json()),
        (
            403,
            json!({
                "error": "report_not_allowed",
                "message": "You can report the opponent of one of your games that ended in the last 7 days.",
            })
        )
    );

    *desk.outcome.lock() = Err(ApiError::internal("database is locked"));
    let r = post(body.clone()).await;
    assert_eq!((r.status, r.json()["error"].clone()), (500, json!("internal_error")));
    assert!(!r.text().contains("locked"), "the detail is not shown");

    // Without a session: 401 before the body is looked at.
    let r = s.t.post("/api/v1/reports").json(&body).send().await;
    assert_eq!(r.status, 401);
    assert_eq!(desk.filed.lock().len(), 4);
}

#[tokio::test]
async fn validation_of_the_body() {
    let (s, desk) = recording().await;
    let alice = s.user("Alice").await;
    let cases: Vec<(Value, &str)> = vec![
        (json!(null), "A JSON object is expected."),
        (json!([1]), "A JSON object is expected."),
        (json!("text"), "A JSON object is expected."),
        (json!({ "gameId": -1, "reported": "bob", "category": "cheating" }), "gameId must be a game id."),
        (json!({ "gameId": 1.5, "reported": "bob", "category": "cheating" }), "gameId must be a game id."),
        (json!({ "gameId": 0, "reported": "bob", "category": "cheating" }), "gameId must be a game id."),
        (json!({ "gameId": "12a", "reported": "bob", "category": "cheating" }), "gameId must be a game id."),
        (
            json!({ "gameId": "99999999999999999", "reported": "bob", "category": "cheating" }),
            "gameId must be a game id.",
        ),
        (
            json!({ "gameId": "9007199254740992", "reported": "bob", "category": "cheating" }),
            "gameId must be a game id.",
        ),
        (
            json!({ "gameId": 9_007_199_254_740_992u64, "reported": "bob", "category": "cheating" }),
            "gameId must be a game id.",
        ),
        (json!({ "reported": "bob", "category": "cheating" }), "gameId must be a game id."),
        (json!({ "gameId": 5, "reported": "", "category": "cheating" }), "reported must be a username."),
        (json!({ "gameId": 5, "reported": "   ", "category": "cheating" }), "reported must be a username."),
        (
            json!({ "gameId": 5, "reported": "x".repeat(25), "category": "cheating" }),
            "reported must be a username.",
        ),
        (json!({ "gameId": 5, "reported": 7, "category": "cheating" }), "reported must be a username."),
        (
            json!({ "gameId": 5, "reported": "bob", "category": "rude" }),
            "category must be one of cheating, abuse, other.",
        ),
        (
            json!({ "gameId": 5, "reported": "bob", "category": ["abuse"] }),
            "category must be one of cheating, abuse, other.",
        ),
        (
            json!({ "gameId": 5, "reported": "bob", "category": "other", "comment": 42 }),
            "comment must be text.",
        ),
        (
            json!({ "gameId": 5, "reported": "bob", "category": "other", "comment": "é".repeat(501) }),
            "comment is limited to 500 characters.",
        ),
    ];
    for (body, message) in cases {
        let r = s.t.post("/api/v1/reports").bearer(&alice.token).json(&body).send().await;
        assert_eq!(
            (r.status, r.json()),
            (400, json!({ "error": "invalid_request", "message": message })),
            "{body}"
        );
    }
    assert!(desk.filed.lock().is_empty(), "nothing reaches the service");
    // A body that is not JSON is the framework's 400.
    let r = s.t.post("/api/v1/reports").bearer(&alice.token).body("application/json", "{").send().await;
    assert_eq!(r.status, 400);
}

#[test]
fn validate_report_normalizes_what_it_takes() {
    let ok = |body: Value| validate_report(&body).unwrap();
    let r =
        ok(json!({ "gameId": 5.0, "reported": "\u{a0}bob\u{feff}", "category": "abuse", "comment": null }));
    assert_eq!(
        r,
        ReportRequest {
            game_id: 5,
            reported: "bob".into(),
            category: ReportCategory::Abuse,
            comment: String::new()
        }
    );
    assert_eq!(
        ok(json!({ "gameId": "0000000000000042", "reported": "bob", "category": "other" })).game_id,
        42
    );
    assert_eq!(
        ok(json!({ "gameId": 9_007_199_254_740_991u64, "reported": "b", "category": "other" })).game_id,
        9_007_199_254_740_991
    );
    let r = ok(json!({ "gameId": 1, "reported": "bob", "category": "other", "comment": "é".repeat(500) }));
    assert_eq!(r.comment.chars().count(), 500);
    let r = ok(
        json!({ "gameId": 1, "reported": "bob", "category": "other", "comment": "\u{0}a\tb\nc\u{1f}\u{7f}\r " }),
    );
    assert_eq!(r.comment, "a\tb\nc", "control characters but tab and line feed dropped, then trimmed");
    // 24 UTF-16 units at most: an emoji counts twice (JavaScript's length).
    assert!(
        validate_report(&json!({ "gameId": 1, "reported": "😀".repeat(12), "category": "other" })).is_ok()
    );
    assert!(
        validate_report(
            &json!({ "gameId": 1, "reported": format!("a{}", "😀".repeat(12)), "category": "other" })
        )
        .is_err()
    );
    // The comment counts code points: 500 emoji pass.
    assert!(
        validate_report(
            &json!({ "gameId": 1, "reported": "b", "category": "other", "comment": "😀".repeat(500) })
        )
        .is_ok()
    );
}

/// A finished game between two players, ended an hour before [`NOW`].
fn game(id: u64, white: &Player, black: &Player) -> GameRecord {
    let mut g = record(id, white, black, moves_of("e2e4"));
    (g.started_at, g.ended_at) = (Some(NOW - 2 * HOUR), Some(NOW - HOUR));
    g
}

#[tokio::test]
async fn through_the_store_rules_eligibility_quota_duplicates_and_the_request_limit() {
    let config = config(&[("REPORTS_PER_DAY", "2"), ("USER_RATE_PER_MIN", "100000")]);
    let store = memory_store(&config).await;
    let desk = Arc::new(StoreDesk { store: store.clone(), reports_per_day: 2 });
    let s = start_with(desk, store, config).await;
    let [alice, bob, carol, dave] =
        [s.user("Alice").await, s.user("Bob").await, s.user("Carol").await, s.user("Dave").await];
    let games = [game(1, &alice, &bob), game(2, &carol, &alice), game(3, &alice, &dave)];
    s.commit(games.to_vec()).await;
    let post = |who: &Player, body: Value| s.t.post("/api/v1/reports").bearer(&who.token).json(&body).send();
    let status = |who: &Player, game: u64, reported: &str| {
        let body = json!({ "gameId": game, "reported": reported, "category": "cheating" });
        let req = post(who, body);
        async move { req.await.status }
    };
    assert_eq!(status(&carol, 1, "bob").await, 403, "not a player of that game");
    assert_eq!(status(&alice, 1, "carol").await, 403, "not the opponent");
    assert_eq!(status(&alice, 1, "alice").await, 403, "oneself");
    assert_eq!(status(&alice, 999, "bob").await, 403, "unknown game");
    assert_eq!(status(&alice, 1, "BOB").await, 202);
    assert_eq!(status(&alice, 1, "bob").await, 202, "a duplicate gets the same answer");
    assert_eq!(status(&alice, 2, "carol").await, 202);
    let r = post(&alice, json!({ "gameId": 3, "reported": "dave", "category": "abuse" })).await;
    assert_eq!((r.status, r.header("retry-after")), (429, Some("3600")));
    assert_eq!(s.store.reports().for_reporter(alice.id, 10).await.unwrap().len(), 2);
    // The coarse limit: 30 requests an hour per player (the requests above: 7).
    let body = json!({ "gameId": 999, "reported": "bob", "category": "cheating" });
    for _ in 0..23 {
        assert_eq!(post(&alice, body.clone()).await.json()["error"], "report_limit");
    }
    let r = post(&alice, body).await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")));
    assert_eq!(status(&bob, 1, "alice").await, 202, "per player");
}
