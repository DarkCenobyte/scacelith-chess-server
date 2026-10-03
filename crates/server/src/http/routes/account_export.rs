//! `POST /api/v1/account/export {password, code?, recoveryCode?}`: everything the server keeps
//! about the signed-in player's account, as one JSON document to save (docs/API.md), with
//! `Content-Disposition: attachment; filename="scacelith-account-<username>.json"`. Owner: auth.
//!
//! Re-authentication as for `POST /account/delete` (the password, plus an authenticator or a
//! recovery code when two-step verification is on): 403 `invalid_password` | `mfa_code_required`
//! | `invalid_code`, 400 `password_not_set`, 429 `too_many_attempts`, 503 `server_busy` / 429
//! `rate_limited` (password hash queue). A store that stays locked answers 503 `busy` "Try again
//! shortly." (`retryAfter` 1). Limits: `account_export` (5 per hour per player, shared; taken
//! first), then those of every re-authentication ([`super::account::reauth_rates_of`]). An
//! `account_exported` security event is recorded.
//!
//! The document ([`EXPORT_FORMAT`], version 1; times are epoch milliseconds):
//!
//! ```text
//! { format, version, exportedAt, server: { name, host }, notes: [plain English],
//!   account: { id, username, email, emailVerified, pendingEmail, mfaEnabled, googleLinked,
//!              googleEmail, hasPassword, acceptChallenges, createdAt, lastLoginAt },
//!   ratings: [{ category, rating, games, wins, draws, losses, peak, provisional, rated,
//!               countedGames, updatedAt }],
//!   ratingRefunds: [{ day, category, points }],
//!   sessions: [{ id, createdAt, lastSeenAt, expiresAt, revokedAt, clientLabel, ip }],
//!   securityEvents: [{ kind, at, ip, detail }],               (newest first)
//!   sanctions: [{ id, kind, reason, source, gameId, startsAt, endsAt, createdAt, liftedAt }],
//!   conduct: [{ kind, at }],
//!   reportsFiled: [{ gameId, reported, category, comment, createdAt, status: 'open'|'closed' }],
//!   games: { total, list: [the summaries of GET /account/games, newest first] } }
//! ```
//!
//! Never in it: the password hash, the MFA secret, the recovery codes, any token or token hash,
//! the anti-cheat's data, the reports made against the player, the identities of moderators
//! (a `moderator_action` event keeps only its action, and only for [`MODERATOR_ACTIONS`]), and
//! other players' private data: rating refunds are added up per UTC day and category (their
//! `rating_refund` events are left out), a filed report is only `open` or `closed`, and an event
//! keeps its IP address only for the kinds of [`IP_KINDS`]. The detail of an event keeps only the
//! fields [`detail_fields`] lists for its kind (`null` for any other kind).

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::account::{credentials_of, credentials_schema, reauth_rates_of};
use super::auth::{bind, ip_of, session_of};
use crate::auth::Auth;
use crate::config::Config;
use crate::http::{Answer, ApiError, AuthMode, Ctx, RateSpec, RouteOpts, Router};
use crate::ids::UserId;
use crate::log::Logger;
use crate::log_warn;
use crate::store::{
    ErrorKind, GameFilter, GameSummary, Refund, RefundScope, SecurityEvent, Store, StoreError, User,
};

/// The `format` of the document.
pub const EXPORT_FORMAT: &str = "scacelith-account-export";
/// The `version` of the document.
pub const EXPORT_VERSION: u32 = 1;

/// Games read per store read.
const GAMES_PAGE: i64 = 500;
/// The most rows of each list.
const ROWS_MAX: i64 = 100_000;
const DAY_MS: i64 = 86_400_000;

/// Moderator actions an exported `moderator_action` event shows (`{action}` only); the others
/// are left out.
pub const MODERATOR_ACTIONS: [&str; 5] = ["ban", "unban", "reset_mfa", "verify_email", "revoke_sessions"];

/// Security event kinds whose IP address the export keeps: what someone signed in to the
/// account, or holding its password and second factor, or a link mailed to its address, did. Any
/// other kind has `ip: null` (its address may be another person's).
pub const IP_KINDS: [&str; 20] = [
    "register",
    "email_verified",
    "login",
    "sso_login",
    "sso_linked",
    "sso_account_created",
    "recovery_code_used",
    "password_reset",
    "password_changed",
    "reauth_failed",
    "mfa_setup_started",
    "mfa_enabled",
    "mfa_disabled",
    "recovery_codes_regenerated",
    "session_revoked",
    "sessions_revoked_all",
    "email_change_requested",
    "email_changed",
    "email_change_refused",
    "account_exported",
];

/// The fields of a security event's detail the export keeps, per event kind (`None`: detail
/// `null`).
pub fn detail_fields(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "login" => &["method"],
        "sso_login" => &["provider"],
        "sso_linked" => &["provider", "method"],
        "sso_account_created" => &["provider"],
        "login_failed" => &["failures"],
        "login_lockout" => &["retryAfterMs"],
        "mfa_failed" => &["attempts"],
        "recovery_code_used" => &["remaining"],
        "reauth_failed" => &["factor"],
        "session_revoked" => &["reason"],
        "sessions_revoked_all" => &["reason"],
        "email_change_refused" => &["reason"],
        "sanction_auto" => &["kind", "gameId", "until"],
        _ => return None,
    })
}

/// The summary of a game in the player's history (`GET /account/games`, owned by the routes
/// module: `account_games::history_summary`): the export lists its games with it.
pub type HistorySummary = fn(&GameSummary, UserId) -> Map<String, Value>;

/// What the export endpoint needs.
#[derive(Clone)]
pub struct ExportRouteDeps {
    /// The configuration (server name and host, retention, limits).
    pub config: Arc<Config>,
    /// The store the document is read from.
    pub store: Store,
    /// The auth service (re-authentication, account view, security event).
    pub auth: Auth,
    /// The summary of a game of `GET /account/games`.
    pub history_summary: HistorySummary,
    /// Where a locked store is reported.
    pub log: Logger,
}

/// The plain English notes of the document.
pub fn export_notes(config: &Config) -> Vec<String> {
    vec![
        format!(
            "This file holds the data {} keeps about your account. Times are milliseconds since 1970-01-01 UTC.",
            config.server_name
        ),
        "Not included, to protect your account: your password (only a one-way hash of it is stored), your two-step verification secret, your recovery codes, and your sign-in and e-mail link tokens.".into(),
        "Not included: the anti-cheat's records (integrity level, anomalies, the analysis of your games and the statistics it uses), the reports other players made about you, and the names of the moderators who acted on your account.".into(),
        "Not included: other players' private data. Your games show the public names and ratings of your opponents. The rating points given back to you after an opponent was found cheating are added up per day, without the games; a report you filed shows only whether it is still open.".into(),
        "IP addresses are given only for what was done while signed in to your account, or with its password, or with a link sent to your address. Failed sign-ins and the requests anyone can make by typing your name or address (a password reset, a registration with your address) are listed without one: it may be another person's.".into(),
        "games.list has a summary of each of your games; the moves of a game are at /api/v1/games/<id> and its PGN at /api/v1/games/<id>/pgn.".into(),
        format!(
            "Security events are kept for {} days, and the IP addresses stored with them and with your sessions for {} days. A session is deleted when it expires, or a day after it was signed out; conduct events (abandoned and aborted games) after 30 days.",
            config.retention_security_days, config.retention_ip_days
        ),
    ]
}

/// A security event as exported, or `None` when it is left out.
pub fn exported_event(e: &SecurityEvent) -> Option<Value> {
    // A detail kept as JSON text is read too; anything but an object counts as none.
    let parsed = match &e.detail {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok(),
        other => other.clone(),
    };
    let d = match parsed {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };
    let detail = match e.kind.as_str() {
        // In ratingRefunds, added up per day.
        "rating_refund" => return None,
        "moderator_action" => {
            let action = d.as_ref()?.get("action").and_then(Value::as_str)?;
            if !MODERATOR_ACTIONS.contains(&action) {
                return None;
            }
            json!({ "action": action })
        }
        kind => match (d, detail_fields(kind)) {
            (Some(d), Some(fields)) => {
                let picked: Map<String, Value> =
                    fields.iter().filter_map(|f| d.get(*f).map(|v| ((*f).to_owned(), v.clone()))).collect();
                Value::Object(picked)
            }
            _ => Value::Null,
        },
    };
    let ip = if IP_KINDS.contains(&e.kind.as_str()) { e.ip.clone() } else { None };
    Some(json!({ "kind": e.kind, "at": e.at, "ip": ip, "detail": detail }))
}

/// Rating refunds added up per UTC day (00:00 UTC, epoch ms) and category, newest day first: the
/// points, without the games they came from.
pub fn refunds_per_day(refunds: &[Refund]) -> Vec<Value> {
    let mut sums: Vec<(i64, &str, i64)> = Vec::new();
    for f in refunds {
        let day = f.created_at.div_euclid(DAY_MS) * DAY_MS;
        match sums.iter_mut().find(|(d, c, _)| *d == day && *c == f.category) {
            Some(sum) => sum.2 += f.points,
            None => sums.push((day, &f.category, f.points)),
        }
    }
    sums.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    sums.into_iter()
        .map(|(day, category, points)| json!({ "day": day, "category": category, "points": points }))
        .collect()
}

/// The file name of a player's export (`scacelith-account-<username>.json`, other characters than
/// `[A-Za-z0-9_.-]` as `_`).
pub fn export_file_name(username: &str) -> String {
    let safe: String = username
        .encode_utf16()
        .map(|u| match char::from_u32(u32::from(u)) {
            Some(c) if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') => c,
            _ => '_',
        })
        .collect();
    format!("scacelith-account-{safe}.json")
}

/// Everything of the document but the account view and the games, from one read.
fn read_records(db: &crate::store::Db, id: UserId) -> Result<Map<String, Value>, StoreError> {
    let google_email = db.sso().for_user(id)?.into_iter().find(|l| l.provider == "google").map(|l| l.email);
    let ratings: Vec<Value> = db
        .ratings()
        .for_user(id)?
        .into_iter()
        .map(|r| {
            json!({
                "category": r.category, "rating": r.record.rating, "games": r.record.games,
                "wins": r.record.wins, "draws": r.record.draws, "losses": r.record.losses,
                "peak": r.record.peak, "provisional": r.provisional, "rated": r.record.rated,
                "countedGames": r.record.counted_games, "updatedAt": r.updated_at,
            })
        })
        .collect();
    let refunds = refunds_per_day(&db.refunds().list(RefundScope::Victim(id), ROWS_MAX)?);
    let sessions: Vec<Value> = db
        .sessions()
        .all_for_user(id)?
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id, "createdAt": r.created_at, "lastSeenAt": r.last_seen_at, "expiresAt": r.expires_at,
                "revokedAt": r.revoked_at, "clientLabel": r.client_label, "ip": r.ip,
            })
        })
        .collect();
    let events: Vec<Value> =
        db.security().for_user(id, ROWS_MAX)?.iter().filter_map(exported_event).collect();
    let sanctions: Vec<Value> = db
        .sanctions()
        .list(id)?
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id, "kind": s.kind.as_str(), "reason": s.reason, "source": s.source.as_str(),
                "gameId": s.game_id.filter(|&g| g != 0), "startsAt": s.starts_at, "endsAt": s.ends_at,
                "createdAt": s.created_at, "liftedAt": s.lifted_at,
            })
        })
        .collect();
    let conduct: Vec<Value> = db
        .conduct()
        .for_user(id, ROWS_MAX)?
        .into_iter()
        .map(|c| json!({ "kind": c.kind.as_str(), "at": c.at }))
        .collect();
    let reports: Vec<Value> = db
        .reports()
        .for_reporter(id, ROWS_MAX)?
        .into_iter()
        .map(|r| {
            json!({
                "gameId": r.game_id.filter(|&g| g != 0), "reported": r.reported_name,
                "category": r.category.as_str(), "comment": r.comment, "createdAt": r.created_at,
                "status": if r.status.as_str() == "open" { "open" } else { "closed" },
            })
        })
        .collect();
    let total = db.games().count_for_user(id, None)?;
    let mut out = Map::new();
    out.insert("googleEmail".into(), json!(google_email.flatten()));
    out.insert("ratings".into(), Value::Array(ratings));
    out.insert("ratingRefunds".into(), Value::Array(refunds));
    out.insert("sessions".into(), Value::Array(sessions));
    out.insert("securityEvents".into(), Value::Array(events));
    out.insert("sanctions".into(), Value::Array(sanctions));
    out.insert("conduct".into(), Value::Array(conduct));
    out.insert("reportsFiled".into(), Value::Array(reports));
    out.insert("total".into(), json!(total));
    Ok(out)
}

/// Builds the export document of `user` (an active account) as of `now`.
pub async fn build_account_export(d: &ExportRouteDeps, user: &User, now: i64) -> Result<Value, StoreError> {
    let id = user.id;
    let account = d.auth.account_view(user).await;
    let mut rec = d.store.read(move |db| read_records(db, id)).await?;
    let mut list = Vec::new();
    let mut before = None;
    loop {
        // One read per page: a long history lets the other readers in between.
        let page = d
            .store
            .read(move |db| db.games().list_for_user(id, before, GAMES_PAGE, &GameFilter::default()))
            .await?;
        list.extend(page.iter().map(|g| Value::Object((d.history_summary)(g, id))));
        match page.last() {
            Some(last) if page.len() as i64 == GAMES_PAGE => before = Some(last.id),
            _ => break,
        }
    }
    let mut take = |key: &str| rec.remove(key).unwrap_or(Value::Null);
    let field = |key: &str| account.get(key).cloned().unwrap_or(Value::Null);
    let flag = |key: &str| Value::Bool(account.get(key).and_then(Value::as_bool).unwrap_or(false));
    Ok(json!({
        "format": EXPORT_FORMAT,
        "version": EXPORT_VERSION,
        "exportedAt": now,
        "server": { "name": d.config.server_name, "host": d.config.server_public_host },
        "notes": export_notes(&d.config),
        "account": {
            "id": id,
            "username": field("username"),
            "email": field("email"),
            "emailVerified": flag("emailVerified"),
            "pendingEmail": field("pendingEmail"),
            "mfaEnabled": flag("mfaEnabled"),
            "googleLinked": flag("googleLinked"),
            "googleEmail": take("googleEmail"),
            "hasPassword": flag("hasPassword"),
            "acceptChallenges": field("acceptChallenges"),
            "createdAt": field("createdAt"),
            "lastLoginAt": field("lastLoginAt"),
        },
        "ratings": take("ratings"),
        "ratingRefunds": take("ratingRefunds"),
        "sessions": take("sessions"),
        "securityEvents": take("securityEvents"),
        "sanctions": take("sanctions"),
        "conduct": take("conduct"),
        "reportsFiled": take("reportsFiled"),
        "games": { "total": take("total"), "list": list },
    }))
}

/// Registers `POST /account/export`.
pub fn register(router: &mut Router, deps: ExportRouteDeps) {
    let [reauth, reauth_user] = reauth_rates_of(&deps.config);
    let d = Arc::new(deps);
    router.post(
        "/account/export",
        RouteOpts::new()
            .auth(AuthMode::Required)
            .rate(RateSpec::new("account_export", 5.0, 3_600_000).by_user().shared())
            .rate(reauth)
            .rate(reauth_user)
            .timeout(std::time::Duration::from_secs(60))
            .body(credentials_schema()),
        bind(&d, export),
    );
}

/// 503 `busy`: the store stayed locked.
fn busy(log: &Logger, err: &dyn std::fmt::Display) -> ApiError {
    log_warn!(log, "account export: store busy", { "err": { "message": err.to_string() } });
    ApiError::new(503, "busy", "Try again shortly.").with_extra("retryAfter", json!(1))
}

async fn export(d: Arc<ExportRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    let ip = ip_of(&ctx);
    let user = match d.auth.reauth_for_export(&session, &credentials_of(&ctx), Some(&ip)).await {
        Ok(user) => user,
        Err(e) if e.is_store_busy() => return Err(busy(&d.log, &e)),
        Err(e) => return Err(e.into()),
    };
    let doc = match build_account_export(&d, &user, ctx.now_ms).await {
        Ok(doc) => doc,
        Err(e) if e.kind() == ErrorKind::Busy => return Err(busy(&d.log, &e)),
        Err(e) => return Err(ApiError::internal(format!("account export: {e}"))),
    };
    d.auth.record_export(user.id, Some(&ip));
    let file = export_file_name(&user.username);
    Ok(Answer::json(doc).header("Content-Disposition", format!("attachment; filename=\"{file}\"")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, ip: Option<&str>, detail: Option<Value>) -> SecurityEvent {
        SecurityEvent { id: 1, kind: kind.into(), user_id: Some(7), ip: ip.map(str::to_owned), at: 5, detail }
    }

    #[test]
    fn events_keep_only_what_the_player_may_see() {
        let e =
            exported_event(&event("login", Some("192.0.2.1"), Some(json!({"method": "password", "x": 1}))));
        assert_eq!(
            e,
            Some(json!({"kind": "login", "at": 5, "ip": "192.0.2.1", "detail": {"method": "password"}}))
        );
        let e = exported_event(&event("login_failed", Some("192.0.2.1"), Some(json!({"failures": 3}))));
        assert_eq!(e, Some(json!({"kind": "login_failed", "at": 5, "ip": null, "detail": {"failures": 3}})));
        let text = Value::String(r#"{"reason":"logout"}"#.into());
        let e = exported_event(&event("session_revoked", None, Some(text)));
        assert_eq!(e.unwrap()["detail"], json!({"reason": "logout"}));
        assert_eq!(exported_event(&event("rating_refund", None, None)), None);
        assert_eq!(exported_event(&event("moderator_action", None, Some(json!({"action": "note"})))), None);
        let e = exported_event(&event(
            "moderator_action",
            Some("x"),
            Some(json!({"action": "ban", "by": "mod"})),
        ));
        assert_eq!(
            e,
            Some(json!({"kind": "moderator_action", "at": 5, "ip": null, "detail": {"action": "ban"}}))
        );
        let e = exported_event(&event("password_reset_requested", None, Some(json!([1]))));
        assert_eq!(e.unwrap()["detail"], Value::Null);
    }

    /// The vectors of the former server's test (account.export.test.js).
    #[test]
    fn event_details_keep_only_the_listed_fields_and_moderator_actions_are_reduced_or_left_out() {
        let at = |kind: &str, at: i64, ip: Option<&str>, detail: Option<Value>| SecurityEvent {
            id: 1,
            kind: kind.into(),
            user_id: Some(7),
            ip: ip.map(str::to_owned),
            at,
            detail,
        };
        let e = at("login", 1, Some("192.0.2.1"), Some(json!({"method": "password", "extra": "x"})));
        assert_eq!(
            exported_event(&e),
            Some(json!({"kind": "login", "at": 1, "ip": "192.0.2.1", "detail": {"method": "password"}}))
        );
        let e = at("some_future_kind", 1, None, Some(json!({"secret": "x"})));
        assert_eq!(
            exported_event(&e),
            Some(json!({"kind": "some_future_kind", "at": 1, "ip": null, "detail": null}))
        );
        let text = |t: &str| Some(Value::String(t.into()));
        let e = at("login", 1, None, text(r#"{"method":"google","x":1}"#));
        assert_eq!(
            exported_event(&e).unwrap()["detail"],
            json!({"method": "google"}),
            "detail stored as JSON text"
        );
        assert_eq!(exported_event(&at("login", 1, None, text("not json"))).unwrap()["detail"], Value::Null);
        let ban = json!({"action": "ban", "moderator": "mod_x", "reason": "r", "hours": 24});
        assert_eq!(
            exported_event(&at("moderator_action", 2, None, Some(ban))),
            Some(json!({"kind": "moderator_action", "at": 2, "ip": null, "detail": {"action": "ban"}}))
        );
        let confirm = json!({"action": "integrity_confirm", "previousLevel": "suspected", "score": 0.8});
        assert_eq!(exported_event(&at("moderator_action", 2, None, Some(confirm))), None);
        assert_eq!(exported_event(&at("moderator_action", 2, None, None)), None);
        // A refund's event would name its game (and so the cheater): ratingRefunds has its points.
        let refund =
            json!({"refundId": 1, "gameId": 9, "cheaterId": 4, "category": "3+2", "points": 7, "by": "mod"});
        assert_eq!(exported_event(&at("rating_refund", 3, None, Some(refund))), None);
        assert_eq!(detail_fields("rating_refund"), None);
        // The IP only for what the account holder did; a future kind has none.
        let ip_of = |kind: &str, detail: Option<Value>| {
            exported_event(&at(kind, 4, Some("198.51.100.7"), detail)).unwrap()["ip"].clone()
        };
        assert_eq!(ip_of("login_failed", Some(json!({"failures": 2}))), Value::Null);
        assert_eq!(ip_of("password_reset_requested", None), Value::Null);
        assert_eq!(ip_of("some_future_kind", None), Value::Null);
        assert_eq!(ip_of("password_changed", None), json!("198.51.100.7"));
        assert!(IP_KINDS.contains(&"login"));
        assert!(!IP_KINDS.contains(&"login_failed") && !IP_KINDS.contains(&"register_existing_email"));
    }

    #[test]
    fn refunds_add_up_per_day_and_category() {
        let refund = |category: &str, points: i64, created_at: i64| Refund {
            id: 1,
            game_id: 2,
            victim_id: 7,
            cheater_id: 8,
            category: category.into(),
            points,
            created_at,
            sanction_id: None,
            source: crate::store::Source::Auto,
            created_by: None,
            notified_at: None,
            victim_name: "v".into(),
            cheater_name: "c".into(),
        };
        let list = [
            refund("3+2", 5, DAY_MS + 10),
            refund("1+0", 2, DAY_MS + 20),
            refund("3+2", 4, DAY_MS + 30),
            refund("3+2", 1, 3 * DAY_MS),
        ];
        assert_eq!(
            refunds_per_day(&list),
            vec![
                json!({"day": 3 * DAY_MS, "category": "3+2", "points": 1}),
                json!({"day": DAY_MS, "category": "1+0", "points": 2}),
                json!({"day": DAY_MS, "category": "3+2", "points": 9}),
            ]
        );
        let list = [
            refund("3+2", 4, DAY_MS * 3 + 5),
            refund("3+2", 6, DAY_MS * 4 - 1),
            refund("1+0", 2, DAY_MS * 3),
            refund("3+2", 1, DAY_MS * 9),
        ];
        assert_eq!(
            refunds_per_day(&list),
            vec![
                json!({"day": DAY_MS * 9, "category": "3+2", "points": 1}),
                json!({"day": DAY_MS * 3, "category": "1+0", "points": 2}),
                json!({"day": DAY_MS * 3, "category": "3+2", "points": 10}),
            ]
        );
    }

    #[test]
    fn file_names() {
        assert_eq!(export_file_name("Alice_9.x-y"), "scacelith-account-Alice_9.x-y.json");
        assert_eq!(export_file_name("a b/c\"é"), "scacelith-account-a_b_c__.json");
        assert_eq!(export_file_name("😀"), "scacelith-account-__.json");
        assert_eq!(export_file_name("Al_ice-9"), "scacelith-account-Al_ice-9.json");
        assert_eq!(export_file_name("a\"b/c"), "scacelith-account-a_b_c.json");
    }
}
