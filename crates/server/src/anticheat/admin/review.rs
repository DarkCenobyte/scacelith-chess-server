//! The review commands: integrity (`list|show|confirm|clear`), rating refunds (`apply|list`),
//! reports (`list|resolve`), anomalies, stats and the moderator's analysis requests.

use std::cmp::Ordering;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::rows::{self, object, public_user};
use super::text::{fixed, iso, js_string, or_dash, pct, slice, table, truthy};
use super::{Ctx, Failure, HOUR_MS, Output, Record, refuse, report_weight_30d, require_user};
use crate::anticheat::num::json_num;
use crate::anticheat::refunds::{
    is_cheating_ban, log_refunds, reason, refund_victims, refund_window_start, victim_totals,
};
use crate::anticheat::scoring::side_json;
use crate::ids::{UserId, is_game_id};
use crate::store::{
    CheaterRefunds, Db, GivenRefund, IntegrityLevel, IntegrityUpdate, JobStatus, NewSanction, Priority,
    RefundScope, Report, ReportCategory, ReportStatus, SanctionKind, Source,
};
use crate::util::{js, json as js_json};

/// `x` as a whole number of milliseconds, when it is a number.
fn time_of(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite()).map(|x| x as i64)
}

/// Adds a review to the evidence, the latest 20 kept.
fn push_review(ev: &mut Map<String, Value>, entry: Value) {
    let mut reviews = match ev.get("reviews") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    reviews.push(entry);
    let skip = reviews.len().saturating_sub(20);
    ev.insert("reviews".into(), Value::Array(reviews.split_off(skip)));
}

/// The text of the refunds given (and the window they were given for).
fn refund_text(db: &Db<'_>, from: Option<i64>, given: &[GivenRefund]) -> String {
    let Some(from) = from else {
        return "No rating refunds (--no-refund, or RATING_REFUND_DAYS=0).\n".into();
    };
    let victims = victim_totals(given);
    let points: i64 = given.iter().map(|r| r.points).sum();
    let mut t = format!(
        "Rating refunds of the games since {}: {} game(s), {points} point(s) to {} player(s)",
        iso(Some(from)),
        given.len(),
        victims.len()
    );
    if given.is_empty() {
        t += ".\n";
        return t;
    }
    t += " (they are told at their next moment out of a game).\n";
    let name =
        |id: UserId| db.users().by_id(id).ok().flatten().map_or_else(|| format!("#{id}"), |u| u.username);
    let lines: Vec<Vec<String>> = given
        .iter()
        .map(|r| {
            vec![
                r.game_id.to_string(),
                name(r.victim_id),
                r.category.clone(),
                r.points.to_string(),
                iso(Some(r.ended_at)),
            ]
        })
        .collect();
    t += &table(&["game", "victim", "category", "points", "ended"], &lines);
    t
}

/// `integrity list [--level L] [--limit N]`: the flagged players by review priority.
pub(super) async fn integrity_list(ctx: &Ctx) -> Result<Output, Failure> {
    let level = IntegrityLevel::parse(ctx.flag("level").map_or("suspected", |f| f.text()))
        .filter(|l| *l != IntegrityLevel::None)
        .ok_or_else(|| refuse("--level must be suspected, high_confidence or confirmed"))?;
    let limit = ctx.int_flag("limit", 1, 10_000, Some(50))?;
    let c = ctx.clone();
    ctx.store
        .read(move |db| {
            struct Row {
                data: Value,
                level: IntegrityLevel,
                score: f64,
                priority: i64,
                cells: Vec<String>,
            }
            let mut out = Vec::new();
            for f in db.integrity().list_flagged(level, limit)? {
                let r = &f.integrity;
                let w = report_weight_30d(db, f.user_id, c.now);
                let score = if r.score.is_nan() { 0.0 } else { r.score };
                let record = Record { level: r.level, score, ..Record::default() };
                let priority = record.priority(w);
                let evidence = r.evidence.as_ref().map(rows::structured).unwrap_or(Value::Null);
                let games = evidence
                    .pointer("/statistics/windows/all/games")
                    .filter(|g| !g.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!("-"));
                let cells = vec![
                    priority.to_string(),
                    f.username.clone(),
                    r.level.as_str().into(),
                    js::to_fixed(score, 2),
                    js_string(Some(&games)),
                    js::number_to_string(w),
                    iso(Some(r.updated_at)),
                    r.reviewed_by.clone().unwrap_or_default(),
                ];
                let data = json!({
                    "userId": f.user_id,
                    "username": f.username,
                    "level": r.level.as_str(),
                    "score": json_num(score),
                    "priority": priority,
                    "reports30d": json_num(w),
                    "games": games,
                    "updatedAt": r.updated_at,
                    "reviewedBy": r.reviewed_by,
                });
                out.push(Row { data, level: r.level, score, priority, cells });
            }
            out.retain(|r| r.level.rank() >= level.rank());
            out.sort_by(|a, b| {
                b.priority.cmp(&a.priority).then(b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
            });
            let cells: Vec<Vec<String>> = out.iter().map(|r| r.cells.clone()).collect();
            let text = table(
                &["priority", "username", "level", "score", "games", "reports30d", "updated", "reviewed"],
                &cells,
            );
            Ok(Output::new(Value::Array(out.into_iter().map(|r| r.data).collect()), text))
        })
        .await
}

/// `integrity show <name>`: evidence, per-game features, anomalies, reports and sanctions.
pub(super) async fn integrity_show(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    ctx.store
        .read(move |db| {
            let u = require_user(db, c.positional(2))?;
            let integ = Record::read_or_default(db, u.id);
            let games: Vec<Value> = db
                .analysis()
                .for_user(u.id, 30, true)
                .unwrap_or_default()
                .iter()
                .filter_map(|row| row.features.as_ref().and_then(|f| side_json(f, u.id)))
                .collect();
            let anomalies = db.anomalies().for_user(u.id, 50).unwrap_or_default();
            let reports = db.reports().for_reported(u.id, 200).unwrap_or_default();
            let sanctions = db.sanctions().list(u.id).unwrap_or_default();
            let priority = integ.priority(report_weight_30d(db, u.id, c.now));
            let data = json!({
                "user": public_user(&u),
                "integrity": {
                    "level": integ.level.as_str(),
                    "score": json_num(integ.score),
                    "evidence": integ.evidence,
                    "updatedAt": integ.updated_at,
                    "reviewedBy": integ.reviewed_by,
                },
                "priority": priority,
                "games": games,
                "anomalies": anomalies.iter().map(rows::anomaly).collect::<Vec<_>>(),
                "reports": reports.iter().map(rows::report).collect::<Vec<_>>(),
                "sanctions": sanctions.iter().map(rows::sanction).collect::<Vec<_>>(),
            });
            let ev = &integ.evidence;
            let st = ev.get("statistics").filter(|s| truthy(Some(s)));
            let get = |v: Option<&Value>, k: &str| v.and_then(|v| v.get(k)).cloned();
            let mut t = format!(
                "Integrity of {} (#{}): {}, score {}, review priority {priority}\n",
                u.username,
                u.id,
                integ.level.as_str(),
                js::to_fixed(integ.score, 2)
            );
            t += &format!(
                "  updated {}{}\n",
                iso(Some(integ.updated_at)),
                integ.reviewed_by.as_deref().filter(|b| !b.is_empty()).map(|b| format!(", last reviewed by {b}")).unwrap_or_default()
            );
            if let Some(st) = st {
                let trigger = st.get("trigger").filter(|v| truthy(Some(v)));
                t += &format!(
                    "\nStatistical evidence (model v{}, {}): automatic level {}{}\n",
                    js_string(st.get("model")),
                    iso(time_of(st.get("computedAt"))),
                    js_string(st.get("level")),
                    trigger.map(|tr| format!(" ({})", js_string(Some(tr)))).unwrap_or_default()
                );
                if let Some(p) = st.get("profile").filter(|v| truthy(Some(v))) {
                    t += &format!("  analysis profile: {}\n", js_string(Some(p)));
                }
                let groups = st.get("groups");
                let g = |k: &str| js_string(groups.and_then(|g| g.get(k)));
                t += &format!(
                    "  groups: Q {} (quality), E {} (engine choice), J {} (jump), T {} (timing)\n",
                    g("Q"),
                    g("E"),
                    g("J"),
                    g("T")
                );
                if let Some(Value::Array(reasons)) = st.get("reasons") {
                    for r in reasons {
                        t += &format!("  - {}\n", js_string(Some(r)));
                    }
                }
                let peak = ev.get("peak").filter(|p| truthy(Some(p)));
                let higher = match (get(peak, "score").and_then(|v| v.as_f64()), st.get("score").and_then(Value::as_f64)) {
                    (Some(p), Some(s)) => p > s,
                    _ => false,
                };
                if higher {
                    t += &format!(
                        "  peak score {} on {} (level {})\n",
                        js_string(get(peak, "score").as_ref()),
                        iso(time_of(get(peak, "at").as_ref())),
                        js_string(get(peak, "level").as_ref())
                    );
                }
            } else {
                t += "\nNo statistical evidence yet.\n";
            }
            if let Some(Value::Array(certain)) = ev.get("certain").filter(|c| c.as_array().is_some_and(|a| !a.is_empty())) {
                let lines: Vec<Vec<String>> = certain
                    .iter()
                    .map(|c| {
                        vec![
                            iso(time_of(c.get("at"))),
                            or_dash(c.get("kind")),
                            or_dash(c.get("gameId")),
                            iso(time_of(c.get("banUntil"))),
                        ]
                    })
                    .collect();
                t += "\nCertain protocol cheats\n";
                t += &table(&["at", "kind", "game", "banUntil"], &lines);
            }
            if let Some(Value::Array(reviews)) = ev.get("reviews").filter(|r| r.as_array().is_some_and(|a| !a.is_empty())) {
                let lines: Vec<Vec<String>> = reviews
                    .iter()
                    .map(|r| {
                        let why = r.get("reason").filter(|v| truthy(Some(v)));
                        vec![
                            iso(time_of(r.get("at"))),
                            or_dash(r.get("action")),
                            or_dash(r.get("by")),
                            why.map(|v| js_string(Some(v))).unwrap_or_default(),
                        ]
                    })
                    .collect();
                t += "\nReviews\n";
                t += &table(&["at", "action", "by", "reason"], &lines);
            }
            // Games of another analysis profile than the statistics' are not part of the scores.
            let profile = st.and_then(|s| s.get("profile")).filter(|p| truthy(Some(p)));
            let other = |g: &Value| profile.is_some_and(|p| g.get("profile") != Some(p));
            let lines: Vec<Vec<String>> = games
                .iter()
                .map(|g| {
                    let complex = match g.get("t1Complex") {
                        Some(Value::Null) => "-".into(),
                        v => format!("{}/{}", pct(v), js_string(g.get("nComplex"))),
                    };
                    vec![
                        if other(g) { format!("{}*", js_string(g.get("gameId"))) } else { or_dash(g.get("gameId")) },
                        or_dash(g.get("category")),
                        or_dash(g.get("rating")),
                        or_dash(g.get("n")),
                        fixed(g.get("accuracy"), 1),
                        fixed(g.get("acpl"), 1),
                        pct(g.get("t1Deep")),
                        pct(g.get("t1Fast")),
                        complex,
                        fixed(g.get("timeCorr"), 2),
                        fixed(g.get("timeCv"), 2),
                    ]
                })
                .collect();
            t += "\nAnalysed games (newest first)\n";
            t += &table(&["game", "cat", "rating", "moves", "acc", "acpl", "t1%", "fast%", "cx%", "time~cx", "cv"], &lines);
            if games.iter().any(other) {
                t += "  * analysed with another profile (engine, network, depths or hash): not in the scores above\n";
            }
            let lines: Vec<Vec<String>> = anomalies
                .iter()
                .map(|a| {
                    vec![
                        iso(Some(a.at)),
                        a.kind.clone(),
                        a.severity.as_str().into(),
                        a.game_id.filter(|g| *g != 0).map(|g| g.to_string()).unwrap_or_default(),
                    ]
                })
                .collect();
            t += "\nAnomalies (latest 50)\n";
            t += &table(&["at", "kind", "severity", "game"], &lines);
            let lines: Vec<Vec<String>> = reports.iter().map(report_line).collect();
            t += "\nReports received\n";
            t += &table(&["id", "at", "category", "weight", "game", "outcome", "comment"], &lines);
            let lines: Vec<Vec<String>> = sanctions
                .iter()
                .map(|x| {
                    vec![
                        x.id.to_string(),
                        x.kind.as_str().into(),
                        x.source.as_str().into(),
                        iso(x.ends_at),
                        x.reason.clone().unwrap_or_else(|| "-".into()),
                    ]
                })
                .collect();
            t += "\nSanctions\n";
            t += &table(&["id", "kind", "source", "until", "reason"], &lines);
            Ok(Output::new(data, t))
        })
        .await
}

/// One received report in `integrity show`.
fn report_line(r: &Report) -> Vec<String> {
    vec![
        r.id.to_string(),
        iso(Some(r.created_at)),
        r.category.as_str().into(),
        js::number_to_string(r.weight),
        r.game_id.map_or_else(|| "-".into(), |g| g.to_string()),
        r.status.as_str().into(),
        slice(r.comment.as_deref().unwrap_or(""), 60).to_string(),
    ]
}

/// What `integrity confirm` stored.
struct Confirmed {
    username: String,
    previous: IntegrityLevel,
    sanction_id: i64,
    until: i64,
    resolved: usize,
    from: Option<i64>,
    since: Option<i64>,
    request: Option<CheaterRefunds>,
    given: Vec<GivenRefund>,
    refund_error: Option<String>,
    refunds_text: String,
    audit: (&'static str, Value),
}

/// `integrity confirm <name> --reason TEXT [--hours N] [--keep-reports] [--refund-since DATE |
/// --no-refund]`: level confirmed and a ban for cheating, the open cheating reports actioned, the
/// rating refunds of the player's victims.
pub(super) async fn integrity_confirm(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    let done = ctx
        .store
        .write(move |db| {
            let u = require_user(db, c.positional(2))?;
            let why = c.text_flag("reason", true, 300)?;
            let hours = c.int_flag("hours", 1, 87_600, Some(c.config.ban_duration_hours))?;
            let no_refund = c.on("no-refund");
            let since = c.date_flag("refund-since")?;
            if no_refund && since.is_some() {
                return Err(refuse("--refund-since and --no-refund exclude each other"));
            }
            let (now, until) = (c.now, c.now + hours * HOUR_MS);
            // The reason tells the store whether the games recorded during the ban are refunded
            // (refunds::ban_refunds): not after --no-refund.
            let ban_reason =
                format!("{}{why}", if no_refund { reason::CONFIRMED_NO_REFUND } else { reason::CONFIRMED });
            let prev = Record::read(db, u.id)?;
            let mut ev = prev.evidence.clone();
            push_review(
                &mut ev,
                json!({ "action": "confirm", "by": c.moderator, "at": now, "reason": why,
                    "previousLevel": prev.level.as_str(), "score": json_num(prev.score) }),
            );
            let mut review = ev.get("review").and_then(Value::as_object).cloned().unwrap_or_default();
            review.insert("confirmedAt".into(), json!(now));
            review.insert("by".into(), json!(c.moderator));
            ev.insert("review".into(), Value::Object(review));
            // The ban first: when it cannot be stored, the level is left as it was.
            let sanction_id = db.sanctions().create(&NewSanction {
                user_id: u.id,
                kind: SanctionKind::Ban,
                reason: Some(ban_reason),
                source: Source::Moderator,
                game_id: None,
                starts_at: now,
                ends_at: Some(until),
                created_by: Some(c.moderator.clone()),
                created_at: now,
            })?;
            db.integrity().set(
                u.id,
                &IntegrityUpdate {
                    level: Some(IntegrityLevel::Confirmed),
                    score: Some(prev.score),
                    evidence: Some(Some(Value::Object(ev))),
                    reviewed_by: Some(Some(c.moderator.clone())),
                    updated_at: Some(now),
                    ..IntegrityUpdate::default()
                },
            )?;
            let resolved = if c.on("keep-reports") {
                0
            } else {
                db.reports()
                    .resolve_open_for(
                        u.id,
                        ReportCategory::Cheating,
                        ReportStatus::Actioned,
                        Some(&c.moderator),
                        now,
                    )?
                    .len()
            };
            let from = if no_refund {
                None
            } else {
                since.or_else(|| refund_window_start(c.config.rating_refund_days, now))
            };
            // The ban stands whatever happens to the refunds (a savepoint of their own): a failure
            // is audited with the ban and reported, and `refunds apply` gives them later.
            let request = from.map(|from| CheaterRefunds {
                cheater_id: u.id,
                since: from,
                now,
                sanction_id: Some(sanction_id),
                source: Source::Moderator,
                by: Some(c.moderator.clone()),
            });
            let (given, refund_error) = match &request {
                Some(r) => match refund_victims(db, r, &c.logger) {
                    Ok(given) => (given, None),
                    Err(e) => (Vec::new(), Some(e.message().to_string())),
                },
                None => (Vec::new(), None),
            };
            let victims = victim_totals(&given).len();
            let points: i64 = given.iter().map(|r| r.points).sum();
            let audit = c.audit(
                db,
                "integrity_confirm",
                Some(u.id),
                vec![
                    ("reason", json!(why)),
                    ("previousLevel", json!(prev.level.as_str())),
                    ("score", json_num(prev.score)),
                    ("sanctionId", json!(sanction_id)),
                    ("until", json!(until)),
                    ("reportsActioned", json!(resolved)),
                    ("refundSince", json!(from)),
                    ("refunds", json!(given.len())),
                    ("refundedVictims", json!(victims)),
                    ("refundedPoints", json!(points)),
                    ("refundError", json!(refund_error)),
                ],
            )?;
            let refunds_text = refund_text(db, from, &given);
            Ok(Confirmed {
                username: u.username,
                previous: prev.level,
                sanction_id,
                until,
                resolved,
                from,
                since,
                request,
                given,
                refund_error,
                refunds_text,
                audit,
            })
        })
        .await?;
    if let Some(r) = &done.request {
        log_refunds(&ctx.logger, r, &done.given);
    }
    if let Some(e) = &done.refund_error {
        let (msg, fields) = done.audit;
        ctx.logger.emit(crate::log::Level::Security, msg, Some(fields));
        return Err(refuse(format!(
            "{}: integrity confirmed and banned until {} (sanction #{}), but the rating refunds failed ({e}): give \
             them with `refunds apply {}{}`",
            done.username,
            iso(Some(done.until)),
            done.sanction_id,
            done.username,
            done.since.map(|s| format!(" --since {}", iso(Some(s)))).unwrap_or_default()
        )));
    }
    let data = json!({
        "level": "confirmed",
        "sanctionId": done.sanction_id,
        "until": done.until,
        "reportsActioned": done.resolved,
        "refundSince": done.from,
        "refunds": done.given.iter().map(rows::given).collect::<Vec<_>>(),
    });
    let text = format!(
        "{}: integrity confirmed (was {}), banned until {} (sanction #{}), {} open cheating report(s) marked \
         actioned.\n{}",
        done.username,
        done.previous.as_str(),
        iso(Some(done.until)),
        done.sanction_id,
        done.resolved,
        done.refunds_text
    );
    Ok(Output::new(data, text).logged(vec![done.audit]))
}

/// `integrity clear <name> [--reason TEXT] [--dismiss-reports]`: level none (the model raises it
/// again only on new evidence); bans are not lifted.
pub(super) async fn integrity_clear(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    ctx.store
        .write(move |db| {
            let u = require_user(db, c.positional(2))?;
            let why = c.text_flag("reason", false, 300)?;
            let now = c.now;
            let prev = Record::read(db, u.id)?;
            let mut ev = prev.evidence.clone();
            push_review(
                &mut ev,
                json!({ "action": "clear", "by": c.moderator, "at": now, "reason": why,
                    "previousLevel": prev.level.as_str(), "score": json_num(prev.score) }),
            );
            ev.insert(
                "review".into(),
                json!({ "clearedAt": now, "clearedScore": json_num(prev.score), "by": c.moderator }),
            );
            db.integrity().set(
                u.id,
                &IntegrityUpdate {
                    level: Some(IntegrityLevel::None),
                    score: Some(prev.score),
                    evidence: Some(Some(Value::Object(ev))),
                    reviewed_by: Some(Some(c.moderator.clone())),
                    updated_at: Some(now),
                    ..IntegrityUpdate::default()
                },
            )?;
            let dismissed = if c.on("dismiss-reports") {
                db.reports()
                    .resolve_open_for(
                        u.id,
                        ReportCategory::Cheating,
                        ReportStatus::Dismissed,
                        Some(&c.moderator),
                        now,
                    )?
                    .len()
            } else {
                0
            };
            let audit = c.audit(
                db,
                "integrity_clear",
                Some(u.id),
                vec![
                    ("reason", json!(why)),
                    ("previousLevel", json!(prev.level.as_str())),
                    ("score", json_num(prev.score)),
                    ("reportsDismissed", json!(dismissed)),
                ],
            )?;
            let data =
                json!({ "level": "none", "previous": prev.level.as_str(), "reportsDismissed": dismissed });
            let text = format!(
                "{}: integrity cleared (was {}).{} Bans are not lifted by this command (user unban).\n",
                u.username,
                prev.level.as_str(),
                if dismissed > 0 {
                    format!(" {dismissed} open cheating report(s) dismissed.")
                } else {
                    String::new()
                }
            );
            Ok(Output::new(data, text).logged(vec![audit]))
        })
        .await
}

/// `reports list [--limit N]`: the open reports grouped by reported player, by priority.
pub(super) async fn reports_list(ctx: &Ctx) -> Result<Output, Failure> {
    let limit = ctx.int_flag("limit", 1, 10_000, Some(100))?;
    let c = ctx.clone();
    ctx.store
        .read(move |db| {
            let open = db.reports().list_open(limit)?;
            let mut groups: IndexMap<UserId, Vec<&Report>> = IndexMap::new();
            for r in &open {
                groups.entry(r.reported_id).or_default().push(r);
            }
            struct Row {
                data: Value,
                priority: i64,
                weight: f64,
                cells: Vec<String>,
            }
            let mut out = Vec::new();
            for (reported, reports) in groups {
                let username = db
                    .users()
                    .by_id(reported)
                    .ok()
                    .flatten()
                    .map_or_else(|| format!("#{reported}"), |u| u.username);
                let integ = Record::read_or_default(db, reported);
                let weight: f64 = reports.iter().map(|r| r.weight).sum();
                let w30 = report_weight_30d(db, reported, c.now);
                let priority = integ.priority(if w30 != 0.0 { w30 } else { weight });
                let mut categories: Vec<&str> = Vec::new();
                for r in &reports {
                    if !categories.contains(&r.category.as_str()) {
                        categories.push(r.category.as_str());
                    }
                }
                let ids: Vec<i64> = reports.iter().map(|r| r.id).collect();
                let latest = reports.iter().map(|r| r.created_at).max().unwrap_or(0);
                let rounded = js::round(weight * 1000.0) / 1000.0;
                let cells = vec![
                    priority.to_string(),
                    username.clone(),
                    integ.level.as_str().into(),
                    js::to_fixed(integ.score, 2),
                    reports.len().to_string(),
                    js::number_to_string(rounded),
                    categories.join(","),
                    iso(Some(latest)),
                    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(","),
                ];
                let data = json!({
                    "reportedId": reported,
                    "username": username,
                    "level": integ.level.as_str(),
                    "score": json_num(integ.score),
                    "priority": priority,
                    "open": reports.len(),
                    "weight": json_num(rounded),
                    "categories": categories.join(","),
                    "ids": ids,
                    "latest": latest,
                });
                out.push(Row { data, priority, weight: rounded, cells });
            }
            out.sort_by(|a, b| {
                b.priority.cmp(&a.priority).then(b.weight.partial_cmp(&a.weight).unwrap_or(Ordering::Equal))
            });
            let cells: Vec<Vec<String>> = out.iter().map(|r| r.cells.clone()).collect();
            let text = table(
                &["priority", "username", "level", "score", "open", "weight", "categories", "latest", "ids"],
                &cells,
            );
            Ok(Output::new(Value::Array(out.into_iter().map(|r| r.data).collect()), text))
        })
        .await
}

/// `reports resolve <id> actioned|dismissed`.
pub(super) async fn reports_resolve(ctx: &Ctx) -> Result<Output, Failure> {
    let id_text = ctx.positional(2).unwrap_or("").to_string();
    if id_text.is_empty() || !id_text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(refuse("reports resolve <id> actioned|dismissed"));
    }
    let outcome = match ctx.positional(3) {
        Some("actioned") => ReportStatus::Actioned,
        Some("dismissed") => ReportStatus::Dismissed,
        _ => return Err(refuse("the outcome is actioned or dismissed")),
    };
    let id = id_text.parse::<i64>().ok();
    let shown = id.map_or(id_text, |i| i.to_string());
    let c = ctx.clone();
    ctx.store
        .write(move |db| {
            let resolved = match id {
                Some(id) => db.reports().resolve(id, outcome, Some(&c.moderator), c.now)?,
                None => false,
            };
            let Some(id) = id.filter(|_| resolved) else {
                return Err(refuse(format!("report #{shown} not found or already resolved")));
            };
            let audit = c.audit(
                db,
                "report_resolve",
                None,
                vec![("reportId", json!(id)), ("outcome", json!(outcome.as_str()))],
            )?;
            let text = format!("Report #{id} {}.\n", outcome.as_str());
            Ok(Output::new(json!({ "id": id, "outcome": outcome.as_str() }), text).logged(vec![audit]))
        })
        .await
}

/// `anomalies <name> [--limit N]`.
pub(super) async fn anomalies(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    ctx.store
        .read(move |db| {
            let u = require_user(db, c.positional(1))?;
            let limit = c.int_flag("limit", 1, 10_000, Some(50))?;
            let list = db.anomalies().for_user(u.id, limit)?;
            let lines: Vec<Vec<String>> = list
                .iter()
                .map(|a| {
                    let detail = a.detail.as_ref().map(rows::structured).unwrap_or_else(|| json!(""));
                    vec![
                        iso(Some(a.at)),
                        a.kind.clone(),
                        a.severity.as_str().into(),
                        a.game_id.filter(|g| *g != 0).map(|g| g.to_string()).unwrap_or_default(),
                        slice(&js_json::to_string(&detail), 80).to_string(),
                    ]
                })
                .collect();
            let text = table(&["at", "kind", "severity", "game", "detail"], &lines);
            Ok(Output::new(Value::Array(list.iter().map(rows::anomaly).collect()), text))
        })
        .await
}

/// `stats`: players flagged by level, open reports.
pub(super) async fn stats(ctx: &Ctx) -> Result<Output, Failure> {
    ctx.store
        .read(move |db| {
            let (mut suspected, mut high, mut confirmed) = (0, 0, 0);
            for f in db.integrity().list_flagged(IntegrityLevel::Suspected, 100_000).unwrap_or_default() {
                match f.integrity.level {
                    IntegrityLevel::Suspected => suspected += 1,
                    IntegrityLevel::HighConfidence => high += 1,
                    IntegrityLevel::Confirmed => confirmed += 1,
                    IntegrityLevel::None => {}
                }
            }
            let open = db.reports().list_open(100_000).map(|r| r.len()).unwrap_or(0);
            let data = json!({
                "integrity": { "suspected": suspected, "high_confidence": high, "confirmed": confirmed },
                "openReports": open,
                "store": null,
            });
            let text = format!(
                "Integrity: {suspected} suspected, {high} high confidence, {confirmed} confirmed\nOpen reports: {open}\n"
            );
            Ok::<_, Failure>(Output::new(data, text))
        })
        .await
}

/// `refunds apply <name> [--since DATE]`: the refunds of a confirmed cheater's games since DATE
/// (default: RATING_REFUND_DAYS before their latest ban for cheating).
pub(super) async fn refunds_apply(ctx: &Ctx) -> Result<Output, Failure> {
    let c = ctx.clone();
    let (output, request, given) = ctx
        .store
        .write(move |db| {
            let u = require_user(db, c.positional(2))?;
            if Record::read_or_default(db, u.id).level != IntegrityLevel::Confirmed {
                return Err(refuse(format!(
                    "{} is not a confirmed cheater (integrity confirm first)",
                    u.username
                )));
            }
            let now = c.now;
            // The window counts back from the latest ban for cheating, not from a later ban for
            // something else (`user ban`).
            let mut bans: Vec<_> = db
                .sanctions()
                .list(u.id)
                .unwrap_or_default()
                .into_iter()
                .filter(|x| is_cheating_ban(x) && x.starts_at <= now)
                .collect();
            bans.sort_by_key(|b| std::cmp::Reverse(b.starts_at));
            let ban = bans.first();
            let since = match c.date_flag("since")? {
                Some(s) => Some(s),
                None => refund_window_start(c.config.rating_refund_days, ban.map_or(now, |b| b.starts_at)),
            };
            let Some(since) = since else {
                return Err(refuse(
                    "RATING_REFUND_DAYS is 0: give the start of the refunds with --since DATE",
                ));
            };
            let sanction_id = ban.map(|b| b.id);
            let request = CheaterRefunds {
                cheater_id: u.id,
                since,
                now,
                sanction_id,
                source: Source::Moderator,
                by: Some(c.moderator.clone()),
            };
            let given = refund_victims(db, &request, &c.logger)?;
            let audit = c.audit(
                db,
                "refunds_apply",
                Some(u.id),
                vec![
                    ("since", json!(since)),
                    ("sanctionId", json!(sanction_id)),
                    ("refunds", json!(given.len())),
                    ("refundedVictims", json!(victim_totals(&given).len())),
                    ("refundedPoints", json!(given.iter().map(|r| r.points).sum::<i64>())),
                ],
            )?;
            let data = json!({
                "since": since,
                "sanctionId": sanction_id,
                "refunds": given.iter().map(rows::given).collect::<Vec<_>>(),
            });
            let text = format!("{}: {}", u.username, refund_text(db, Some(since), &given));
            Ok((Output::new(data, text).logged(vec![audit]), request, given))
        })
        .await?;
    log_refunds(&ctx.logger, &request, &given);
    Ok(output)
}

/// `refunds list [<name>] [--victim NAME] [--limit N]`: the refunds of a cheater's games, those a
/// victim received, or all.
pub(super) async fn refunds_list(ctx: &Ctx) -> Result<Output, Failure> {
    let limit = ctx.int_flag("limit", 1, 10_000, Some(100))?;
    let c = ctx.clone();
    ctx.store
        .read(move |db| {
            let cheater = match c.positional(2).filter(|n| !n.is_empty()) {
                Some(name) => Some(require_user(db, Some(name))?),
                None => None,
            };
            let victim = match c.flag("victim") {
                None => None,
                Some(super::Flag::On) => return Err(refuse("--victim NAME")),
                Some(super::Flag::Value(v)) => Some(require_user(db, Some(v))?),
            };
            let scope = match (cheater, victim) {
                (Some(_), Some(_)) => return Err(refuse("give a cheater or --victim, not both")),
                (Some(c), None) => RefundScope::Cheater(c.id),
                (None, Some(v)) => RefundScope::Victim(v.id),
                (None, None) => RefundScope::All,
            };
            let list = db.refunds().list(scope, limit)?;
            let lines: Vec<Vec<String>> = list
                .iter()
                .map(|r| {
                    let by = match (&r.created_by, r.sanction_id) {
                        (Some(by), _) if !by.is_empty() => by.clone(),
                        (_, Some(id)) if id != 0 => format!("ban #{id}"),
                        _ => String::new(),
                    };
                    vec![
                        r.id.to_string(),
                        iso(Some(r.created_at)),
                        r.game_id.to_string(),
                        r.cheater_name.clone(),
                        r.victim_name.clone(),
                        r.category.clone(),
                        r.points.to_string(),
                        r.source.as_str().into(),
                        by,
                        r.notified_at.filter(|t| *t != 0).map_or_else(|| "not yet".into(), |t| iso(Some(t))),
                    ]
                })
                .collect();
            let text = table(
                &["id", "at", "game", "cheater", "victim", "category", "points", "source", "by", "notified"],
                &lines,
            );
            Ok(Output::new(Value::Array(list.iter().map(rows::refund).collect()), text))
        })
        .await
}

/// The name of an analysis priority.
fn priority_name(p: Priority) -> &'static str {
    match p {
        Priority::Ordinary => "ordinary",
        Priority::Signal => "signal",
        Priority::Report => "report",
        Priority::Manual => "manual",
    }
}

/// `analysis queue <gameId>`: the game is analysed before every other one, whatever the automatic
/// policy decided for it (a casual or short game is queued too). A job waiting at a lower
/// priority moves up and a failed one is queued again; a game being analysed, already analysed or
/// already requested is left as it is. The job is read and queued in one store job, so that an
/// engine cannot claim it in between.
pub(super) async fn analysis_queue(ctx: &Ctx) -> Result<Output, Failure> {
    let id_text = ctx.positional(2).unwrap_or("");
    let id = (!id_text.is_empty() && id_text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id_text.parse::<u64>().ok())
        .flatten()
        .filter(|id| is_game_id(*id))
        .ok_or_else(|| refuse("analysis queue <gameId>: the number of a game"))?;
    let c = ctx.clone();
    ctx.store
        .write(move |db| {
            if db.games().by_id(id)?.is_none() {
                return Err(refuse(format!("no game #{id}")));
            }
            let previous = db.analysis().job(id)?;
            let leave = previous.as_ref().is_some_and(|j| {
                matches!(j.status, JobStatus::Running | JobStatus::Done)
                    || (j.status == JobStatus::Queued && j.priority >= Priority::Manual)
            });
            let mut logs = Vec::new();
            if !leave {
                db.analysis().enqueue(id, c.now)?;
                logs.push(c.audit(
                    db,
                    "analysis_queue",
                    None,
                    vec![("gameId", json!(id)), ("previousStatus", json!(previous.as_ref().map(|j| j.status.as_str())))],
                )?);
            }
            let text = match (&previous, leave) {
                (None, _) => format!(
                    "Game #{id} queued for engine analysis before every other game; it was not in the queue (a casual \
                     or short game, or one the queue policy left out).\n"
                ),
                (Some(j), false) if j.status == JobStatus::Failed => format!(
                    "Game #{id} queued for engine analysis before every other game; its analysis had failed ({}).\n",
                    super::text::cell(j.error.as_deref().unwrap_or("-"))
                ),
                (Some(j), false) => format!(
                    "Game #{id} queued for engine analysis before every other game; it was waiting at priority {}.\n",
                    priority_name(j.priority)
                ),
                (Some(j), true) if j.status == JobStatus::Queued => {
                    format!("Game #{id} is already queued before every other game.\n")
                }
                (Some(j), true) if j.status == JobStatus::Running => format!("Game #{id} is being analysed now.\n"),
                (Some(j), true) => {
                    format!("Game #{id} was already analysed ({}): not queued again.\n", iso(j.finished_at))
                }
            };
            let data = object([
                ("gameId", json!(id)),
                ("queued", json!(!leave)),
                ("previous", previous.as_ref().map_or(Value::Null, rows::job)),
            ]);
            Ok(Output::new(data, text).logged(logs))
        })
        .await
}
