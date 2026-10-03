//! The account commands (`user show|ban|unban|reset-mfa|verify-email|revoke-sessions`) and the
//! bench accounts of test servers.

use std::fs::{OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use serde_json::json;

use super::rows::{self, public_user};
use super::text::{iso, table};
use super::{
    Ctx, DAY_MS, Failure, HOUR_MS, Output, Record, current_euid, refuse, report_counts, report_weight_30d,
    require_user, signup_instead,
};
use crate::anticheat::num::json_num;
use crate::anticheat::refunds::reason;
use crate::store::{
    Db, ErrorKind, NewSanction, NewSecurityEvent, NewSession, NewUser, SanctionKind, Severity, Source,
    UserUpdate,
};
use crate::util::js;

/// `user show <name>`: the account, its ratings, sanctions and integrity summary; the pending
/// signup that holds the name when no account has it.
pub(super) async fn user_show(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    ctx.store
        .read(move |db| {
            let name = c.positional(2);
            if let Some(p) = signup_instead(db, name, c.now)? {
                let link = if p.token_hash.is_some() {
                    "link stored (user verify-email creates the account, as the link would)"
                } else {
                    "no link: the address had an account, whose owner got a notice"
                };
                let text = format!(
                    "Pending signup {} (no account yet)\n  e-mail {}, {link}\n  signed up {}, holds the name until {}\n",
                    p.username,
                    p.email,
                    iso(Some(p.created_at)),
                    iso(Some(p.expires_at))
                );
                return Ok(Output::new(json!({ "pendingSignup": rows::signup(&p) }), text));
            }
            let u = require_user(db, name)?;
            let now = c.now;
            let ratings = db.ratings().for_user(u.id).unwrap_or_default();
            let sanctions = db.sanctions().list(u.id).unwrap_or_default();
            let active_ban = db.sanctions().active_ban(u.id, now).ok().flatten();
            let integ = Record::read_or_default(db, u.id);
            let live = |t: i64| t == 0 || t > now;
            let sessions = db
                .sessions()
                .list_for_user(u.id)
                .unwrap_or_default()
                .iter()
                .filter(|s| s.revoked_at.is_none() && live(s.expires_at) && live(s.idle_expires_at))
                .count();
            let (mut info, mut suspicious, mut certain) = (0, 0, 0);
            for a in db.anomalies().for_user(u.id, 1000).unwrap_or_default() {
                match a.severity {
                    Severity::Info => info += 1,
                    Severity::Suspicious => suspicious += 1,
                    Severity::Certain => certain += 1,
                }
            }
            let counts = report_counts(db, u.id);
            let weight = report_weight_30d(db, u.id, now);
            let data = json!({
                "user": public_user(&u),
                "ratings": ratings.iter().map(rows::rating).collect::<Vec<_>>(),
                "activeBan": active_ban.as_ref().map(rows::sanction),
                "sanctions": sanctions.iter().map(rows::sanction).collect::<Vec<_>>(),
                "integrity": {
                    "level": integ.level.as_str(),
                    "score": json_num(integ.score),
                    "updatedAt": integ.updated_at,
                    "reviewedBy": integ.reviewed_by,
                },
                "activeSessions": sessions,
                "anomalies": { "info": info, "suspicious": suspicious, "certain": certain },
                "reports": { "total": counts.total, "open": counts.open, "weight30d": json_num(weight) },
            });
            let mut t = format!("User {} (#{})  {}\n", u.username, u.id, u.status.as_str());
            t += &format!(
                "  e-mail {} ({}), MFA {}\n",
                u.email.as_deref().filter(|e| !e.is_empty()).unwrap_or("-"),
                if u.email_verified { "verified" } else { "NOT verified" },
                if u.mfa_enabled { "on" } else { "off" }
            );
            t += &format!(
                "  created {}, last login {}, active sessions {sessions}\n",
                iso(Some(u.created_at)),
                iso(u.last_login_at)
            );
            t += &format!(
                "  integrity {} (score {}){}\n",
                integ.level.as_str(),
                js::to_fixed(integ.score, 2),
                integ.reviewed_by.as_deref().map(|b| format!(", reviewed by {b}")).unwrap_or_default()
            );
            t += &format!(
                "  anomalies: {certain} certain, {suspicious} suspicious, {info} info; reports received: {} ({} open)\n",
                counts.total, counts.open
            );
            t += &match &active_ban {
                Some(b) => format!("  BANNED until {}: {}\n", iso(b.ends_at), b.reason.as_deref().unwrap_or("null")),
                None => "  not banned\n".into(),
            };
            let rating_rows: Vec<Vec<String>> = ratings
                .iter()
                .map(|r| {
                    let rec = &r.record;
                    vec![
                        r.category.clone(),
                        rec.rating.to_string(),
                        rec.games.to_string(),
                        format!("{}/{}/{}", rec.wins, rec.draws, rec.losses),
                        rec.peak.to_string(),
                    ]
                })
                .collect();
            t += "\nRatings\n";
            t += &table(&["category", "rating", "games", "w/d/l", "peak"], &rating_rows);
            let sanction_rows: Vec<Vec<String>> = sanctions
                .iter()
                .map(|x| {
                    vec![
                        x.id.to_string(),
                        x.kind.as_str().into(),
                        x.source.as_str().into(),
                        iso(Some(x.starts_at)),
                        iso(x.ends_at),
                        x.reason.clone().unwrap_or_else(|| "-".into()),
                        x.created_by.clone().unwrap_or_else(|| "-".into()),
                        x.lifted_at.filter(|t| *t != 0).map(|t| iso(Some(t))).unwrap_or_default(),
                    ]
                })
                .collect();
            t += "\nSanctions\n";
            t += &table(&["id", "kind", "source", "from", "until", "reason", "by", "lifted"], &sanction_rows);
            Ok(Output::new(data, t))
        })
        .await
}

/// `user ban <name> --hours N --reason TEXT [--revoke-sessions]`: a ban for anything but
/// cheating, which refunds nothing.
pub(super) async fn user_ban(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    ctx.store
        .write(move |db| {
            let u = require_user(db, c.positional(2))?;
            let hours = c.int_flag("hours", 1, 87_600, None)?;
            let why = c.text_flag("reason", true, 300)?;
            // The reason of a ban tells whether it was given for cheating (refunds::reason): this
            // ban is not, and refunds nothing.
            if why.starts_with(reason::CONFIRMED) || why.starts_with(reason::CONFIRMED_NO_REFUND) {
                return Err(refuse(format!(
                    "a reason starting with \"{}\" or \"{}\" marks a ban for cheating: use integrity confirm for one, \
                     or word the reason differently",
                    reason::CONFIRMED,
                    reason::CONFIRMED_NO_REFUND
                )));
            }
            let (now, until) = (c.now, c.now + hours * HOUR_MS);
            let id = db.sanctions().create(&NewSanction {
                user_id: u.id,
                kind: SanctionKind::Ban,
                reason: Some(why.clone()),
                source: Source::Moderator,
                game_id: None,
                starts_at: now,
                ends_at: Some(until),
                created_by: Some(c.moderator.clone()),
                created_at: now,
            })?;
            let revoked =
                if c.on("revoke-sessions") { db.sessions().revoke_all_for_user(u.id, None, now)?.len() } else { 0 };
            let audit = c.audit(
                db,
                "ban",
                Some(u.id),
                vec![
                    ("hours", json!(hours)),
                    ("reason", json!(why)),
                    ("sanctionId", json!(id)),
                    ("until", json!(until)),
                    ("revokedSessions", json!(revoked)),
                ],
            )?;
            let text = format!(
                "Banned {} until {} (sanction #{id}).{}\nThe running server applies it when the player next connects \
                 or tries to start a game (use --revoke-sessions to also log them out).\n",
                u.username,
                iso(Some(until)),
                if revoked > 0 { format!(" {revoked} sessions revoked.") } else { String::new() }
            );
            let data = json!({ "sanctionId": id, "until": until, "revokedSessions": revoked });
            Ok(Output::new(data, text).logged(vec![audit]))
        })
        .await
}

/// `user unban <name>`: lifts the active bans.
pub(super) async fn user_unban(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    ctx.store
        .write(move |db| {
            let u = require_user(db, c.positional(2))?;
            let mut lifted: Vec<i64> = Vec::new();
            for _ in 0..50 {
                let Some(b) = db.sanctions().active_ban(u.id, c.now)? else { break };
                if lifted.contains(&b.id) {
                    break;
                }
                db.sanctions().lift(b.id, Some(&c.moderator), c.now)?;
                lifted.push(b.id);
            }
            let mut logs = Vec::new();
            if !lifted.is_empty() {
                logs.push(c.audit(db, "unban", Some(u.id), vec![("sanctions", json!(lifted))])?);
            }
            let text = if lifted.is_empty() {
                format!("{} has no active ban.\n", u.username)
            } else {
                let ids: Vec<String> = lifted.iter().map(i64::to_string).collect();
                format!("Lifted {} ban(s) of {}: #{}.\n", lifted.len(), u.username, ids.join(", #"))
            };
            Ok(Output::new(json!({ "lifted": lifted }), text).logged(logs))
        })
        .await
}

/// `user reset-mfa <name>`: disables TOTP, deletes the recovery codes, logs out everywhere.
pub(super) async fn user_reset_mfa(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    ctx.store
        .write(move |db| {
            let u = require_user(db, c.positional(2))?;
            let reset = UserUpdate {
                mfa_enabled: Some(false),
                mfa_secret_enc: Some(None),
                pending_mfa_secret_enc: Some(None),
                mfa_last_step: Some(0),
                ..UserUpdate::default()
            };
            db.users().update(u.id, &reset)?;
            db.mfa().replace_recovery_codes(u.id, &[], c.now)?;
            let revoked = db.sessions().revoke_all_for_user(u.id, None, c.now)?.len();
            let audit = c.audit(db, "reset_mfa", Some(u.id), vec![("revokedSessions", json!(revoked))])?;
            let text = format!(
                "MFA disabled for {}, recovery codes deleted, {revoked} sessions revoked.\nOnly do this after verifying \
                 the owner by other means.\n",
                u.username
            );
            Ok(Output::new(json!({ "revokedSessions": revoked }), text).logged(vec![audit]))
        })
        .await
}

/// `user verify-email <name>`: marks the address verified; without an account, creates the
/// account of the pending signup that holds the name, as its link would.
pub(super) async fn user_verify_email(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    ctx.store
        .write(move |db| {
            let name = c.positional(2);
            if let Some(confirmed) = confirm_signup(db, &c, name)? {
                return Ok(confirmed);
            }
            let u = require_user(db, name)?;
            db.users().update(u.id, &UserUpdate { email_verified: Some(true), ..UserUpdate::default() })?;
            let audit = c.audit(db, "verify_email", Some(u.id), Vec::new())?;
            let text = format!("E-mail address of {} marked verified.\n", u.username);
            Ok(Output::new(json!({ "ok": true }), text).logged(vec![audit]))
        })
        .await
}

/// What the link of the pending signup holding `name` does: its account is created, its address
/// verified, and the signup deleted. A signup without a link (its address had an account), or one
/// whose username or address another account has now, is dropped and nothing is created. `None`
/// without such a signup.
fn confirm_signup(db: &Db<'_>, c: &Ctx, name: Option<&str>) -> Result<Option<Output>, Failure> {
    let Some(p) = signup_instead(db, name, c.now)? else { return Ok(None) };
    db.signups().delete(p.id)?;
    let id = if p.token_hash.is_none() || db.users().by_email(&p.email)?.is_some() {
        None
    } else {
        let user = NewUser {
            username: p.username.clone(),
            email: Some(p.email.clone()),
            password_hash: Some(p.password_hash.clone()),
            email_verified: true,
            accept_challenges: true,
            created_at: c.now,
        };
        match db.transaction(|db| db.users().create(&user)) {
            Ok(id) => Some(id),
            Err(e) if matches!(e.kind(), ErrorKind::UsernameTaken | ErrorKind::EmailTaken) => None,
            Err(e) => return Err(e.into()),
        }
    };
    let Some(id) = id else {
        let audit = c.audit(
            db,
            "confirm_signup",
            None,
            vec![("username", json!(p.username)), ("status", json!("taken"))],
        )?;
        let text = format!(
            "Pending signup {} dropped, no account created: its e-mail address had an account, or another account \
             has its username or its address now.\n",
            p.username
        );
        return Ok(Some(Output::new(json!({ "status": "taken" }), text).logged(vec![audit])));
    };
    db.security().insert_batch(&[NewSecurityEvent {
        kind: "register".into(),
        user_id: Some(id),
        ip: None,
        at: Some(c.now),
        detail: None,
    }])?;
    let audit = c.audit(
        db,
        "confirm_signup",
        Some(id),
        vec![("username", json!(p.username)), ("status", json!("confirmed"))],
    )?;
    let text = format!(
        "Account {} (#{id}) created from its pending signup, e-mail address {} verified.\n",
        p.username, p.email
    );
    let logs = vec![("register", json!({ "userId": id })), audit];
    Ok(Some(Output::new(json!({ "status": "confirmed", "userId": id }), text).logged(logs)))
}

/// `user revoke-sessions <name>`: logs the account out everywhere.
pub(super) async fn user_revoke_sessions(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    ctx.store
        .write(move |db| {
            let u = require_user(db, c.positional(2))?;
            let revoked = db.sessions().revoke_all_for_user(u.id, None, c.now)?.len();
            let audit =
                c.audit(db, "revoke_sessions", Some(u.id), vec![("revokedSessions", json!(revoked))])?;
            let text = format!(
                "{revoked} session(s) of {} revoked (the servers' caches drop them within 30 s).\n",
                u.username
            );
            Ok(Output::new(json!({ "revokedSessions": revoked }), text).logged(vec![audit]))
        })
        .await
}

/// Whether a bench prefix is valid: a letter, then up to 15 letters, digits or `_`.
fn valid_prefix(p: &str) -> bool {
    let mut chars = p.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && p.len() <= 16
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `bench-accounts --count N [--prefix bench] --out FILE [--format tokens|tsv]
/// --i-know-this-is-a-test-server`: verified accounts with a live session each, their tokens
/// written to a file of mode 600.
pub(super) async fn bench_accounts(ctx: &Ctx) -> Result<Output, Failure> {
    if !ctx.on("i-know-this-is-a-test-server") {
        return Err(refuse(
            "bench-accounts creates verified accounts with live sessions; refused without \
             --i-know-this-is-a-test-server",
        ));
    }
    let count = ctx.int_flag("count", 1, 100_000, None)?;
    let prefix = ctx.flag("prefix").map_or("bench", |f| f.text()).to_string();
    if !valid_prefix(&prefix) {
        return Err(refuse("--prefix: letters, digits and _ (starting with a letter)"));
    }
    let out = ctx.text_flag("out", true, 4096)?;
    let format = ctx.flag("format").map_or("tokens", |f| f.text()).to_string();
    if format != "tokens" && format != "tsv" {
        return Err(refuse("--format tokens|tsv"));
    }
    let width = count.to_string().len().max(4);
    let max_len = ctx.config.username_max.max(0) as usize;
    if prefix.len() + width > max_len {
        return Err(refuse(format!("usernames would exceed {max_len} characters: shorten --prefix")));
    }
    // An existing token file is made 600 below, which only its owner (or root) may do: another
    // user's file is refused before any account or session is created, and so is a symbolic link
    // (the tokens would land wherever it points; the open below refuses one too, O_NOFOLLOW).
    if let Ok(meta) = tokio::fs::symlink_metadata(&out).await {
        if meta.file_type().is_symlink() {
            return Err(refuse(format!("--out: {out} is a symbolic link; give the path of the file itself")));
        }
        let euid = ctx.euid.or_else(current_euid);
        if meta.is_file() && euid.is_some_and(|e| e != 0 && e != meta.uid()) {
            return Err(refuse(format!(
                "--out: {out} belongs to another user, so its mode cannot be made 600"
            )));
        }
    }
    let c = ctx.clone();
    let (lines, created, reused, audit) = ctx
        .store
        .write(move |db| {
            let now = c.now;
            let (mut lines, mut created, mut reused) = (Vec::new(), 0, 0);
            for i in 1..=count {
                let username = format!("{prefix}{i:0width$}");
                let email = format!("{}@bench.invalid", username.to_lowercase());
                let id = match db.users().by_username(&username)? {
                    Some(u) if u.email.as_deref().unwrap_or("").to_lowercase() != email => {
                        return Err(refuse(format!("\"{username}\" exists and is not a bench account")));
                    }
                    Some(u) => {
                        if !u.email_verified {
                            db.users().update(
                                u.id,
                                &UserUpdate { email_verified: Some(true), ..UserUpdate::default() },
                            )?;
                        }
                        reused += 1;
                        u.id
                    }
                    None => {
                        // No usable password: bench accounts only log in with the tokens written here.
                        let id = db.users().create(&NewUser {
                            username: username.clone(),
                            email: Some(email),
                            password_hash: Some("!bench-account-no-password".into()),
                            email_verified: true,
                            accept_challenges: true,
                            created_at: now,
                        })?;
                        created += 1;
                        id
                    }
                };
                let token = crate::security::keys::random_token("sct_");
                db.sessions().create(&NewSession {
                    user_id: id,
                    token_hash: (c.hash_token)(&token),
                    created_at: now,
                    expires_at: now + c.config.session_max_days * DAY_MS,
                    idle_expires_at: Some(now + c.config.session_idle_days * DAY_MS),
                    client_label: Some("bench".into()),
                    ip: None,
                })?;
                lines.push(if format == "tsv" { format!("{username}\t{token}") } else { token });
            }
            let audit = c.audit(
                db,
                "bench_accounts",
                None,
                vec![
                    ("count", json!(count)),
                    ("prefix", json!(prefix)),
                    ("created", json!(created)),
                    ("reused", json!(reused)),
                ],
            )?;
            Ok::<_, Failure>((lines, created, reused, audit))
        })
        .await?;
    let path = out.clone();
    tokio::task::spawn_blocking(move || write_tokens(&path, &lines))
        .await
        .map_err(|e| Failure::Failed(e.to_string()))??;
    let text = format!(
        "{count} bench accounts ready ({created} created, {reused} reused); tokens written to {out} (mode 600).\n"
    );
    let data = json!({ "count": count, "created": created, "reused": reused, "out": out });
    Ok(Output::new(data, text).logged(vec![audit]))
}

/// Writes the tokens: a new file is created with mode 600, an existing regular file is made 600
/// before the tokens go in, a device or a pipe (`--out /dev/null`) keeps its mode. A symbolic link
/// is refused.
fn write_tokens(path: &str, lines: &[String]) -> Result<(), Failure> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                refuse(format!("--out: {path} is a symbolic link; give the path of the file itself"))
            } else {
                Failure::from(e)
            }
        })?;
    if file.metadata()?.is_file() {
        file.set_permissions(Permissions::from_mode(0o600))?;
    }
    let mut body = lines.join("\n");
    body.push('\n');
    file.write_all(body.as_bytes())?;
    Ok(())
}
