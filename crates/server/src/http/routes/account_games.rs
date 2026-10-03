//! The signed-in player's own game history (docs/API.md; the Node server's
//! `src/http/routes/account-games.js`), newest first, with filters and the number of games
//! matching them.
//!
//! ```text
//! GET /api/v1/account/games?before=<gameId>&limit=20&category=3%2B2&rated=true&result=win
//!   -> 200 { games: [summary], next: <gameId> | null, total }
//! ```
//!
//! Query (each optional; an empty value counts as absent, unknown parameters are ignored):
//! `before` (exclusive game id cursor: the previous `next`), `limit` (1..50, default 20, a larger
//! value capped), `category` (an official category id, a '+' decoded to a space accepted, or
//! `custom`), `rated` (`true` | `false`), `result` (`win` | `loss` | `draw` from the player's
//! side; aborted games only appear without it). Errors: 400 `invalid_cursor` | `invalid_limit` |
//! `invalid_filter` (with `field`), 401 without a session, 429 (`account_games`: 60 a minute per
//! player).
//!
//! A summary ([`history_summary`], which the data export reuses) is the summary of the player's
//! game lists plus `baseMs`, `incMs` and `outcome` (`win` | `loss` | `draw` | `aborted`). `next`
//! is the id of the page's last game when older games match the filter; `total` counts every game
//! matching it, all pages together.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::games::{game_summary, parse_game_id, parse_limit, store_failure};
use crate::config::Config;
use crate::http::{Answer, ApiError, AuthMode, Ctx, RateSpec, RouteOpts, Router};
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::store::{GameFilter, GameSummary, ResultFilter, Store, StoreError, js_trim, status};

/// Most games of a page.
const LIST_MAX: i64 = 50;
/// Games of a page without `limit`.
const LIST_DEFAULT: i64 = 20;

/// The game's outcome from the side of `user`: `win`, `loss`, `draw` or `aborted`.
pub fn outcome_for(g: &GameSummary, user: UserId) -> &'static str {
    match g.status {
        status::DRAW => "draw",
        status::WHITE_WINS if g.white_id == user => "win",
        status::WHITE_WINS => "loss",
        status::BLACK_WINS if g.white_id == user => "loss",
        status::BLACK_WINS => "win",
        _ => "aborted",
    }
}

/// The summary of a game in the player's history (and the data export): the list summary with
/// `color`, plus `baseMs`, `incMs` and `outcome`.
pub fn history_summary(g: &GameSummary, user: UserId) -> Map<String, Value> {
    let mut m = game_summary(g, Some(user));
    m.insert("baseMs".into(), g.base_ms.into());
    m.insert("incMs".into(), g.inc_ms.into());
    m.insert("outcome".into(), outcome_for(g, user).into());
    m
}

/// A query of `GET /account/games`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    /// Only games older than this id.
    pub before: Option<GameId>,
    /// Page size (1..=50).
    pub limit: i64,
    /// The filters.
    pub filter: GameFilter,
}

fn invalid(code: &'static str, message: String, field: &'static str) -> ApiError {
    ApiError::new(400, code, message).with_extra("field", field.into())
}

/// Parses the query of `GET /account/games` against the accepted category ids (`custom`
/// included), in the order before, limit, category, rated, result.
pub fn parse_history_query(ctx: &Ctx, categories: &[String]) -> Result<HistoryQuery, ApiError> {
    let param = |name: &str| ctx.query_str(name).filter(|s| !s.is_empty());
    let before = match param("before") {
        None => None,
        Some(raw) => Some(
            parse_game_id(raw)
                .ok_or_else(|| invalid("invalid_cursor", "before must be a game id.".into(), "before"))?,
        ),
    };
    let limit = match param("limit") {
        None => LIST_DEFAULT,
        Some(raw) => parse_limit(raw, LIST_MAX)
            .ok_or_else(|| invalid("invalid_limit", format!("limit must be 1 to {LIST_MAX}."), "limit"))?,
    };
    let mut filter = GameFilter::default();
    if let Some(raw) = param("category") {
        let category = js_trim(raw).replace(' ', "+");
        if !categories.contains(&category) {
            let message = format!("category must be one of: {}.", categories.join(", "));
            return Err(invalid("invalid_filter", message, "category"));
        }
        filter.category = Some(category);
    }
    if let Some(raw) = param("rated") {
        filter.rated = Some(match raw {
            "true" => true,
            "false" => false,
            _ => return Err(invalid("invalid_filter", "rated must be true or false.".into(), "rated")),
        });
    }
    if let Some(raw) = param("result") {
        let result = ResultFilter::parse(raw).ok_or_else(|| {
            invalid("invalid_filter", "result must be one of: win, loss, draw.".into(), "result")
        })?;
        filter.result = Some(result);
    }
    Ok(HistoryQuery { before, limit, filter })
}

/// The services of the account history route.
#[derive(Clone)]
pub struct AccountGamesDeps {
    /// The configuration (official categories).
    pub config: Arc<Config>,
    /// The store.
    pub store: Store,
    /// The logger of the API.
    pub log: Logger,
}

/// Registers `GET /account/games`.
pub fn register(router: &mut Router, deps: AccountGamesDeps) {
    let categories: Vec<String> =
        deps.config.categories.iter().map(|c| c.id.clone()).chain(["custom".to_string()]).collect();
    let state = Arc::new((deps, categories));
    let opts = RouteOpts::new()
        .auth(AuthMode::Required)
        .rate(RateSpec::new("account_games", 60.0, 60_000).by_user());
    router.get("/account/games", opts, move |ctx| history(state.clone(), ctx));
}

async fn history(state: Arc<(AccountGamesDeps, Vec<String>)>, ctx: Ctx) -> Result<Answer, ApiError> {
    let (deps, categories) = &*state;
    let q = parse_history_query(&ctx, categories)?;
    let user = ctx.user_id().ok_or_else(|| ApiError::internal("account games without a session"))?;
    let read = deps.store.read(move |db| -> Result<Value, StoreError> {
        // One game more than the page tells whether another page follows.
        let mut list = db.games().list_for_user(user, q.before, q.limit + 1, &q.filter)?;
        let more = list.len() as i64 > q.limit;
        list.truncate(usize::try_from(q.limit).unwrap_or(0));
        let next = match list.last() {
            Some(last) if more => Value::from(last.id),
            _ => Value::Null,
        };
        let total = db.games().count_for_user(user, Some(&q.filter))?;
        let games: Vec<Value> = list.iter().map(|g| Value::Object(history_summary(g, user))).collect();
        Ok(json!({ "games": games, "next": next, "total": total }))
    });
    match read.await {
        Ok(body) => Ok(Answer::json(body)),
        Err(e) => store_failure(&deps.log, "account games", e),
    }
}

#[cfg(test)]
mod tests;
