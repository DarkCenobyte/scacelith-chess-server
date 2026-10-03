//! `POST /api/v1/reports`: a player reports the opponent of one of their recent games (DESIGN 5.9,
//! docs/API.md; the Node server's `src/http/routes/reports.js` and the request half of
//! `src/anticheat/reports.js`).
//!
//! Body: `{ gameId, reported (username), category: cheating | abuse | other, comment? (<= 500) }`.
//! Answers: 202 `{ status: "received" }` (accepted, or already reported: the same answer, which
//! says nothing about the reported account), 400 `invalid_request`, 403 `report_not_allowed` (not
//! an opponent of the reporter in a game that ended within 7 days), 429 `report_limit`
//! (`REPORTS_PER_DAY`, with `Retry-After: 3600`). The route checks the body itself
//! ([`validate_report`]: `gameId` as a number or a digit string); the eligibility rules, the
//! reporter's credibility, the caps and the analysis request belong to the reports service of the
//! anti-cheat module, reached through [`ReportDesk`]. A coarse limit of 30 requests an hour per
//! player (`reports`) comes first.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::Config;
use crate::http::json::{MAX_SAFE_INTEGER, safe_integer};
use crate::http::router::BoxFuture;
use crate::http::{Answer, ApiError, AuthInfo, AuthMode, RateSpec, RouteOpts, Router};
use crate::ids::{GameId, UserId};
use crate::store::{GameSummary, ReportCategory, js_trim};

/// The categories of a report, in the order of the error message.
pub const REPORT_CATEGORIES: [&str; 3] = ["cheating", "abuse", "other"];
/// Longest comment, in characters (code points) after cleaning.
pub const COMMENT_MAX: usize = 500;
/// Longest `reported` value, in UTF-16 code units.
pub const REPORTED_MAX: usize = 24;

// ------------------------------------------------------------------------------------------------
// Adapter to the anti-cheat reports service.
//
// The routes reach the reports service (`handleReport` / `canReport` of the Node server's
// anticheat/reports.js) through this trait. `crate::anticheat::reports::Reports` implements it;
// the server passes that service to the routes as `Arc<dyn ReportDesk>` when it builds them.
// ------------------------------------------------------------------------------------------------

/// What the reports service did with a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportOutcome {
    /// Accepted, or already filed by the same reporter for the same game: 202.
    Received,
    /// The reporter filed `REPORTS_PER_DAY` reports in the last 24 hours: 429 `report_limit`.
    LimitReached,
    /// The game is not one the reporter played against `reported` and that ended within the last
    /// 7 days: 403 `report_not_allowed`.
    NotAllowed,
}

/// The reports service as the routes use it.
pub trait ReportDesk: Send + Sync + 'static {
    /// Files a report checked by [`validate_report`] (`handleReport` past the body check): the
    /// daily quota, the eligibility of the game and opponent (also under a former name), the
    /// duplicate check, the credibility weight and its caps, the security log line and the
    /// analysis request. A failure of the store is the error to answer.
    fn file(
        &self,
        reporter: AuthInfo,
        report: ReportRequest,
        now_ms: i64,
    ) -> BoxFuture<Result<ReportOutcome, ApiError>>;

    /// Whether [`ReportDesk::file`] would take a new report from `user` against the opponent of
    /// `game` now (`canReport`: the game qualifies, the reporter is under the daily quota and has
    /// not reported that opponent for that game yet). A failure of the store answers `false`.
    fn can_report(&self, user: UserId, game: GameSummary, now_ms: i64) -> BoxFuture<bool>;
}

// ------------------------------------------------------------------------------------------------
// End of the adapter.
// ------------------------------------------------------------------------------------------------

/// A report as the route checked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRequest {
    /// The game the report is about.
    pub game_id: GameId,
    /// The reported player's username as the reporter typed it, trimmed.
    pub reported: String,
    /// What the report is about.
    pub category: ReportCategory,
    /// The comment without control characters, trimmed (empty when none).
    pub comment: String,
}

fn bad(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::invalid_request(message)
}

/// The game id of a report: a safe positive integer, or a string of 1 to 16 digits.
fn report_game_id(v: Option<&Value>) -> Option<GameId> {
    let n = match v? {
        Value::String(s) if (1..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse::<i64>().ok().filter(|&n| n <= MAX_SAFE_INTEGER)?
        }
        v => safe_integer(v)?,
    };
    GameId::try_from(n).ok().filter(|&id| id > 0)
}

/// JavaScript's `[\u0000-\u0008\u000b-\u001f\u007f]`: the control characters but tab and line feed.
fn is_dropped_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}')
}

/// Checks a report body (`validateReport`), in the order gameId, reported, category, comment.
pub fn validate_report(body: &Value) -> Result<ReportRequest, ApiError> {
    let Value::Object(b) = body else { return Err(bad("A JSON object is expected.")) };
    let game_id = report_game_id(b.get("gameId")).ok_or_else(|| bad("gameId must be a game id."))?;
    let reported = match b.get("reported") {
        Some(Value::String(s)) if !js_trim(s).is_empty() && s.encode_utf16().count() <= REPORTED_MAX => {
            js_trim(s).to_string()
        }
        _ => return Err(bad("reported must be a username.")),
    };
    let category = b
        .get("category")
        .and_then(Value::as_str)
        .filter(|c| REPORT_CATEGORIES.contains(c))
        .and_then(ReportCategory::parse)
        .ok_or_else(|| bad(format!("category must be one of {}.", REPORT_CATEGORIES.join(", "))))?;
    let comment = match b.get("comment") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => {
            let cleaned: String = s.chars().filter(|&c| !is_dropped_control(c)).collect();
            js_trim(&cleaned).to_string()
        }
        Some(_) => return Err(bad("comment must be text.")),
    };
    if comment.chars().count() > COMMENT_MAX {
        return Err(bad(format!("comment is limited to {COMMENT_MAX} characters.")));
    }
    Ok(ReportRequest { game_id, reported, category, comment })
}

/// The services of the reports route.
#[derive(Clone)]
pub struct ReportsDeps {
    /// The configuration (`REPORTS_PER_DAY`).
    pub config: Arc<Config>,
    /// The reports service.
    pub desk: Arc<dyn ReportDesk>,
}

/// Registers `POST /reports`.
pub fn register(router: &mut Router, deps: ReportsDeps) {
    let deps = Arc::new(deps);
    let opts = RouteOpts::new()
        .auth(AuthMode::Required)
        .own_body_validation()
        // Coarse per-player cap on requests; the daily report quota is the service's.
        .rate(RateSpec::new("reports", 30.0, 3_600_000).by_user());
    router.post("/reports", opts, move |ctx| {
        let deps = deps.clone();
        async move {
            let report = validate_report(&ctx.body)?;
            let reporter =
                ctx.auth.clone().ok_or_else(|| ApiError::internal("a report without a session"))?;
            match deps.desk.file(reporter, report, ctx.now_ms).await? {
                ReportOutcome::Received => Ok(Answer::json(json!({ "status": "received" })).status(202)),
                ReportOutcome::LimitReached => Err(ApiError::new(
                    429,
                    "report_limit",
                    format!("At most {} reports per day.", deps.config.reports_per_day),
                )
                // The framework adds `Retry-After: 3600` from `retryAfter`.
                .with_extra("retryAfter", 3600.into())),
                ReportOutcome::NotAllowed => Err(ApiError::new(
                    403,
                    "report_not_allowed",
                    "You can report the opponent of one of your games that ended in the last 7 days.",
                )),
            }
        }
    });
}

#[cfg(test)]
mod tests;
