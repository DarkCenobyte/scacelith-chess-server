//! The leaderboards (DESIGN 5.9, docs/API.md; the Node server's `src/http/routes/players.js`).
//!
//! ```text
//! GET /api/v1/leaderboard?category=3+2&limit<=100   top established players of a category
//! ```
//!
//! No session and no limit of its own: the board of each category is read at most once every
//! 10 seconds (the cache keeps the 100 best players; `limit` cuts the cached list). A player is
//! listed once they have `PROVISIONAL_GAMES` games in the category.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::games::{parse_limit, store_failure};
use crate::config::Config;
use crate::http::{Answer, ApiError, AuthMode, Ctx, RouteOpts, Router};
use crate::log::Logger;
use crate::store::{Store, js_trim};

/// Most players of a board.
const BOARD_MAX: i64 = 100;
/// How long a board is served from the cache, in milliseconds.
const BOARD_CACHE_MS: i64 = 10_000;

/// The services of the leaderboard route.
#[derive(Clone)]
pub struct LeaderboardDeps {
    /// The configuration (official categories, `PROVISIONAL_GAMES`).
    pub config: Arc<Config>,
    /// The store.
    pub store: Store,
    /// The logger of the API.
    pub log: Logger,
}

/// A board as it was read.
struct Board {
    /// When it was read (the request's wall clock).
    at: i64,
    /// `{rank, username, rating, games, wins, draws, losses, peak}` of up to 100 players.
    players: Arc<Vec<Value>>,
}

struct Leaderboards {
    deps: LeaderboardDeps,
    cache: Mutex<HashMap<String, Board>>,
}

/// Registers `GET /leaderboard`.
pub fn register(router: &mut Router, deps: LeaderboardDeps) {
    let boards = Arc::new(Leaderboards { deps, cache: Mutex::new(HashMap::new()) });
    router.get("/leaderboard", RouteOpts::new().auth(AuthMode::None), move |ctx| {
        leaderboard(boards.clone(), ctx)
    });
}

async fn leaderboard(boards: Arc<Leaderboards>, ctx: Ctx) -> Result<Answer, ApiError> {
    let config = &boards.deps.config;
    // "3+2" in a query string decodes to "3 2" (form encoding): both spellings are accepted.
    let category = ctx.query_str("category").map(|c| js_trim(c).replace(' ', "+")).unwrap_or_default();
    if category.is_empty() || config.category(&category).is_none() {
        let ids: Vec<&str> = config.categories.iter().map(|c| c.id.as_str()).collect();
        return Err(ApiError::new(
            400,
            "invalid_category",
            format!("category must be one of: {}.", ids.join(", ")),
        ));
    }
    let limit = match ctx.query_str("limit").filter(|s| !s.is_empty()) {
        None => BOARD_MAX,
        Some(raw) => parse_limit(raw, BOARD_MAX)
            .ok_or_else(|| ApiError::new(400, "invalid_limit", format!("limit must be 1 to {BOARD_MAX}.")))?,
    };
    let t = ctx.now_ms;
    let cached = boards
        .cache
        .lock()
        .get(&category)
        .filter(|b| t - b.at <= BOARD_CACHE_MS)
        .map(|b| (b.at, b.players.clone()));
    let (at, players) = match cached {
        Some(hit) => hit,
        None => {
            let min_games = config.provisional_games;
            let c = category.clone();
            let rows = match boards
                .deps
                .store
                .read(move |db| db.ratings().leaderboard(&c, BOARD_MAX, Some(min_games)))
                .await
            {
                Ok(rows) => rows,
                Err(e) => return store_failure(&boards.deps.log, "leaderboard", e),
            };
            let players: Vec<Value> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    json!({
                        "rank": i + 1,
                        "username": r.username,
                        "rating": r.rating,
                        "games": r.games,
                        "wins": r.wins,
                        "draws": r.draws,
                        "losses": r.losses,
                        "peak": r.peak,
                    })
                })
                .collect();
            let players = Arc::new(players);
            boards.cache.lock().insert(category.clone(), Board { at: t, players: players.clone() });
            (t, players)
        }
    };
    let shown: Vec<Value> = players.iter().take(usize::try_from(limit).unwrap_or(0)).cloned().collect();
    Ok(Answer::json(json!({
        "category": category,
        "minGames": config.provisional_games,
        "updatedAt": at,
        "players": shown,
    })))
}

#[cfg(test)]
mod tests;
