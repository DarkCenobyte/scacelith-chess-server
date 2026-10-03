//! Port of store.account-api.test.js: the filtered game history and its count (with their query
//! plans), the e-mail change, a user's tokens, every session, security events, sanctions, conduct
//! events, reports filed and refunds received.

use rusqlite::types::Value as SqlValue;
use serde_json::json;

use super::support::*;
use crate::ids::UserId;
use crate::store::{
    CheaterRefunds, ConductEvent, ConductKind, ErrorKind, GAMES_COUNT_FOR_USER_SQL, GAMES_FOR_USER_SQL,
    GameFilter, GameRecord, GameSummary, NewReport, NewSanction, NewSecurityEvent, NewSession, NewToken,
    RefundScope, ReportCategory, ReportStatus, ResultFilter, SanctionKind, Source, Store, UserUpdate, status,
};

const DAY: i64 = 86_400_000;

/// Game records with increasing ids.
struct Records(u64);

impl Records {
    fn new() -> Records {
        Records(3_000_000_000_000)
    }

    fn next(&mut self, white: UserId, black: UserId, edit: impl FnOnce(&mut GameRecord)) -> GameRecord {
        self.0 += 1;
        let mut r = record(self.0, white, black);
        r.white_name = format!("p{white}");
        r.black_name = format!("p{black}");
        r.started_at = Some(1000);
        r.ended_at = Some(2000);
        r.moves = vec![12 | (28 << 6)];
        r.spent_ms = Some(vec![0]);
        r.clock_ms = Some(vec![180_000]);
        r.flags = 0;
        edit(&mut r);
        r
    }
}

fn ids(rows: &[GameSummary]) -> Vec<u64> {
    rows.iter().map(|g| g.id).collect()
}

fn rec_ids(rows: &[&GameRecord]) -> Vec<u64> {
    rows.iter().map(|g| g.id).collect()
}

fn filter(category: Option<&str>, rated: Option<bool>, result: Option<ResultFilter>) -> GameFilter {
    GameFilter { category: category.map(Into::into), rated, result }
}

struct History {
    store: Store,
    me: UserId,
    list: Vec<GameRecord>,
}

/// The history fixture: player `me` with games of every kind, against two opponents.
async fn history() -> History {
    let store = memory_store().await;
    let me = store.users().create(new_user("Me", Some("me@e.org"))).await.unwrap();
    let a = store.users().create(new_user("Ann", Some("a@e.org"))).await.unwrap();
    let b = store.users().create(new_user("Ben", Some("b@e.org"))).await.unwrap();
    let mut r = Records::new();
    let list = vec![
        r.next(me, a, |g| g.status = status::WHITE_WINS),
        r.next(a, me, |g| g.status = status::WHITE_WINS),
        r.next(me, b, |g| (g.status, g.reason) = (status::DRAW, 3)),
        r.next(b, me, |g| g.status = status::BLACK_WINS),
        r.next(me, a, |g| (g.status, g.reason, g.rated) = (status::ABORTED, 9, false)),
        r.next(me, b, |g| (g.status, g.rated) = (status::BLACK_WINS, false)),
        r.next(a, me, |g| {
            (g.status, g.category, g.base_ms, g.inc_ms) = (status::DRAW, "5+0".into(), 300_000, 0);
        }),
        r.next(me, a, |g| {
            (g.status, g.category, g.rated, g.base_ms, g.inc_ms) =
                (status::WHITE_WINS, "custom".into(), false, 60_000, 1000);
        }),
        r.next(a, b, |g| g.status = status::WHITE_WINS),
    ];
    store.finish_batch(list.clone()).await.unwrap();
    History { store, me, list }
}

#[tokio::test]
async fn list_for_user_filters_by_category_rated_and_result_from_the_players_side_newest_first() {
    let History { store, me, list } = history().await;
    let games = store.games();
    let l = |f: GameFilter| games.list_for_user(me, None, 100, f);
    let mine: Vec<&GameRecord> = list[..8].iter().rev().collect();
    assert_eq!(
        ids(&l(GameFilter::default()).await.unwrap()),
        rec_ids(&mine),
        "no filter: every game, both colours"
    );
    let three = games.list_for_user(me, None, 3, GameFilter::default()).await.unwrap();
    assert_eq!(ids(&three), rec_ids(&mine[..3]));
    let win = l(filter(None, None, Some(ResultFilter::Win))).await.unwrap();
    assert_eq!(ids(&win), rec_ids(&[&list[7], &list[3], &list[0]]));
    let loss = l(filter(None, None, Some(ResultFilter::Loss))).await.unwrap();
    assert_eq!(ids(&loss), rec_ids(&[&list[5], &list[1]]));
    let draw = l(filter(None, None, Some(ResultFilter::Draw))).await.unwrap();
    assert_eq!(ids(&draw), rec_ids(&[&list[6], &list[2]]));
    let rated = l(filter(None, Some(true), None)).await.unwrap();
    assert_eq!(ids(&rated), rec_ids(&[&list[6], &list[3], &list[2], &list[1], &list[0]]));
    let casual = l(filter(None, Some(false), None)).await.unwrap();
    assert_eq!(ids(&casual), rec_ids(&[&list[7], &list[5], &list[4]]));
    assert_eq!(ids(&l(filter(Some("custom"), None, None)).await.unwrap()), rec_ids(&[&list[7]]));
    let f = filter(Some("5+0"), Some(true), Some(ResultFilter::Draw));
    assert_eq!(ids(&l(f).await.unwrap()), rec_ids(&[&list[6]]));
    let f = filter(Some("3+2"), Some(true), Some(ResultFilter::Win));
    assert_eq!(ids(&l(f).await.unwrap()), rec_ids(&[&list[3], &list[0]]));
    assert!(l(filter(Some("1+0"), None, None)).await.unwrap().is_empty());
    // Aborted games only come without a result filter.
    for result in [ResultFilter::Win, ResultFilter::Loss, ResultFilter::Draw] {
        let rows = l(filter(None, None, Some(result))).await.unwrap();
        assert!(!rows.iter().any(|g| g.status == status::ABORTED), "{result:?}");
    }
    // The rating changes come with the rated games only.
    let rows = l(GameFilter::default()).await.unwrap();
    let first = rows.iter().find(|g| g.id == list[0].id).unwrap();
    let changes = first.rating_changes.unwrap();
    assert_eq!((changes.white.before, changes.white.after, changes.black.after), (1500, 1510, 1490));
    assert!(rows.iter().find(|g| g.id == list[4].id).unwrap().rating_changes.is_none());
    assert_eq!(first.ply_count, 1);
    // Unknown result names are the caller's to refuse.
    assert_eq!(ResultFilter::parse("aborted"), None);
    assert_eq!(ResultFilter::parse("won"), None);
    assert_eq!(ResultFilter::parse("loss"), Some(ResultFilter::Loss));
    store.close().await;
}

#[tokio::test]
async fn list_for_user_pages_with_the_before_cursor_and_count_for_user_counts_every_page() {
    let store = memory_store().await;
    let me = store.users().create(new_user("Pager", Some("p@e.org"))).await.unwrap();
    let op = store.users().create(new_user("Other", Some("o@e.org"))).await.unwrap();
    let mut r = Records::new();
    let list: Vec<GameRecord> = (0..57)
        .map(|i| {
            let rated = i % 3 == 0;
            if i % 2 == 1 {
                let st = if i % 5 == 0 { status::DRAW } else { status::WHITE_WINS };
                r.next(me, op, |g| (g.rated, g.status) = (rated, st))
            } else {
                r.next(op, me, |g| (g.rated, g.status) = (rated, status::WHITE_WINS))
            }
        })
        .collect();
    store.finish_batch(list.clone()).await.unwrap();
    let casual = filter(None, Some(false), None);
    let expected: Vec<&GameRecord> = list.iter().filter(|g| !g.rated).rev().collect();
    let games = store.games();
    let mut seen = Vec::new();
    let mut before = None;
    loop {
        let page = games.list_for_user(me, before, 10, casual.clone()).await.unwrap();
        seen.extend(ids(&page));
        if page.len() < 10 {
            break;
        }
        before = Some(page.last().unwrap().id);
    }
    assert_eq!(seen, rec_ids(&expected));
    assert_eq!(games.count_for_user(me, Some(casual.clone())).await.unwrap(), expected.len() as i64);
    assert_eq!(games.count_for_user(me, None).await.unwrap(), 57, "without a filter: every game");
    assert_eq!(
        games.count_for_user(me, Some(GameFilter::default())).await.unwrap(),
        57,
        "an empty filter too"
    );
    let wins = list
        .iter()
        .filter(|g| (g.white_id == me) == (g.status == status::WHITE_WINS) && g.status != status::DRAW)
        .count() as i64;
    assert_eq!(
        games.count_for_user(me, Some(filter(None, None, Some(ResultFilter::Win)))).await.unwrap(),
        wins
    );
    let draws = list.iter().filter(|g| g.status == status::DRAW && !g.rated).count() as i64;
    let f = filter(None, Some(false), Some(ResultFilter::Draw));
    assert_eq!(games.count_for_user(me, Some(f)).await.unwrap(), draws);
    let op_losses =
        games.count_for_user(op, Some(filter(None, None, Some(ResultFilter::Loss)))).await.unwrap();
    assert_eq!(op_losses, wins);
    assert!(games.list_for_user(me, Some(list[0].id), 100, GameFilter::default()).await.unwrap().is_empty());
    assert!(games.list_for_user(me, None, 0, GameFilter::default()).await.unwrap().is_empty());
    assert!(games.list_for_user(me, None, -5, casual.clone()).await.unwrap().is_empty());
    assert_eq!(
        games.list_for_user(me, None, 500, GameFilter::default()).await.unwrap().len(),
        57,
        "no cap in the store"
    );
    assert_eq!(games.recent_for_user(me, 5, None).await.unwrap().len(), 5);
    store.close().await;
}

fn details(rows: Vec<crate::store::PlanRow>) -> Vec<String> {
    rows.into_iter().map(|r| r.detail).collect()
}

#[tokio::test]
async fn the_history_queries_read_the_per_colour_indexes_never_the_whole_games_table() {
    let store = memory_store().await;
    let int = SqlValue::Integer;
    let list = vec![
        int(1),
        int(1_000_000_000_000_000),
        SqlValue::Text("3+2".into()),
        int(1),
        int(1),
        int(2),
        int(20),
    ];
    let count = vec![int(1), SqlValue::Null, SqlValue::Null, int(0), int(3), int(3)];
    let list_plan = details(store.explain_query_plan(GAMES_FOR_USER_SQL, list).await.unwrap());
    let count_plan = details(store.explain_query_plan(GAMES_COUNT_FOR_USER_SQL, count).await.unwrap());
    let has = |plan: &[String], index: &str, col: &str| {
        let plain = format!("SEARCH games USING INDEX {index} ({col}=?");
        let covering = format!("SEARCH games USING COVERING INDEX {index} ({col}=?");
        plan.iter().any(|d| d.starts_with(&plain) || d.starts_with(&covering))
    };
    for plan in [&list_plan, &count_plan] {
        let text = plan.join("\n");
        assert!(has(plan, "games_white", "white_id"), "{text}");
        assert!(has(plan, "games_black", "black_id"), "{text}");
        assert!(!plan.iter().any(|d| d.starts_with("SCAN games")), "no table scan:\n{text}");
    }
    assert!(
        list_plan.iter().any(|d| d.contains("games_white (white_id=? AND id<?)")),
        "the cursor bounds the range"
    );
    let recent = details(
        store
            .explain_query_plan(
                "SELECT id FROM games WHERE white_id = ?1 AND id < ?2 UNION SELECT id FROM games WHERE black_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
                vec![int(1), int(1 << 40), int(20)],
            )
            .await
            .unwrap(),
    );
    assert!(!recent.iter().any(|d| d.starts_with("SCAN games")), "{}", recent.join("\n"));
    store.close().await;
}

#[tokio::test]
async fn email_change_normalized_lookups_email_taken_for_another_account_nothing_changed_then() {
    let store = memory_store().await;
    let users = store.users();
    let id = users
        .create(crate::store::NewUser { email_verified: true, ..new_user("Mover", Some("old@example.org")) })
        .await
        .unwrap();
    let other = users.create(new_user("Owner", Some("Taken@Example.org"))).await.unwrap();
    let email = |e: &str, verified: Option<bool>| UserUpdate {
        email: Some(Some(e.into())),
        email_verified: verified,
        ..UserUpdate::default()
    };
    assert!(users.update(id, email(" New.Addr@Example.ORG ", Some(true))).await.unwrap());
    let u = users.by_id(id).await.unwrap().unwrap();
    assert_eq!(u.email.as_deref(), Some("New.Addr@Example.ORG"));
    assert_eq!(users.by_email("new.addr@example.org".into()).await.unwrap().unwrap().id, id);
    assert_eq!(
        users.by_email("old@example.org".into()).await.unwrap(),
        None,
        "the old address is free again"
    );
    assert!(
        users.update(id, email("NEW.ADDR@example.org", None)).await.unwrap(),
        "own address, another case"
    );
    let e = users.update(id, email("taken@EXAMPLE.org", Some(false))).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::EmailTaken);
    let u = users.by_id(id).await.unwrap().unwrap();
    assert_eq!(u.email.as_deref(), Some("NEW.ADDR@example.org"), "unchanged after the refusal");
    assert!(u.email_verified);
    assert_eq!(users.by_email("taken@example.org".into()).await.unwrap().unwrap().id, other);
    users.create(new_user("Later", Some("old@example.org"))).await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn tokens_delete_for_user_and_live_for_user() {
    let store = memory_store().await;
    let id = store.users().create(new_user("Tok", Some("t@e.org"))).await.unwrap();
    let other = store.users().create(new_user("Oth", Some("o@e.org"))).await.unwrap();
    let now = 1_900_000_000_000;
    let tokens = store.tokens();
    let make =
        |kind: &str, h: &str, user: UserId, email: Option<&str>, created_at: i64, expires_at: i64| NewToken {
            kind: kind.into(),
            token_hash: sha(h),
            user_id: Some(user),
            data: email.map(|e| json!({"email": e})),
            created_at,
            expires_at,
        };
    tokens.create(make("email_change", "old", id, Some("a@x.org"), now - 3000, now + DAY)).await.unwrap();
    tokens.create(make("email_change", "new", id, Some("b@x.org"), now - 1000, now + DAY)).await.unwrap();
    tokens.create(make("email_change", "expired", id, Some("c@x.org"), now, now - 1)).await.unwrap();
    tokens.create(make("reset", "reset", id, None, now, now + DAY)).await.unwrap();
    tokens.create(make("email_change", "theirs", other, Some("d@x.org"), now, now + DAY)).await.unwrap();

    let email_of = |t: Option<crate::store::Token>| t.and_then(|t| t.data).map(|d| d["email"].clone());
    let live = tokens.live_for_user(id, "email_change".into(), now).await.unwrap().unwrap();
    assert_eq!(live.data, Some(json!({"email": "b@x.org"})), "the newest live token");
    assert_eq!(live.user_id, Some(id));
    assert_eq!(live.kind, "email_change");
    tokens.consume("email_change".into(), sha("new"), now).await.unwrap().unwrap();
    let live = tokens.live_for_user(id, "email_change".into(), now).await.unwrap();
    assert_eq!(email_of(live), Some(json!("a@x.org")), "a consumed token is not live");
    assert_eq!(
        tokens.live_for_user(id, "email_change".into(), now + 2 * DAY).await.unwrap(),
        None,
        "expired"
    );
    assert_eq!(tokens.live_for_user(id, "verify_email".into(), now).await.unwrap(), None, "another kind");

    assert_eq!(
        tokens.delete_for_user(id, "email_change".into()).await.unwrap(),
        3,
        "consumed and expired ones too"
    );
    assert_eq!(tokens.live_for_user(id, "email_change".into(), now).await.unwrap(), None);
    assert_eq!(tokens.get("email_change".into(), sha("old")).await.unwrap(), None);
    assert!(tokens.get("reset".into(), sha("reset")).await.unwrap().is_some(), "other kinds stay");
    let theirs = tokens.live_for_user(other, "email_change".into(), now).await.unwrap();
    assert_eq!(email_of(theirs), Some(json!("d@x.org")), "other users' tokens stay");
    assert_eq!(tokens.delete_for_user(id, "email_change".into()).await.unwrap(), 0);
    let plan = details(
        store
            .explain_query_plan(
                "DELETE FROM tokens WHERE user_id = ? AND kind = ?",
                vec![SqlValue::Integer(id.into()), SqlValue::Text("x".into())],
            )
            .await
            .unwrap(),
    );
    assert!(plan.iter().any(|d| d.contains("tokens_user")), "{}", plan.join("\n"));
    store.close().await;
}

#[tokio::test]
async fn all_for_user_lists_revoked_and_expired_sessions_with_their_ip_newest_first() {
    let store = memory_store().await;
    let id = store.users().create(new_user("Sess", Some("s@e.org"))).await.unwrap();
    let other = store.users().create(new_user("Else", Some("x@e.org"))).await.unwrap();
    let t = 1_900_000_000_000;
    let sessions = store.sessions();
    let make = |user, h: &str, created_at, expires_at, label: Option<&str>, ip: Option<&str>| NewSession {
        user_id: user,
        token_hash: sha(h),
        created_at,
        expires_at,
        idle_expires_at: None,
        client_label: label.map(Into::into),
        ip: ip.map(Into::into),
    };
    let s1 = sessions
        .create(make(id, "1", t, t + DAY, Some("Scacelith 1.0 (Linux)"), Some("203.0.113.5")))
        .await
        .unwrap();
    let s2 = sessions.create(make(id, "2", t + 10, t + 20, None, Some("2001:db8::1"))).await.unwrap();
    let s3 = sessions.create(make(id, "3", t + 20, t + DAY, None, None)).await.unwrap();
    sessions.create(make(other, "4", t, t + DAY, None, None)).await.unwrap();
    sessions.revoke(s3, Some(id), t + 30).await.unwrap();
    let all = sessions.all_for_user(id).await.unwrap();
    assert_eq!(all.iter().map(|s| s.id).collect::<Vec<_>>(), vec![s3, s2, s1]);
    assert_eq!(
        all[0],
        crate::store::SessionInfo {
            id: s3,
            created_at: t + 20,
            last_seen_at: t + 20,
            expires_at: t + DAY,
            idle_expires_at: t + DAY,
            revoked_at: Some(t + 30),
            client_label: None,
            ip: None,
        }
    );
    assert_eq!(all[1].ip.as_deref(), Some("2001:db8::1"));
    assert_eq!(all[2].client_label.as_deref(), Some("Scacelith 1.0 (Linux)"));
    assert_eq!(all[2].revoked_at, None);
    let mut live: Vec<i64> = sessions.list_for_user(id).await.unwrap().iter().map(|s| s.id).collect();
    live.sort_unstable();
    assert_eq!(live, vec![s1, s2], "list_for_user still leaves revoked ones out");
    assert!(sessions.all_for_user(424_242).await.unwrap().is_empty());
    store.close().await;
}

fn sanction(user_id: UserId, kind: SanctionKind, by: &str, at: i64, ends_at: Option<i64>) -> NewSanction {
    NewSanction {
        user_id,
        kind,
        reason: Some("abuse".into()),
        source: Source::Moderator,
        game_id: None,
        starts_at: at,
        ends_at,
        created_by: Some(by.into()),
        created_at: at,
    }
}

#[tokio::test]
async fn security_events_sanctions_lifted_ones_too_and_conduct_events_of_one_user() {
    let store = memory_store().await;
    let id = store.users().create(new_user("Hist", Some("h@e.org"))).await.unwrap();
    let other = store.users().create(new_user("Oth", Some("o@e.org"))).await.unwrap();
    let t = 1_900_000_000_000;
    let ev = |kind: &str, user, ip: Option<&str>, at, detail| NewSecurityEvent {
        kind: kind.into(),
        user_id: Some(user),
        ip: ip.map(Into::into),
        at: Some(at),
        detail,
    };
    store
        .security()
        .insert_batch(vec![
            ev("login", id, Some("203.0.113.1"), t, Some(json!({"method": "password"}))),
            ev("password_changed", id, None, t + 10, None),
            ev("login", other, Some("203.0.113.2"), t + 5, None),
        ])
        .await
        .unwrap();
    let events = store.security().for_user(id, 50).await.unwrap();
    assert_eq!(
        events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        ["password_changed", "login"],
        "newest first"
    );
    assert_eq!(events[1].detail, Some(json!({"method": "password"})));
    assert_eq!(events[1].ip.as_deref(), Some("203.0.113.1"));
    assert_eq!(store.security().for_user(id, 1).await.unwrap().len(), 1, "capped");

    let sanctions = store.sanctions();
    let s1 = sanctions.create(sanction(id, SanctionKind::Warning, "mod-a", t, None)).await.unwrap();
    let s2 = sanctions.create(sanction(id, SanctionKind::Ban, "mod-b", t + 1, Some(t + DAY))).await.unwrap();
    assert!(sanctions.lift(s2, Some("mod-c".into()), t + 2).await.unwrap());
    assert!(!sanctions.lift(s2, Some("mod-d".into()), t + 3).await.unwrap(), "lifted once");
    let list = sanctions.list(id).await.unwrap();
    assert_eq!(list.iter().map(|s| s.id).collect::<Vec<_>>(), vec![s2, s1]);
    assert_eq!(list[0].lifted_at, Some(t + 2));
    assert_eq!(list[0].lifted_by.as_deref(), Some("mod-c"));
    assert_eq!(sanctions.active(id, t + 3).await.unwrap().len(), 1, "the lifted ban is not active");
    assert_eq!(sanctions.active_ban(id, t + 3).await.unwrap(), None);

    let conduct = store.conduct();
    conduct.record(id, ConductKind::Abandon, t).await.unwrap();
    conduct.record(id, ConductKind::NoShow, t + 5).await.unwrap();
    conduct.record(other, ConductKind::Abort, t + 1).await.unwrap();
    let e = |kind, at| ConductEvent { kind, at };
    assert_eq!(
        conduct.for_user(id, 50).await.unwrap(),
        vec![e(ConductKind::NoShow, t + 5), e(ConductKind::Abandon, t)]
    );
    assert_eq!(conduct.for_user(id, 1).await.unwrap(), vec![e(ConductKind::NoShow, t + 5)]);
    assert_eq!(conduct.for_user(other, 50).await.unwrap(), vec![e(ConductKind::Abort, t + 1)]);
    store.close().await;
}

#[tokio::test]
async fn reports_for_reporter_lists_the_reports_a_player_filed_with_the_reported_name_and_outcome() {
    let store = memory_store().await;
    let me = store.users().create(new_user("Rep", Some("r@e.org"))).await.unwrap();
    let x = store.users().create(new_user("Xavier", Some("x@e.org"))).await.unwrap();
    let y = store.users().create(new_user("Yolanda", Some("y@e.org"))).await.unwrap();
    let t = 1_900_000_000_000;
    let report = |from, to, game, category, comment: Option<&str>, weight, at| NewReport {
        reporter_id: from,
        reported_id: to,
        game_id: Some(game),
        category,
        comment: comment.map(Into::into),
        weight,
        at,
    };
    let reports = store.reports();
    let r1 =
        reports.create(report(me, x, 11, ReportCategory::Cheating, Some("engine"), 0.8, t)).await.unwrap();
    let r2 = reports.create(report(me, y, 12, ReportCategory::Abuse, None, 1.0, t + 1)).await.unwrap();
    reports.create(report(x, me, 11, ReportCategory::Other, None, 1.0, t + 2)).await.unwrap();
    assert!(reports.resolve(r1, ReportStatus::Dismissed, Some("mod".into()), t + 3).await.unwrap());
    let filed = reports.for_reporter(me, 50).await.unwrap();
    let summary: Vec<_> =
        filed.iter().map(|r| (r.id, r.reported_name.as_str(), r.category, r.status, r.outcome())).collect();
    assert_eq!(
        summary,
        vec![
            (r2, "Yolanda", ReportCategory::Abuse, ReportStatus::Open, None),
            (r1, "Xavier", ReportCategory::Cheating, ReportStatus::Dismissed, Some(ReportStatus::Dismissed)),
        ]
    );
    assert_eq!(filed[1].comment.as_deref(), Some("engine"));
    assert_eq!(filed[1].game_id, Some(11));
    assert_eq!(filed[1].created_at, t);
    assert_eq!(filed[1].weight, 0.8);
    assert_eq!(reports.for_reporter(me, 1).await.unwrap().len(), 1);
    assert!(reports.for_reporter(y, 50).await.unwrap().is_empty());
    // The name follows the account (anonymized after a deletion).
    store.users().anonymize(y, t + 4).await.unwrap();
    assert_eq!(reports.for_reporter(me, 50).await.unwrap()[0].reported_name, format!("deleted#{y}"));
    store.close().await;
}

#[tokio::test]
async fn refunds_list_for_a_victim_shows_the_refunds_a_player_received() {
    let store = memory_store().await;
    let cheat = store.users().create(new_user("Cheat", Some("c@e.org"))).await.unwrap();
    let victim = store.users().create(new_user("Vic", Some("v@e.org"))).await.unwrap();
    let mut r = Records::new();
    let g = r.next(cheat, victim, |g| (g.status, g.ended_at) = (status::WHITE_WINS, Some(5000)));
    let g2 = r.next(victim, cheat, |g| g.status = status::WHITE_WINS);
    store.finish_batch(vec![g.clone(), g2]).await.unwrap();
    let given = store
        .refunds()
        .apply_for_cheater(CheaterRefunds {
            cheater_id: cheat,
            since: 0,
            now: 6000,
            sanction_id: None,
            source: Source::Auto,
            by: None,
        })
        .await
        .unwrap();
    assert_eq!(given.len(), 1);
    let got = store.refunds().list(RefundScope::Victim(victim), 50).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].game_id, g.id);
    assert_eq!(got[0].points, 10);
    assert_eq!(got[0].victim_name, "Vic");
    assert_eq!(got[0].cheater_name, "Cheat");
    assert!(store.refunds().list(RefundScope::Victim(cheat), 50).await.unwrap().is_empty());
    assert_eq!(store.refunds().list(RefundScope::Cheater(cheat), 50).await.unwrap().len(), 1);
    assert_eq!(store.refunds().list(RefundScope::All, 50).await.unwrap().len(), 1);
    store.close().await;
}
