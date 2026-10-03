//! Player reports: eligibility rules, reporter credibility, the anti-brigading cap and the review
//! priority, plus the service behind `POST /api/v1/reports` ([`Reports::file`]) and the
//! `reportable` flag of `GET /api/v1/games/:id` ([`Reports::can_report`]).
//!
//! A report never changes a player's integrity level; it only raises the review priority that
//! moderators see (`admin reports`, `admin integrity list`), in proportion to the reporter's
//! credibility, and asks for the engine analysis of the reported game ahead of the ordinary games
//! (a low-credibility report only at the priority of a suspicion signal). Many reports from new or
//! low-credibility accounts do not add up: the weight stored for a report is capped so that the
//! reports received by one player in 24 hours sum to at most [`rules::DAILY_WEIGHT_CAP`], and
//! the low-credibility ones to at most [`rules::LOW_CRED_DAILY_CAP`].

use serde_json::{Value, json};

use super::integrity::IntegrityLevel;
use super::players::level_of;
use crate::clock::SharedClock;
use crate::config::Config;
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::store::{
    Db, ErrorKind, GameSummary, NewReport, Priority, ReportCategory, ReportStatus, Store, StoreError,
};
use crate::util::js;
use crate::{log_security, log_warn};

/// The rules of the reports.
pub mod rules {
    /// Milliseconds in a day.
    pub const DAY_MS: i64 = 86_400_000;
    /// The game must have ended within the last 7 days.
    pub const MAX_AGE_MS: i64 = 7 * DAY_MS;
    /// Longest comment, in code points.
    pub const COMMENT_MAX: usize = 500;
    /// Longest reported username, in UTF-16 code units.
    pub const USERNAME_MAX: usize = 24;
    /// Summed weight of the reports one player receives per 24 hours.
    pub const DAILY_WEIGHT_CAP: f64 = 2.0;
    /// A report weighing less than this is of low credibility.
    pub const LOW_CREDIBILITY: f64 = 0.5;
    /// The low-credibility reports one player receives per 24 hours weigh at most this together.
    pub const LOW_CRED_DAILY_CAP: f64 = 0.5;
    /// Account age (days) giving full base credibility.
    pub const FULL_AGE_DAYS: f64 = 30.0;
    /// Games played giving full base credibility.
    pub const FULL_GAMES: f64 = 50.0;
    /// Past reports of a reporter weighed for their track record.
    pub const TRACK_RECORD_REPORTS: i64 = 500;
}

use rules::*;

fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    }
}

/// `Math.round(x * 1000) / 1000`.
fn round3(x: f64) -> f64 {
    js::round(x * 1000.0) / 1000.0
}

/// What weighs a reporter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reporter {
    /// Account creation time (`None`: unknown, counted as created now).
    pub created_at: Option<i64>,
    /// Games played over every category.
    pub games_played: i64,
    /// Past reports actioned by a moderator.
    pub actioned: i64,
    /// Past reports dismissed.
    pub dismissed: i64,
    /// The reporter's own integrity level.
    pub level: IntegrityLevel,
    pub now: i64,
}

/// A reporter's credibility.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReporterWeight {
    /// 0.02 .. 2, rounded to 3 decimals.
    pub weight: f64,
    pub base: f64,
    pub trust: f64,
}

/// Credibility weight of a reporter (0.02 .. 2):
/// * base = 0.1 + 0.9 sqrt(age x games), age = min(1, days / 30), games = min(1, games / 50):
///   both are needed (fresh account farms and old idle accounts both stay low);
/// * trust = 2 (actioned + 1) / (actioned + dismissed + 2), clamped to 0.25 .. 1.75: a Laplace
///   estimate of the reporter's hit rate, 1.0 without history;
/// * a reporter flagged high_confidence counts half, a confirmed cheater a fifth.
pub fn reporter_weight(r: &Reporter) -> ReporterWeight {
    let created = r.created_at.filter(|&c| c != 0).unwrap_or(r.now);
    let age_days = ((r.now - created) as f64 / DAY_MS as f64).max(0.0);
    let age = clamp(age_days / FULL_AGE_DAYS, 0.0, 1.0);
    let games = clamp(r.games_played as f64 / FULL_GAMES, 0.0, 1.0);
    let base = 0.1 + 0.9 * (age * games).sqrt();
    let (actioned, dismissed) = (r.actioned as f64, r.dismissed as f64);
    let trust = clamp((2.0 * (actioned + 1.0)) / (actioned + dismissed + 2.0), 0.25, 1.75);
    let mut w = base * trust;
    match r.level {
        IntegrityLevel::Confirmed => w *= 0.2,
        IntegrityLevel::HighConfidence => w *= 0.5,
        _ => {}
    }
    ReporterWeight { weight: round3(clamp(w, 0.02, 2.0)), base: round3(base), trust: round3(trust) }
}

/// The weight stored for a new report of raw weight `raw`, given the sums of the weights the same
/// player received in the last 24 hours: all of them (`today`) and those below
/// [`rules::LOW_CREDIBILITY`] (`today_low`).
pub fn capped_weight_of_sums(raw: f64, today: f64, today_low: f64) -> f64 {
    let mut w = raw;
    if raw < LOW_CREDIBILITY {
        w = w.min((LOW_CRED_DAILY_CAP - today_low).max(0.0));
    }
    w = w.min((DAILY_WEIGHT_CAP - today).max(0.0));
    round3(w)
}

/// A report received: its stored weight and time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Received {
    pub weight: f64,
    pub at: i64,
}

/// [`capped_weight_of_sums`] over a list of received reports (those older than 24 hours do not
/// count).
pub fn capped_weight(raw: f64, received: &[Received], now: i64) -> f64 {
    let (mut today, mut today_low) = (0.0, 0.0);
    for r in received.iter().filter(|r| r.at >= now - DAY_MS) {
        today += r.weight;
        if r.weight < LOW_CREDIBILITY {
            today_low += r.weight;
        }
    }
    capped_weight_of_sums(raw, today, today_low)
}

/// Sum of the weights of the reports received in the last `days` days.
pub fn recent_report_weight(received: &[Received], now: i64, days: i64) -> f64 {
    round3(received.iter().filter(|r| r.at >= now - days * DAY_MS).map(|r| r.weight).sum())
}

/// Review priority of a player for moderators (0 .. ~130): integrity level, statistical score,
/// and the credibility-weighted reports of the last 30 days (logarithmic: the 10th report adds
/// less than the 1st).
pub fn review_priority(level: IntegrityLevel, score: f64, report_weight: f64) -> i64 {
    let base = match level {
        IntegrityLevel::None => 0.0,
        IntegrityLevel::Suspected => 40.0,
        IntegrityLevel::HighConfidence => 70.0,
        IntegrityLevel::Confirmed => 5.0,
    };
    let score = if score.is_nan() { 0.0 } else { score };
    let report_weight = if report_weight.is_nan() { 0.0 } else { report_weight };
    let stat = (4.0 * score.max(0.0)).min(20.0);
    let rep = 15.0 * (1.0 + report_weight.max(0.0)).log2();
    js::round_i64(base + stat + rep)
}

/// A validated report body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportRequest {
    pub game_id: GameId,
    /// The reported username, trimmed.
    pub reported: String,
    pub category: ReportCategory,
    /// Without control characters, trimmed (may be empty).
    pub comment: String,
}

/// Validates a report body; the error is the message of the 400 answer.
pub fn validate_report(body: &Value) -> Result<ReportRequest, String> {
    let Some(body) = body.as_object() else { return Err("A JSON object is expected.".into()) };
    let game_id = match body.get("gameId") {
        Some(Value::String(s)) if (1..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse::<u64>().ok().map(|n| n as f64)
        }
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    };
    let game_id = match game_id {
        Some(g) if g > 0.0 && g.fract() == 0.0 && g <= 9_007_199_254_740_991.0 => g as GameId,
        _ => return Err("gameId must be a game id.".into()),
    };
    let reported = match body.get("reported") {
        Some(Value::String(s)) if !js::trim(s).is_empty() && js::utf16_len(s) <= USERNAME_MAX => js::trim(s),
        _ => return Err("reported must be a username.".into()),
    };
    let category = match body.get("category").and_then(Value::as_str).and_then(ReportCategory::parse) {
        Some(c) => c,
        None => return Err("category must be one of cheating, abuse, other.".into()),
    };
    let comment = match body.get("comment") {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err("comment must be text.".into()),
    };
    let cleaned: String = comment
        .chars()
        .filter(|&c| !matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'))
        .collect();
    let comment = js::trim(&cleaned).to_string();
    if comment.chars().count() > COMMENT_MAX {
        return Err(format!("comment is limited to {COMMENT_MAX} characters."));
    }
    Ok(ReportRequest { game_id, reported: reported.to_string(), category, comment })
}

/// The opponent a player may report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opponent {
    pub id: UserId,
    /// Their name in the game record.
    pub name: String,
}

/// The opponent `user` may report for `game`: the user played it, against another account, and it
/// ended within the last 7 days (not in the future). Only the reporter's own games qualify, so no
/// lookup of arbitrary usernames happens.
pub fn reportable_opponent(game: &GameSummary, user: UserId, now: i64) -> Option<Opponent> {
    let (id, name) = if user == game.white_id {
        (game.black_id, &game.black_name)
    } else if user == game.black_id {
        (game.white_id, &game.white_name)
    } else {
        return None;
    };
    if id == 0 || id == user {
        return None;
    }
    let ended = game.ended_at;
    if ended == 0 || ended < now - MAX_AGE_MS || ended > now + 60_000 {
        return None;
    }
    Some(Opponent { id, name: name.clone() })
}

/// The answer to `POST /api/v1/reports` (the route answers 401 `Log in first.` itself for an
/// anonymous request).
#[derive(Debug)]
pub enum ReportOutcome {
    /// 202 `{"status": "received"}`: filed, or already filed (the same answer, which says nothing
    /// about the reported account).
    Accepted,
    /// 400 `{"error": "invalid_request", "message": ...}`.
    Invalid(String),
    /// 429 `{"error": "report_limit", "message": "At most <n> reports per day.", "retryAfter": 3600}`
    /// with `Retry-After: 3600`.
    Limit { per_day: i64 },
    /// 403 `report_not_allowed`: not the opponent of the reporter in a game that ended within
    /// 7 days.
    NotAllowed,
    /// The store failed (the former server answered 500 `internal_error`; `busy` may be a 503).
    Failed(StoreError),
}

/// The message of [`ReportOutcome::NotAllowed`].
pub const NOT_ALLOWED_MESSAGE: &str =
    "You can report the opponent of one of your games that ended in the last 7 days.";

impl ReportOutcome {
    /// The HTTP status (500 for [`ReportOutcome::Failed`]).
    pub fn status(&self) -> u16 {
        match self {
            ReportOutcome::Accepted => 202,
            ReportOutcome::Invalid(_) => 400,
            ReportOutcome::Limit { .. } => 429,
            ReportOutcome::NotAllowed => 403,
            ReportOutcome::Failed(_) => 500,
        }
    }

    /// The JSON body (`None` for [`ReportOutcome::Failed`], left to the route's error handling).
    pub fn body(&self) -> Option<Value> {
        match self {
            ReportOutcome::Accepted => Some(json!({ "status": "received" })),
            ReportOutcome::Invalid(message) => {
                Some(json!({ "error": "invalid_request", "message": message }))
            }
            ReportOutcome::Limit { per_day } => Some(json!({ "error": "report_limit",
                "message": format!("At most {per_day} reports per day."), "retryAfter": 3600 })),
            ReportOutcome::NotAllowed => {
                Some(json!({ "error": "report_not_allowed", "message": NOT_ALLOWED_MESSAGE }))
            }
            ReportOutcome::Failed(_) => None,
        }
    }

    /// The `Retry-After` header, in seconds.
    pub fn retry_after(&self) -> Option<u64> {
        matches!(self, ReportOutcome::Limit { .. }).then_some(3600)
    }
}

/// A report stored, for the log line written after the commit.
struct Filed {
    id: i64,
    reported: UserId,
    weight: f64,
    raw_weight: f64,
}

enum Filing {
    Answer(ReportOutcome),
    Filed(Filed),
}

/// The reports service (module documentation). Cheap to clone.
#[derive(Clone)]
pub struct Reports {
    store: Store,
    clock: SharedClock,
    logger: Logger,
    per_day: i64,
}

impl std::fmt::Debug for Reports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reports").field("per_day", &self.per_day).finish_non_exhaustive()
    }
}

impl Reports {
    /// The service of a server (`REPORTS_PER_DAY` from `config`).
    pub fn new(config: &Config, store: Store, clock: SharedClock) -> Reports {
        Reports { store, clock, logger: Logger::root().child("anticheat"), per_day: config.reports_per_day }
    }

    /// Handles a report filed by `reporter` (`POST /api/v1/reports`, the body as parsed JSON),
    /// in one store write job: the `REPORTS_PER_DAY` quota, the game and its opponent, the
    /// duplicate check, the reporter's weight capped against brigading, the report, and the
    /// analysis request of the game (`report` priority for a credible report, `signal` for a
    /// low-credibility one, none for an abuse report).
    pub async fn file(&self, reporter: UserId, body: &Value) -> ReportOutcome {
        let req = match validate_report(body) {
            Ok(r) => r,
            Err(message) => return ReportOutcome::Invalid(message),
        };
        let now = self.clock.wall_ms();
        let (per_day, logger) = (self.per_day, self.logger.clone());
        let (game_id, category) = (req.game_id, req.category);
        let job = self.store.write(move |db| file_report(db, reporter, &req, per_day, now, &logger));
        match job.await {
            Ok(Filing::Answer(answer)) => answer,
            Ok(Filing::Filed(f)) => {
                log_security!(self.logger, "report.filed", { "reportId": f.id, "reporterId": reporter,
                    "reportedId": f.reported, "gameId": game_id, "category": category.as_str(),
                    "weight": f.weight, "rawWeight": f.raw_weight });
                ReportOutcome::Accepted
            }
            Err(e) => ReportOutcome::Failed(e),
        }
    }

    /// Whether [`Reports::file`] would take a new report from `user` against the opponent of `game`
    /// now (`GET /api/v1/games/:id` `reportable`): the game qualifies, the user is under
    /// `REPORTS_PER_DAY` and has not reported that opponent for that game yet. Any store failure
    /// answers `false`.
    pub async fn can_report(&self, user: UserId, game: &GameSummary) -> bool {
        let now = self.clock.wall_ms();
        let Some(opponent) = reportable_opponent(game, user, now) else { return false };
        let (per_day, game_id) = (self.per_day, game.id);
        self.store
            .read(move |db| {
                Ok::<_, StoreError>(
                    db.reports().count_by_reporter_since(user, now - DAY_MS)? < per_day
                        && !db.reports().exists(user, opponent.id, Some(game_id))?,
                )
            })
            .await
            .unwrap_or(false)
    }
}

/// The write job of [`Reports::file`].
fn file_report(
    db: &Db<'_>,
    reporter: UserId,
    req: &ReportRequest,
    per_day: i64,
    now: i64,
    logger: &Logger,
) -> Result<Filing, StoreError> {
    if db.reports().count_by_reporter_since(reporter, now - DAY_MS)? >= per_day {
        return Ok(Filing::Answer(ReportOutcome::Limit { per_day }));
    }
    let game = db.games().by_id(req.game_id)?;
    let Some(opponent) = game.and_then(|g| reportable_opponent(&g.summary, reporter, now)) else {
        return Ok(Filing::Answer(ReportOutcome::NotAllowed));
    };
    let want = req.reported.to_lowercase();
    // The opponent may have been renamed since the game.
    let matches = opponent.name.to_lowercase() == want
        || db.users().by_id(opponent.id).ok().flatten().is_some_and(|u| u.username.to_lowercase() == want);
    if !matches {
        return Ok(Filing::Answer(ReportOutcome::NotAllowed));
    }
    if db.reports().exists(reporter, opponent.id, Some(req.game_id))? {
        return Ok(Filing::Answer(ReportOutcome::Accepted));
    }

    let (mut actioned, mut dismissed) = (0, 0);
    for r in db.reports().for_reporter(reporter, TRACK_RECORD_REPORTS).unwrap_or_default() {
        match r.outcome() {
            Some(ReportStatus::Actioned) => actioned += 1,
            Some(ReportStatus::Dismissed) => dismissed += 1,
            _ => {}
        }
    }
    let raw = reporter_weight(&Reporter {
        created_at: db.users().by_id(reporter).ok().flatten().map(|u| u.created_at),
        games_played: db
            .ratings()
            .for_user(reporter)
            .unwrap_or_default()
            .iter()
            .map(|r| r.record.games)
            .sum(),
        actioned,
        dismissed,
        level: level_of(db, reporter),
        now,
    });
    let today = db.reports().weight_since(opponent.id, now - DAY_MS, LOW_CREDIBILITY).unwrap_or_default();
    let weight = capped_weight_of_sums(raw.weight, today.total, today.low);
    let created = db.reports().create(&NewReport {
        reporter_id: reporter,
        reported_id: opponent.id,
        game_id: Some(req.game_id),
        category: req.category,
        comment: Some(req.comment.clone()),
        weight,
        at: now,
    });
    let id = match created {
        Ok(id) => id,
        // The same report filed at the same moment: the UNIQUE index kept one.
        Err(e) if e.kind() == ErrorKind::Duplicate => return Ok(Filing::Answer(ReportOutcome::Accepted)),
        Err(e) => return Err(e),
    };
    // The reported game is analysed ahead of the ordinary ones, even when the queue policy left it
    // out: at report priority when the report is credible, otherwise beside the statistical
    // suspicion signals and not ahead of them. An abuse report is not about how the game was
    // played. This only produces evidence for moderators.
    if req.category != ReportCategory::Abuse {
        let priority = if weight >= LOW_CREDIBILITY { Priority::Report } else { Priority::Signal };
        if let Err(e) = db.transaction(|db| db.analysis().request(req.game_id, priority, now)) {
            log_warn!(logger, "analysis request of a reported game failed", { "err": crate::log::error(&e),
                "gameId": req.game_id });
        }
    }
    Ok(Filing::Filed(Filed { id, reported: opponent.id, weight, raw_weight: raw.weight }))
}

#[cfg(test)]
mod tests;
