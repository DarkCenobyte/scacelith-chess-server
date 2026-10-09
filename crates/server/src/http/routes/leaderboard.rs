//! The leaderboards (DESIGN 5.9, docs/API.md; the Node server's `src/http/routes/players.js`).
//!
//! ```text
//! GET /api/v1/leaderboard?category=3+2&limit<=100   top established players of a category
//! ```
//!
//! No session and no limit of its own: the board of each category is read at most once every
//! 10 seconds (the cache keeps the 100 best players; `limit` cuts the cached list), one read at a
//! time, in a task of its own (a request that goes away does not stop it). A request gets the
//! cached board while it is fresh, or while another request's read of the next one runs; with no
//! board yet it waits for that read, and with an old board and no read running it starts one and
//! waits for it. A failed read answers the requests that waited for it, and the next request reads
//! again. A player is listed once they have `PROVISIONAL_GAMES` games in the category.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::games::{busy_answer, parse_limit, store_failure};
use crate::config::Config;
use crate::http::router::BoxFuture;
use crate::http::{Answer, ApiError, AuthMode, Ctx, RouteOpts, Router};
use crate::log::Logger;
use crate::store::{LeaderboardRow, Store, StoreError, js_trim};

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
#[derive(Clone)]
struct Board {
    /// When it was read (the wall clock of the request that started the read).
    at: i64,
    /// `{rank, username, rating, games, wins, draws, losses, peak}` of up to 100 players.
    players: Arc<Vec<Value>>,
}

/// How a read failed, for every request that waited for it (logged once, by the read).
#[derive(Clone)]
enum Failure {
    /// The store stayed locked: 503 `busy`.
    Busy,
    /// Any other failure: 500.
    Error(ApiError),
}

/// How a read ended, for every request that waited for it.
type Outcome = Result<Board, Failure>;

/// Reads the rows of the board of a category: the store's query (tests slow it and count it).
type ReadRows = Arc<dyn Fn(String) -> BoxFuture<Result<Vec<LeaderboardRow>, StoreError>> + Send + Sync>;

/// The board of a category: the last one read, and the read running now.
#[derive(Default)]
struct Slot {
    board: Option<Board>,
    /// Its outcome is `None` until the read ends.
    reading: Option<watch::Receiver<Option<Outcome>>>,
}

struct Leaderboards {
    deps: LeaderboardDeps,
    read_rows: ReadRows,
    /// Never held across an await.
    slots: Mutex<HashMap<String, Slot>>,
}

/// Registers `GET /leaderboard`.
pub fn register(router: &mut Router, deps: LeaderboardDeps) {
    let (store, min_games) = (deps.store.clone(), deps.config.provisional_games);
    let read_rows: ReadRows = Arc::new(move |category: String| {
        Box::pin(store.read(move |db| db.ratings().leaderboard(&category, BOARD_MAX, Some(min_games))))
    });
    register_with(router, deps, read_rows);
}

/// Registers `GET /leaderboard` over `read_rows`.
fn register_with(router: &mut Router, deps: LeaderboardDeps, read_rows: ReadRows) {
    let boards = Arc::new(Leaderboards { deps, read_rows, slots: Mutex::new(HashMap::new()) });
    router.get("/leaderboard", RouteOpts::new().auth(AuthMode::None), move |ctx| {
        leaderboard(boards.clone(), ctx)
    });
}

/// The JSON players of board rows.
fn players_of(rows: &[LeaderboardRow]) -> Vec<Value> {
    rows.iter()
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
        .collect()
}

/// The read of a category's board while its task runs: when the task ends, however it ends,
/// the slot forgets this read (and keeps the board it read).
struct InFlight {
    boards: Arc<Leaderboards>,
    category: String,
    rx: watch::Receiver<Option<Outcome>>,
}

impl InFlight {
    /// Stores the board read, if any, and forgets this read, in one critical section.
    fn end(&self, board: Option<Board>) {
        let mut slots = self.boards.slots.lock();
        if let Some(slot) = slots.get_mut(&self.category) {
            if let Some(board) = board {
                slot.board = Some(board);
            }
            if slot.reading.as_ref().is_some_and(|r| r.same_channel(&self.rx)) {
                slot.reading = None;
            }
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.end(None);
    }
}

impl Leaderboards {
    /// The board of `category` for a request at `t` (module documentation).
    async fn board(self: &Arc<Self>, category: &str, t: i64) -> Outcome {
        let mut rx = {
            let mut slots = self.slots.lock();
            let slot = slots.entry(category.to_owned()).or_default();
            let serve = slot.board.as_ref().filter(|b| t - b.at <= BOARD_CACHE_MS || slot.reading.is_some());
            if let Some(board) = serve {
                return Ok(board.clone());
            }
            match &slot.reading {
                Some(rx) => rx.clone(),
                None => {
                    let rx = self.start_read(category, t);
                    slot.reading = Some(rx.clone());
                    rx
                }
            }
        };
        let outcome = rx.wait_for(Option::is_some).await.map(|done| done.clone());
        match outcome {
            Ok(Some(outcome)) => outcome,
            // The task ended without an outcome (the runtime is shutting down).
            _ => Err(Failure::Error(ApiError::internal("the leaderboard read was abandoned"))),
        }
    }

    /// Starts the read of a board in a task of its own, so that it ends even when the request
    /// that started it goes away.
    fn start_read(self: &Arc<Self>, category: &str, t: i64) -> watch::Receiver<Option<Outcome>> {
        let (tx, rx) = watch::channel(None);
        let flight = InFlight { boards: self.clone(), category: category.to_owned(), rx: rx.clone() };
        tokio::spawn(async move {
            let boards = flight.boards.clone();
            let outcome = match (boards.read_rows)(flight.category.clone()).await {
                Ok(rows) => Ok(Board { at: t, players: Arc::new(players_of(&rows)) }),
                Err(e) => Err(match store_failure(&boards.deps.log, "leaderboard", e) {
                    Ok(_) => Failure::Busy,
                    Err(e) => Failure::Error(e),
                }),
            };
            flight.end(outcome.as_ref().ok().cloned());
            tx.send_replace(Some(outcome));
        });
        rx
    }
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
    let board = match boards.board(&category, ctx.now_ms).await {
        Ok(board) => board,
        Err(Failure::Busy) => return Ok(busy_answer()),
        Err(Failure::Error(e)) => return Err(e),
    };
    let shown: Vec<Value> = board.players.iter().take(usize::try_from(limit).unwrap_or(0)).cloned().collect();
    Ok(Answer::json(json!({
        "category": category,
        "minGames": config.provisional_games,
        "updatedAt": board.at,
        "players": shown,
    })))
}

#[cfg(test)]
mod tests;
