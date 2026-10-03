//! `POST /api/v1/account/export`: the document, its re-authentication and rate limit, and above
//! all what it must never hold (account.export.test.js). The secrets test reads every secret
//! column of the database and searches the document's text for each of them.

use std::collections::BTreeSet;

use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};

use super::{DAY_MS, Harness, PW, link_in, sha256_hex, token_of};
use crate::http::testing::TestResponse;
use crate::ids::{GameId, UserId};
use crate::security::encoding::b64_url;
use crate::security::totp::{base32_decode, totp};
use crate::store::{
    CheaterRefunds, ConductKind, GameRecord, IntegrityLevel, IntegrityUpdate, NewAnomaly, NewReport,
    NewSanction, NewSecurityEvent, PopulationUpdate, RatingRecord, ReportCategory, ReportStatus, Sample,
    SanctionKind, Severity, Source, StoreError, status,
};

const EXPORT: &str = "/api/v1/account/export";

async fn export(h: &Harness, token: &str, body: Value) -> TestResponse {
    h.post_as(token, EXPORT, body).await
}

/// A finished rated 3+2 game, ended half an hour ago.
fn game(h: &Harness, id: GameId, white: (UserId, &str), black: (UserId, &str), result: u8) -> GameRecord {
    GameRecord {
        id,
        category: "3+2".into(),
        rated: true,
        base_ms: 180_000,
        inc_ms: 2_000,
        white_id: white.0,
        black_id: black.0,
        white_name: white.1.into(),
        black_name: black.1.into(),
        white_rating: Some(1500),
        black_rating: Some(1500),
        started_at: Some(h.now() - 3_600_000),
        ended_at: Some(h.now() - 1_800_000),
        status: result,
        reason: 1,
        flags: 1,
        moves: vec![12 | (28 << 6), 52 | (36 << 6)],
        ..GameRecord::default()
    }
}

/// Turns two-step verification on: the secret (base32) and the recovery codes.
async fn enable_mfa(h: &Harness, token: &str) -> (String, Vec<String>) {
    let st = h.post_as(token, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    let encoded = st.json()["secret"].as_str().unwrap().to_owned();
    let code = totp(&base32_decode(&encoded).unwrap(), h.now());
    let en = h.post_as(token, "/api/v1/account/mfa/totp/enable", json!({ "code": code })).await;
    assert_eq!(en.status, 200, "{}", en.text());
    let codes = en.json()["recoveryCodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    (encoded, codes)
}

fn code_of(h: &Harness, encoded: &str) -> String {
    totp(&base32_decode(encoded).unwrap(), h.now())
}

fn sorted_keys(v: &Value) -> Vec<String> {
    let mut keys = super::keys(v);
    keys.sort();
    keys
}

fn kinds(events: &Value) -> Vec<String> {
    events.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_owned()).collect()
}

fn find<'a>(events: &'a Value, kind: &str) -> &'a Value {
    events.as_array().unwrap().iter().find(|e| e["kind"] == kind).unwrap_or_else(|| panic!("no {kind} event"))
}

// ---- the document, re-authentication, rate limit -------------------------------------------------

#[tokio::test]
async fn the_document_format_account_sections_content_disposition_and_account_exported_recorded() {
    let h = Harness::with_env(&[("SERVER_NAME", "Club"), ("SERVER_PUBLIC_HOST", "chess.example.org")]).await;
    let u = h.create_user("alice").await;
    let bob = h.create_user("bob").await;
    let token = h.login_with("alice", PW, json!({ "clientLabel": "Scacelith 1.4 (Windows)" })).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (a, b) = ((u, "alice"), (bob, "bob"));
    let games = vec![
        game(&h, 11, a, b, status::WHITE_WINS),
        game(&h, 12, b, a, status::WHITE_WINS),
        game(&h, 13, a, b, status::DRAW),
    ];
    h.store.finish_batch(games).await.unwrap();
    // The record as the games left it is replaced by a known one.
    let rec = RatingRecord {
        rating: 1612,
        games: 42,
        wins: 20,
        draws: 5,
        losses: 17,
        peak: 1650,
        rated: true,
        counted_games: 42,
        ..RatingRecord::initial(1612)
    };
    let now = h.now();
    h.store
        .write(move |db| {
            db.ratings().put(u, "3+2", &rec, 1234)?;
            db.conduct().record(u, ConductKind::Abort, now - 5000)?;
            db.reports().create(&NewReport {
                reporter_id: u,
                reported_id: bob,
                game_id: Some(12),
                category: ReportCategory::Cheating,
                comment: Some("too strong".into()),
                weight: 1.0,
                at: now - 100,
            })?;
            db.exec(
                "INSERT INTO rating_refunds (game_id, victim_id, cheater_id, category, points, created_at, source, created_by)
                 VALUES (12, ?1, ?2, '3+2', 9, ?3, 'moderator', 'mod_a')",
                rusqlite::params![u, bob, now - 50],
            )
        })
        .await
        .unwrap();
    h.auth.events().flush().await;

    let r = export(&h, &token, json!({ "password": PW })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.header("content-type").unwrap().starts_with("application/json"));
    assert_eq!(
        r.header("content-disposition"),
        Some("attachment; filename=\"scacelith-account-alice.json\"")
    );
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let d = r.json();
    assert_eq!(
        (&d["format"], &d["version"], &d["exportedAt"]),
        (&json!("scacelith-account-export"), &json!(1), &json!(now))
    );
    assert_eq!(d["server"], json!({ "name": "Club", "host": "chess.example.org" }));
    let notes: Vec<&str> = d["notes"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    assert!(notes.len() >= 4);
    assert!(notes.iter().any(|n| n.contains("password") && n.contains("recovery codes")));
    assert!(
        notes.iter().any(|n| n.contains("anti-cheat") && n.contains("reports other players made about you"))
    );
    assert_eq!(
        d["account"],
        json!({
            "id": u, "username": "alice", "email": "alice@example.com", "emailVerified": true, "pendingEmail": null,
            "mfaEnabled": false, "googleLinked": false, "googleEmail": null, "hasPassword": true,
            "acceptChallenges": "all", "createdAt": now, "lastLoginAt": now,
        })
    );
    assert_eq!(
        d["ratings"],
        json!([{
            "category": "3+2", "rating": 1612, "games": 42, "wins": 20, "draws": 5, "losses": 17, "peak": 1650,
            "provisional": false, "rated": true, "countedGames": 42, "updatedAt": 1234,
        }])
    );
    assert_eq!(
        d["ratingRefunds"],
        json!([{ "day": (now - 50).div_euclid(DAY_MS) * DAY_MS, "category": "3+2", "points": 9 }])
    );
    let sessions = d["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sorted_keys(&sessions[0]),
        ["clientLabel", "createdAt", "expiresAt", "id", "ip", "lastSeenAt", "revokedAt"]
    );
    assert_eq!(sessions[0]["clientLabel"], "Scacelith 1.4 (Windows)");
    let events = d["securityEvents"].as_array().unwrap();
    assert!(events.iter().any(|e| e["kind"] == "login" && e["detail"]["method"] == "password"));
    assert_eq!(d["conduct"], json!([{ "kind": "abort", "at": now - 5000 }]));
    assert_eq!(
        d["reportsFiled"],
        json!([{
            "gameId": 12, "reported": "bob", "category": "cheating", "comment": "too strong", "createdAt": now - 100,
            "status": "open",
        }])
    );
    assert_eq!(d["games"]["total"], 3);
    let list: Vec<(Value, Value, Value)> = d["games"]["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["id"].clone(), g["outcome"].clone(), g["color"].clone()))
        .collect();
    assert_eq!(
        list,
        [
            (json!(13), json!("draw"), json!("white")),
            (json!(12), json!("loss"), json!("black")),
            (json!(11), json!("win"), json!("white")),
        ]
    );
    assert_eq!(d["games"]["list"][0]["baseMs"], 180_000);

    let exported: Vec<_> =
        h.events().await.into_iter().filter(|e| e.kind == "account_exported").map(|e| e.user_id).collect();
    assert_eq!(exported, [Some(u)]);
}

#[tokio::test]
async fn re_authentication_like_deletion_and_401_without_a_session() {
    let h = Harness::new().await;
    h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    let r = export(&h, &token, json!({ "password": "nope nope nope" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")));
    let r = export(&h, &token, json!({})).await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_request")));
    assert_eq!(h.post(EXPORT, json!({ "password": PW })).await.status, 401);

    let (secret, codes) = enable_mfa(&h, &token).await;
    h.advance(30_000);
    let r = export(&h, &token, json!({ "password": PW })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("mfa_code_required")));
    let r = export(&h, &token, json!({ "password": PW, "code": "000000" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_code")));
    // Four of the five attempts of the hour (account_export, a sliding hour) are used by now:
    // two hours later they no longer count.
    h.advance(2 * 3_600_000);
    let r = export(&h, &token, json!({ "password": PW, "code": code_of(&h, &secret) })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["account"]["mfaEnabled"], true);
    let r = export(&h, &token, json!({ "password": PW, "recoveryCode": codes[3] })).await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn rate_5_exports_per_hour_and_player_in_a_sliding_hour() {
    let h = Harness::new().await;
    h.create_user("alice").await;
    h.create_user("bob").await;
    let a = h.token("alice", PW).await;
    let b = h.token("bob", PW).await;
    for i in 0..5 {
        assert_eq!(export(&h, &a, json!({ "password": PW })).await.status, 200, "export {i}");
    }
    h.advance(60_000);
    let r = export(&h, &a, json!({ "password": PW })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")));
    assert!(r.header("retry-after").unwrap().parse::<u64>().unwrap() > 0);
    assert_eq!(export(&h, &b, json!({ "password": PW })).await.status, 200, "per player");
    h.advance(13 * 60_000);
    assert_eq!(
        export(&h, &a, json!({ "password": PW })).await.status,
        429,
        "a sliding hour, not a bucket refilled every 12 minutes"
    );
    h.advance(60 * 60_000);
    assert_eq!(export(&h, &a, json!({ "password": PW })).await.status, 200, "again in the next hour");
}

#[tokio::test]
async fn a_long_history_is_exported_whole_newest_first() {
    let h = Harness::new().await;
    let u = h.create_user("alice").await;
    let bob = h.create_user("bob").await;
    let token = h.token("alice", PW).await;
    const N: u64 = 1203;
    let records: Vec<GameRecord> = (1..=N)
        .map(|i| {
            let (white, black) =
                if i % 2 == 1 { ((u, "alice"), (bob, "bob")) } else { ((bob, "bob"), (u, "alice")) };
            let result = if i % 5 == 0 { status::ABORTED } else { status::WHITE_WINS };
            GameRecord {
                category: if i % 3 == 0 { "custom".into() } else { "3+2".into() },
                rated: i % 3 != 0,
                started_at: Some(i as i64),
                ended_at: Some(i as i64 + 1),
                ..game(&h, i, white, black, result)
            }
        })
        .collect();
    h.store.finish_batch(records).await.unwrap();
    let r = export(&h, &token, json!({ "password": PW })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let d = r.json();
    let ids: Vec<u64> =
        d["games"]["list"].as_array().unwrap().iter().map(|g| g["id"].as_u64().unwrap()).collect();
    assert_eq!(d["games"]["total"], N);
    assert_eq!(ids.len() as u64, N);
    assert_eq!(ids[..3], [1203, 1202, 1201]);
    assert_eq!(ids.last(), Some(&1));
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len() as u64, N, "every game once (pages of 500)");
    assert!(d["games"]["list"].as_array().unwrap().iter().any(|g| g["outcome"] == "aborted"));
}

#[tokio::test]
async fn ip_addresses_only_those_of_the_account_holder_not_of_someone_who_typed_the_address_or_name() {
    let h = Harness::with_env(&[("AUTH_FAILURES_PER_ACCOUNT", "3")]).await;
    let u = h.create_user_with("alice", Some("alice@example.com"), Some(PW), true).await;
    const BOB: &str = "198.51.100.77";
    const ALICE: &str = "203.0.113.7";
    let post_from = |ip: &'static str, path: &'static str, body: Value| {
        h.call_from(ip, http::Method::POST, path).json(&body).send()
    };
    // Bob uses Alice's address to register, asks for a reset of her password, guesses it, and
    // gets her password right but not her second factor.
    let signup =
        json!({ "username": "bobby", "email": "alice@example.com", "password": "another fine passphrase" });
    assert_eq!(post_from(BOB, "/api/v1/auth/register", signup).await.status, 202);
    assert_eq!(
        post_from(BOB, "/api/v1/auth/password/forgot", json!({ "email": "alice@example.com" })).await.status,
        202
    );
    for _ in 0..3 {
        let r =
            post_from(BOB, "/api/v1/auth/login", json!({ "login": "alice", "password": "not her password" }))
                .await;
        assert_eq!(r.status, 401);
    }
    h.advance(3_600_000);
    let sign_in = post_from(ALICE, "/api/v1/auth/login", json!({ "login": "alice", "password": PW })).await;
    assert_eq!(sign_in.status, 200, "{}", sign_in.text());
    let token = sign_in.json()["token"].as_str().unwrap().to_owned();
    let as_alice = |path: &'static str, body: Value| {
        h.call_from(ALICE, http::Method::POST, path).bearer(&token).json(&body).send()
    };
    let st = as_alice("/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    let secret = st.json()["secret"].as_str().unwrap().to_owned();
    let en = as_alice("/api/v1/account/mfa/totp/enable", json!({ "code": code_of(&h, &secret) })).await;
    assert_eq!(en.status, 200);
    let r = post_from(BOB, "/api/v1/auth/login", json!({ "login": "alice", "password": PW })).await;
    let mfa_token = r.json()["mfaToken"].as_str().expect("an MFA step").to_owned();
    let r =
        post_from(BOB, "/api/v1/auth/login/mfa", json!({ "mfaToken": mfa_token, "code": "000000" })).await;
    assert_eq!(r.status, 401);
    h.advance(30_000);
    let stored: BTreeSet<String> = h
        .events()
        .await
        .into_iter()
        .filter(|e| e.user_id == Some(u) && e.ip.as_deref() == Some(BOB))
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        stored.into_iter().collect::<Vec<_>>(),
        [
            "login_failed",
            "login_lockout",
            "mfa_failed",
            "password_reset_requested",
            "register_existing_email"
        ]
    );

    let r = as_alice(EXPORT, json!({ "password": PW, "code": code_of(&h, &secret) })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(!r.text().contains(BOB), "no IP address of another person");
    let d = r.json();
    let ev = &d["securityEvents"];
    for kind in
        ["register_existing_email", "password_reset_requested", "login_failed", "login_lockout", "mfa_failed"]
    {
        assert_eq!(find(ev, kind)["ip"], Value::Null, "{kind}");
    }
    assert_eq!(find(ev, "login_failed")["detail"], json!({ "failures": 3 }), "the rest of the event stays");
    for kind in ["login", "mfa_setup_started", "mfa_enabled"] {
        assert_eq!(find(ev, kind)["ip"], ALICE, "{kind}");
    }
    assert!(d["sessions"].as_array().unwrap().iter().all(|s| s["ip"] == ALICE));
    assert!(d["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("IP address")));
}

// ---- no secret, no anti-cheat data, no moderator identity -----------------------------------------

/// Every value of `column` in the rows `sql` selects.
async fn column(h: &Harness, sql: &'static str, id: UserId) -> Vec<SqlValue> {
    h.store.read(move |db| db.all(sql, [id], |r| r.get::<_, SqlValue>(0))).await.unwrap()
}

/// The forbidden texts, each with its label.
#[derive(Default)]
struct Forbidden(Vec<(String, String)>);

impl Forbidden {
    fn add(&mut self, label: &str, v: impl Into<String>) {
        self.0.push((label.to_owned(), v.into()));
    }

    fn add_sql(&mut self, label: &str, v: &SqlValue) {
        match v {
            SqlValue::Null => {}
            SqlValue::Text(t) => self.add(label, t.clone()),
            SqlValue::Blob(b) => {
                self.add(&format!("{label} (hex)"), hex::encode(b));
                self.add(&format!("{label} (base64url)"), b64_url(b));
            }
            SqlValue::Integer(i) => self.add(label, i.to_string()),
            SqlValue::Real(f) => self.add(label, f.to_string()),
        }
    }
}

#[tokio::test]
async fn an_export_never_holds_a_secret_the_anti_cheats_data_reports_against_the_player_or_a_moderators_identity()
 {
    let h = Harness::new().await;
    let uid = h.create_user_with("Alice", Some("alice@example.org"), Some(PW), true).await;
    let cheater = h.create_user_with("Mallory", Some("mallory@example.org"), Some(PW), true).await;
    let rita = h.create_user_with("Rita_Reporter", Some("rita@example.org"), Some(PW), true).await;
    let victim = h.create_user_with("Victor", Some("victor@example.org"), Some(PW), true).await;

    // Sessions: three logins, one of them signed out.
    let login = async |label: &str| {
        h.login_with("Alice", PW, json!({ "clientLabel": label })).await["token"].as_str().unwrap().to_owned()
    };
    let token = login("desk").await;
    let laptop = login("laptop").await;
    let phone = login("phone").await;
    assert_eq!(h.post_as(&phone, "/api/v1/auth/logout", json!({})).await.status, 200);

    // Two-step verification with its secret and recovery codes.
    let (secret, recovery_codes) = enable_mfa(&h, &token).await;
    h.advance(30_000);

    // A password reset link and a pending e-mail change link, both live.
    h.post("/api/v1/auth/password/forgot", json!({ "email": "alice@example.org" })).await;
    let mails = h.sent().await;
    let reset_mail = mails.iter().find(|m| m.subject.contains("Reset")).expect("the reset mail");
    let reset_token = token_of(&link_in(&reset_mail.text).unwrap()).unwrap();
    let change =
        json!({ "newEmail": "alice.new@example.org", "password": PW, "recoveryCode": recovery_codes[0] });
    let r = h.post_as(&token, "/api/v1/account/email", change).await;
    assert_eq!(r.status, 202, "{}", r.text());
    let mails = h.sent().await;
    let change_mail = mails.iter().find(|m| m.to == "alice.new@example.org").expect("the change mail");
    let change_token = token_of(&link_in(&change_mail.text).unwrap()).unwrap();

    // Games: Alice loses a rated game to the cheater, then the cheater's victims are refunded by
    // a moderator (the refunds and their rating_refund events, as the anti-cheat writes them).
    let (a, m, v) = ((uid, "Alice"), (cheater, "Mallory"), (victim, "Victor"));
    let lost = game(&h, 7_000_000_000_001, m, a, status::WHITE_WINS);
    let lost_id = lost.id;
    let games = vec![
        lost,
        game(&h, 7_000_000_000_002, a, v, status::DRAW),
        game(&h, 7_000_000_000_003, v, m, status::BLACK_WINS),
    ];
    h.store.finish_batch(games).await.unwrap();
    let now = h.now();
    let sanction =
        |user_id: UserId, kind: SanctionKind, reason: &str, by: &str, ends_at: Option<i64>| NewSanction {
            user_id,
            kind,
            reason: Some(reason.into()),
            source: Source::Moderator,
            game_id: None,
            starts_at: now - 1000,
            ends_at,
            created_by: Some(by.into()),
            created_at: now - 1000,
        };
    let ban = sanction(cheater, SanctionKind::Ban, "confirmed: engine", "mod_banhammer", None);
    let block = sanction(uid, SanctionKind::MmBlock, "abandons", "mod_morgana", Some(now + 3_600_000));
    let warning = sanction(uid, SanctionKind::Warning, "chat", "mod_morgana", None);
    let given = h
        .store
        .write(move |db| {
            let sanction_id = db.sanctions().create(&ban)?;
            let given = db.refunds().apply_for_cheater(&CheaterRefunds {
                cheater_id: cheater,
                since: 0,
                now,
                sanction_id: Some(sanction_id),
                source: Source::Moderator,
                by: Some("mod_refunder".into()),
            })?;
            let refund_events: Vec<NewSecurityEvent> = given
                .iter()
                .map(|g| NewSecurityEvent {
                    kind: "rating_refund".into(),
                    user_id: Some(g.victim_id),
                    ip: None,
                    at: Some(now),
                    detail: Some(json!({
                        "refundId": g.id, "gameId": g.game_id, "cheaterId": cheater, "category": g.category,
                        "points": g.points, "sanctionId": sanction_id, "source": "moderator", "by": "mod_refunder",
                    })),
                })
                .collect();
            db.security().insert_batch(&refund_events)?;
            // Sanctions of Alice (one lifted), with moderator identities.
            db.sanctions().create(&block)?;
            let lifted = db.sanctions().create(&warning)?;
            db.sanctions().lift(lifted, Some("mod_lifterson"), now)?;
            db.conduct().record(uid, ConductKind::Abandon, now - 5000)?;
            // Security events written by moderators and the anti-cheat about Alice.
            let event = |kind: &str, at: i64, detail: Value| NewSecurityEvent {
                kind: kind.into(),
                user_id: Some(uid),
                ip: None,
                at: Some(at),
                detail: Some(detail),
            };
            db.security().insert_batch(&[
                event(
                    "moderator_action",
                    now - 4000,
                    json!({ "action": "reset_mfa", "moderator": "mod_morgana", "revokedSessions": 2 }),
                ),
                event(
                    "moderator_action",
                    now - 3000,
                    json!({
                        "action": "integrity_clear", "moderator": "mod_morgana", "reason": "INTEGRITY-CLEAR-REASON",
                        "previousLevel": "high_confidence", "score": 0.914273, "reportsDismissed": 3,
                    }),
                ),
                event(
                    "sanction_auto",
                    now - 2000,
                    json!({ "kind": "mm_block", "gameId": lost_id, "until": now + 1000, "signal": "AUTO-SIGNAL-DETAIL" }),
                ),
            ])?;
            // Reports: one filed by Alice, one received.
            let report = |reporter_id, reported_id, category, comment: &str, weight| NewReport {
                reporter_id,
                reported_id,
                game_id: Some(lost_id),
                category,
                comment: Some(comment.into()),
                weight,
                at: now - 500,
            };
            db.reports().create(&report(uid, cheater, ReportCategory::Cheating, "engine moves", 0.7317))?;
            db.reports().create(&report(rita, uid, ReportCategory::Other, "RECEIVED-REPORT-COMMENT-42", 0.6193))?;
            // The anti-cheat's own data about Alice.
            db.integrity().set(
                uid,
                &IntegrityUpdate {
                    level: Some(IntegrityLevel::HighConfidence),
                    score: Some(0.873311),
                    evidence: Some(Some(json!({ "why": "EVIDENCE-TEXT-77" }))),
                    note: Some(Some("MODNOTE-55".into())),
                    reviewed_by: Some(Some("mod_reviewer".into())),
                    ..IntegrityUpdate::default()
                },
            )?;
            db.anomalies().insert_batch(&[NewAnomaly {
                user_id: Some(uid),
                game_id: Some(lost_id),
                kind: "move_time_uniformity_TEST".into(),
                severity: Severity::Suspicious,
                at: Some(now),
                detail: Some(json!({ "z": "ANOMALY-DETAIL-31" })),
            }])?;
            let sample = Sample::Values(vec![1.0, 2.0, 3.0]);
            db.integrity().update_population(&[PopulationUpdate { key: "3+2|1500|POPMETRIC_TEST".into(), sample }], now)?;
            Ok::<_, StoreError>(given)
        })
        .await
        .unwrap();
    let refunded: i64 = given.iter().filter(|g| g.victim_id == uid).map(|g| g.points).sum();
    assert_eq!(refunded, 10, "Alice is refunded the game she lost to the cheater");
    h.auth.events().flush().await;

    // The export.
    let r = export(&h, &laptop, json!({ "password": PW, "code": code_of(&h, &secret) })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let text = r.text().to_owned();
    let d = r.json();

    // What it holds.
    assert_eq!(d["account"]["username"], "Alice");
    assert_eq!(d["account"]["mfaEnabled"], true);
    assert_eq!(d["account"]["pendingEmail"], "alice.new@example.org");
    let sessions = d["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 3);
    assert_eq!(sessions.iter().filter(|s| !s["revokedAt"].is_null()).count(), 1);
    let mut labels: Vec<&str> = sessions.iter().map(|s| s["clientLabel"].as_str().unwrap()).collect();
    labels.sort();
    assert_eq!(labels, ["desk", "laptop", "phone"]);
    assert!(sessions.iter().all(|s| s["ip"] == "203.0.113.10"));
    assert_eq!(d["games"]["total"], 2);
    let mut outcomes: Vec<&str> =
        d["games"]["list"].as_array().unwrap().iter().map(|g| g["outcome"].as_str().unwrap()).collect();
    outcomes.sort();
    assert_eq!(outcomes, ["draw", "loss"]);
    let refunds = d["ratingRefunds"].as_array().unwrap();
    assert_eq!(refunds.len(), 1);
    assert_eq!(sorted_keys(&refunds[0]), ["category", "day", "points"]);
    assert_eq!(refunds[0]["points"], refunded);
    let sanctions = d["sanctions"].as_array().unwrap();
    assert_eq!(sanctions.len(), 2);
    assert!(sanctions.iter().any(|s| !s["liftedAt"].is_null()));
    assert!(sanctions.iter().all(|s| s.get("createdBy").is_none() && s.get("liftedBy").is_none()));
    assert_eq!(kinds(&d["conduct"]), ["abandon"]);
    assert_eq!(
        d["reportsFiled"],
        json!([{
            "gameId": lost_id, "reported": "Mallory", "category": "cheating", "comment": "engine moves",
            "createdAt": now - 500, "status": "open",
        }])
    );
    let ev = &d["securityEvents"];
    let k = kinds(ev);
    for kind in [
        "login",
        "mfa_enabled",
        "recovery_code_used",
        "email_change_requested",
        "sanction_auto",
        "moderator_action",
    ] {
        assert!(k.iter().any(|x| x == kind), "security event {kind}");
    }
    assert!(!k.iter().any(|x| x == "rating_refund"), "a refund is in ratingRefunds, without its game");
    let moderator: Vec<&Value> = ev
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "moderator_action")
        .map(|e| &e["detail"])
        .collect();
    assert_eq!(moderator, [&json!({ "action": "reset_mfa" })]);
    assert_eq!(find(ev, "login")["detail"], json!({ "method": "password" }));
    assert_eq!(find(ev, "recovery_code_used")["detail"], json!({ "remaining": 9 }));
    assert_eq!(sorted_keys(&find(ev, "sanction_auto")["detail"]), ["gameId", "kind", "until"]);

    // What it must never hold: every secret column of the database...
    let mut forbidden = Forbidden::default();
    let users = column(&h, "SELECT password_hash FROM users WHERE id = ?1", uid).await;
    let mfa = column(&h, "SELECT mfa_secret_enc FROM users WHERE id = ?1", uid).await;
    assert!(users[0] != SqlValue::Null && mfa[0] != SqlValue::Null, "a password hash and an MFA secret");
    forbidden.add_sql("password hash", &users[0]);
    forbidden.add_sql("MFA secret (encrypted)", &mfa[0]);
    for v in column(&h, "SELECT mfa_pending_secret_enc FROM users WHERE id = ?1", uid).await {
        forbidden.add_sql("MFA pending secret", &v);
    }
    let codes = column(&h, "SELECT code_hash FROM mfa_recovery_codes WHERE user_id = ?1", uid).await;
    assert_eq!(codes.len(), 9);
    codes.iter().for_each(|c| forbidden.add_sql("recovery code hash", c));
    let session_hashes = column(&h, "SELECT token_hash FROM sessions WHERE user_id = ?1", uid).await;
    assert_eq!(session_hashes.len(), 3);
    session_hashes.iter().for_each(|x| forbidden.add_sql("session token hash", x));
    let token_kinds = column(&h, "SELECT kind FROM tokens WHERE user_id = ?1 ORDER BY kind", uid).await;
    assert_eq!(token_kinds, [SqlValue::Text("email_change".into()), SqlValue::Text("password_reset".into())]);
    for x in column(&h, "SELECT token_hash FROM tokens WHERE user_id = ?1", uid).await {
        forbidden.add_sql("token hash", &x);
    }
    // ...the secrets the player was given...
    for t in [&token, &laptop, &phone] {
        forbidden.add("session token", t.as_str());
    }
    forbidden.add("reset token", reset_token.as_str());
    forbidden.add("e-mail change token", change_token.as_str());
    forbidden.add("reset token hash", sha256_hex(&reset_token));
    forbidden.add("MFA secret (base32)", secret.as_str());
    forbidden.add("MFA secret (hex)", hex::encode(base32_decode(&secret).unwrap()));
    for c in &recovery_codes {
        forbidden.add("recovery code", c.as_str());
    }
    forbidden.add("password", PW);
    // ...the anti-cheat's data and the reports against Alice...
    let integrity = h
        .store
        .read(move |db| {
            db.all("SELECT level FROM player_integrity WHERE user_id = ?1", [uid], |r| r.get::<_, String>(0))
        })
        .await
        .unwrap();
    assert_eq!(integrity, ["high_confidence"]);
    for v in [
        "high_confidence",
        "0.873311",
        "EVIDENCE-TEXT-77",
        "MODNOTE-55",
        "mod_reviewer",
        "integrity_clear",
        "INTEGRITY-CLEAR-REASON",
        "0.914273",
        "move_time_uniformity_TEST",
        "ANOMALY-DETAIL-31",
        "suspicious",
        "POPMETRIC_TEST",
        "RECEIVED-REPORT-COMMENT-42",
        "Rita_Reporter",
        "0.6193",
        "0.7317",
        "AUTO-SIGNAL-DETAIL",
        "reportsDismissed",
        "previousLevel",
        "cheaterId",
        "refundId",
    ] {
        forbidden.add("anti-cheat / report", v);
    }
    // ...and the moderators.
    for v in ["mod_morgana", "mod_lifterson", "mod_refunder", "mod_banhammer"] {
        forbidden.add("moderator", v);
    }
    // And no field that would carry one (as JSON keys: the notes name some of these in words).
    for v in [
        "passwordHash",
        "password_hash",
        "tokenHash",
        "token_hash",
        "mfaSecret",
        "secretEnc",
        "recoveryCodes",
        "codeHash",
        "integrity",
        "level",
        "score",
        "evidence",
        "weight",
        "anomalies",
        "features",
        "createdBy",
        "liftedBy",
        "resolvedBy",
        "reviewedBy",
        "moderator",
        "by",
    ] {
        forbidden.add("field", format!("\"{v}\":"));
    }
    assert!(forbidden.0.len() > 40);
    let found: Vec<&(String, String)> =
        forbidden.0.iter().filter(|(_, v)| v.len() >= 4 && text.contains(v.as_str())).collect();
    assert!(found.is_empty(), "forbidden data in the export: {found:?}");

    // The pieces it leaves out are really in the database (the search above is not vacuous).
    let count = |sql: &'static str| h.store.read(move |db| db.count(sql, [uid]));
    assert_eq!(count("SELECT count(*) FROM anomalies WHERE user_id = ?1").await.unwrap(), 1);
    assert_eq!(count("SELECT count(*) FROM reports WHERE reported_id = ?1").await.unwrap(), 1);
    let moderator_events =
        count("SELECT count(*) FROM security_events WHERE user_id = ?1 AND kind = 'moderator_action'")
            .await
            .unwrap();
    assert_eq!(moderator_events, 2);
    let refund_events =
        count("SELECT count(*) FROM security_events WHERE user_id = ?1 AND kind = 'rating_refund'");
    assert_eq!(refund_events.await.unwrap(), 1);
}

#[tokio::test]
async fn rating_refunds_and_the_outcomes_of_reports_never_tell_which_opponent_was_sanctioned() {
    let h = Harness::new().await;
    let uid = h.create_user_with("Alice", Some("alice@example.org"), Some(PW), true).await;
    let mallory = h.create_user_with("Mallory", Some("mallory@example.org"), Some(PW), true).await;
    let victor = h.create_user_with("Victor", Some("victor@example.org"), Some(PW), true).await;
    let token = h.token("Alice", PW).await;
    let (a, m, v) = ((uid, "Alice"), (mallory, "Mallory"), (victor, "Victor"));
    let lost_to_m = game(&h, 7_100_000_000_001, m, a, status::WHITE_WINS);
    let lost_to_v = game(&h, 7_100_000_000_002, v, a, status::WHITE_WINS);
    let lost_to_m2 = game(&h, 7_100_000_000_003, a, m, status::BLACK_WINS);
    let ids = [lost_to_m.id, lost_to_v.id, lost_to_m2.id];
    h.store.finish_batch(vec![lost_to_m, lost_to_v, lost_to_m2]).await.unwrap();
    // Mallory is banned for cheating: Alice gets back the points of her two games against her.
    let at = h.now();
    let given = h
        .store
        .write(move |db| {
            let sanction_id = db.sanctions().create(&NewSanction {
                user_id: mallory,
                kind: SanctionKind::Ban,
                reason: Some("confirmed: engine".into()),
                source: Source::Moderator,
                game_id: None,
                starts_at: at,
                ends_at: None,
                created_by: Some("mod_x".into()),
                created_at: at,
            })?;
            let given = db.refunds().apply_for_cheater(&CheaterRefunds {
                cheater_id: mallory,
                since: 0,
                now: at,
                sanction_id: Some(sanction_id),
                source: Source::Moderator,
                by: Some("mod_x".into()),
            })?;
            let events: Vec<NewSecurityEvent> = given
                .iter()
                .map(|g| NewSecurityEvent {
                    kind: "rating_refund".into(),
                    user_id: Some(g.victim_id),
                    ip: None,
                    at: Some(at),
                    detail: Some(json!({ "refundId": g.id, "gameId": g.game_id, "cheaterId": mallory, "points": g.points })),
                })
                .collect();
            db.security().insert_batch(&events)?;
            // Alice's reports: the one on Mallory was actioned, the one on Victor dismissed.
            for (reported, game_id, comment, outcome) in
                [(mallory, ids[0], "engine", ReportStatus::Actioned), (victor, ids[1], "fast", ReportStatus::Dismissed)]
            {
                let id = db.reports().create(&NewReport {
                    reporter_id: uid,
                    reported_id: reported,
                    game_id: Some(game_id),
                    category: ReportCategory::Cheating,
                    comment: Some(comment.into()),
                    weight: 0.5,
                    at,
                })?;
                db.reports().resolve(id, outcome, Some("mod_x"), at)?;
            }
            Ok::<_, StoreError>(given)
        })
        .await
        .unwrap();
    let mut refunded: Vec<GameId> = given.iter().filter(|g| g.victim_id == uid).map(|g| g.game_id).collect();
    refunded.sort();
    assert_eq!(refunded, [ids[0], ids[2]]);
    let points: i64 = given.iter().filter(|g| g.victim_id == uid).map(|g| g.points).sum();
    assert_eq!(points, 20);
    h.auth.events().flush().await;

    let r = export(&h, &token, json!({ "password": PW })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let mut d = r.json();
    assert_eq!(d["games"]["total"], 3);
    // The points given back, added up per UTC day and category (as the game's notice gives them).
    assert_eq!(
        d["ratingRefunds"],
        json!([{ "day": at - at.rem_euclid(DAY_MS), "category": "3+2", "points": points }])
    );
    assert!(
        !kinds(&d["securityEvents"]).iter().any(|k| k == "rating_refund"),
        "refund events are in ratingRefunds"
    );
    // A filed report is open or closed; whether the reported player was sanctioned is not said.
    let mut filed: Vec<(String, String)> = d["reportsFiled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["reported"].as_str().unwrap().to_owned(), x["status"].as_str().unwrap().to_owned()))
        .collect();
    filed.sort();
    assert_eq!(
        filed,
        [("Mallory".to_owned(), "closed".to_owned()), ("Victor".to_owned(), "closed".to_owned())]
    );
    assert!(!r.text().contains("actioned") && !r.text().contains("dismissed"));
    // No refund field, and nothing outside the game list and Alice's own reports, names a game.
    d["games"] = Value::Null;
    d["reportsFiled"] = Value::Null;
    let rest = d.to_string();
    let named: Vec<&GameId> = ids.iter().filter(|id| rest.contains(&id.to_string())).collect();
    assert!(named.is_empty(), "no refunded game in the refunds or the events: {named:?}");
}
