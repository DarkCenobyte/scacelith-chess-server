//! Player reports: eligibility rules, reporter credibility, the anti-brigading cap and the review
//! priority, plus the service behind `POST /api/v1/reports` ([`Reports::file`]) and the
//! `reportable` flag of `GET /api/v1/games/:id` ([`Reports::can_report`]), which the routes reach
//! through [`ReportDesk`] (the route checks the body with
//! [`validate_report`](crate::http::routes::reports::validate_report) and answers the outcome).
//!
//! A report never changes a player's integrity level; it only raises the review priority that
//! moderators see (`admin reports`, `admin integrity list`), in proportion to the reporter's
//! credibility, and asks for the engine analysis of the reported game ahead of the ordinary games
//! (a low-credibility report only at the priority of a suspicion signal). Many reports from new or
//! low-credibility accounts do not add up: the weight stored for a report is capped so that the
//! reports received by one player in 24 hours sum to at most [`rules::DAILY_WEIGHT_CAP`], and
//! the low-credibility ones to at most [`rules::LOW_CRED_DAILY_CAP`].

use super::integrity::IntegrityLevel;
use super::players::level_of;
use crate::config::Config;
use crate::http::router::BoxFuture;
use crate::http::routes::reports::{ReportDesk, ReportOutcome, ReportRequest};
use crate::http::{ApiError, AuthInfo};
use crate::ids::UserId;
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

/// A report stored, for the log line written after the commit.
struct Filed {
    id: i64,
    reported: UserId,
    weight: f64,
    raw_weight: f64,
}

/// What a filing did: answered without a new report, or stored one.
enum Filing {
    Answer(ReportOutcome),
    Filed(Filed),
}

/// The reports service (module documentation). Cheap to clone.
#[derive(Clone)]
pub struct Reports {
    store: Store,
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
    pub fn new(config: &Config, store: Store) -> Reports {
        Reports { store, logger: Logger::root().child("anticheat"), per_day: config.reports_per_day }
    }

    /// Files a report of `reporter` checked by the route, at `now`, in one store write job: the
    /// `REPORTS_PER_DAY` quota, the game and its opponent (also under a former name), the
    /// duplicate check (the same answer as a new report), the reporter's weight capped against
    /// brigading, the report, and the analysis request of the game (`report` priority for a
    /// credible report, `signal` for a low-credibility one, none for an abuse report). The
    /// `report.filed` security line is logged after the commit. A store failure is returned.
    pub async fn file(
        &self,
        reporter: UserId,
        req: ReportRequest,
        now: i64,
    ) -> Result<ReportOutcome, StoreError> {
        let (per_day, logger) = (self.per_day, self.logger.clone());
        let (game_id, category) = (req.game_id, req.category);
        match self.store.write(move |db| file_report(db, reporter, &req, per_day, now, &logger)).await? {
            Filing::Answer(answer) => Ok(answer),
            Filing::Filed(f) => {
                log_security!(self.logger, "report.filed", { "reportId": f.id, "reporterId": reporter,
                    "reportedId": f.reported, "gameId": game_id, "category": category.as_str(),
                    "weight": f.weight, "rawWeight": f.raw_weight });
                Ok(ReportOutcome::Received)
            }
        }
    }

    /// Whether [`Reports::file`] would take a new report from `user` against the opponent of `game`
    /// at `now` (`GET /api/v1/games/:id` `reportable`): the game qualifies, the user is under
    /// `REPORTS_PER_DAY` and has not reported that opponent for that game yet. Any store failure
    /// answers `false`.
    pub async fn can_report(&self, user: UserId, game: &GameSummary, now: i64) -> bool {
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

/// The routes' view of the service: `POST /reports` answers the outcome (a store failure as 500
/// `internal_error`, as the former server did), `GET /games/:id` shows `reportable`. The app
/// passes the service to the routes as `Arc<dyn ReportDesk>`.
impl ReportDesk for Reports {
    fn file(
        &self,
        reporter: AuthInfo,
        report: ReportRequest,
        now_ms: i64,
    ) -> BoxFuture<Result<ReportOutcome, ApiError>> {
        let reports = self.clone();
        // `Reports::file` is the inherent method: it takes precedence over this one.
        Box::pin(async move {
            Reports::file(&reports, reporter.user_id, report, now_ms).await.map_err(ApiError::internal)
        })
    }

    fn can_report(&self, user: UserId, game: GameSummary, now_ms: i64) -> BoxFuture<bool> {
        let reports = self.clone();
        Box::pin(async move { Reports::can_report(&reports, user, &game, now_ms).await })
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
        return Ok(Filing::Answer(ReportOutcome::LimitReached));
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
        return Ok(Filing::Answer(ReportOutcome::Received));
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
        Err(e) if e.kind() == ErrorKind::Duplicate => return Ok(Filing::Answer(ReportOutcome::Received)),
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
