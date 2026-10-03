//! Player profiles and their recent games (DESIGN 5.9, docs/API.md; the Node server's
//! `src/http/routes/players.js`). Public data only: never an e-mail address, a session, a
//! sanction, an anomaly or an integrity level. Deleted (anonymized) accounts have no profile;
//! their games stay readable under the anonymized name.
//!
//! ```text
//! GET /api/v1/players/:username        profile: ratings per category, game counts
//! GET /api/v1/players/:username/games  recent games, newest first (?before=<gameId>&limit<=50)
//! ```
//!
//! Both take an optional session and share the `public_read` limit with the game routes.

use std::sync::Arc;

use serde_json::{Value, json};

use super::games::{game_summary, parse_game_id, parse_limit, public_read_rate, store_failure};
use crate::config::Config;
use crate::http::url::decode_uri_component;
use crate::http::{Answer, ApiError, AuthMode, Ctx, RouteOpts, Router};
use crate::ids::GameId;
use crate::log::Logger;
use crate::store::{Db, Store, StoreError, User, UserStatus};

/// Most games of a page.
const LIST_MAX: i64 = 50;
/// Games of a page without `limit`.
const LIST_DEFAULT: i64 = 20;

/// The services of the player routes.
#[derive(Clone)]
pub struct PlayersDeps {
    /// The configuration (official categories).
    pub config: Arc<Config>,
    /// The store.
    pub store: Store,
    /// The logger of the API.
    pub log: Logger,
}

/// Registers `GET /players/:username` and `GET /players/:username/games`.
pub fn register(router: &mut Router, deps: PlayersDeps) {
    let deps = Arc::new(deps);
    let opts = || RouteOpts::new().auth(AuthMode::Optional).rate(public_read_rate());
    let d = deps.clone();
    router.get("/players/:username", opts(), move |ctx| profile(d.clone(), ctx));
    router.get("/players/:username/games", opts(), move |ctx| games_of(deps.clone(), ctx));
}

/// The widest username the server ever allowed: `^[A-Za-z0-9_.-]{2,24}$`.
fn plausible_username(name: &str) -> bool {
    (2..=24).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// The username of the path (decoded once more when it still holds a `%`), or 400.
fn username_param(ctx: &Ctx) -> Result<String, ApiError> {
    let invalid = || ApiError::new(400, "invalid_username", "Invalid username.");
    let raw = ctx.param("username").ok_or_else(invalid)?;
    let name =
        if raw.contains('%') { decode_uri_component(raw).ok_or_else(invalid)? } else { raw.to_string() };
    if plausible_username(&name) { Ok(name) } else { Err(invalid()) }
}

fn no_such_player() -> ApiError {
    ApiError::not_found("No such player.")
}

/// The active account of `name` (case-insensitive).
fn active_player(db: &Db<'_>, name: &str) -> Result<Option<User>, StoreError> {
    Ok(db.users().by_username(name)?.filter(|u| u.status == UserStatus::Active))
}

async fn profile(deps: Arc<PlayersDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let name = username_param(&ctx)?;
    let config = deps.config.clone();
    let read = deps.store.read(move |db| -> Result<Option<Value>, StoreError> {
        let Some(user) = active_player(db, &name)? else { return Ok(None) };
        let mut rows = db.ratings().for_user(user.id)?;
        rows.retain(|r| config.category(&r.category).is_some());
        rows.sort_by_key(|r| config.categories.iter().position(|c| c.id == r.category));
        let (mut wins, mut draws, mut losses, mut rated) = (0, 0, 0, 0);
        let ratings: Vec<Value> = rows
            .iter()
            .map(|r| {
                let rec = &r.record;
                wins += rec.wins;
                draws += rec.draws;
                losses += rec.losses;
                rated += rec.games;
                json!({
                    "category": r.category,
                    "rating": rec.rating,
                    "provisional": r.provisional,
                    "games": rec.games,
                    "wins": rec.wins,
                    "draws": rec.draws,
                    "losses": rec.losses,
                    "peak": rec.peak,
                })
            })
            .collect();
        let total = db.games().count_for_user(user.id, None)?;
        Ok(Some(json!({
            "username": user.username,
            "createdAt": user.created_at,
            "ratings": ratings,
            "games": { "total": total, "rated": rated, "wins": wins, "draws": draws, "losses": losses },
        })))
    });
    match read.await {
        Ok(Some(body)) => Ok(Answer::json(body)),
        Ok(None) => Err(no_such_player()),
        Err(e) => store_failure(&deps.log, "profile", e),
    }
}

/// The page of `GET /players/:username/games`: `before` and `limit`, an empty value counting as
/// absent.
fn page_query(ctx: &Ctx) -> Result<(Option<GameId>, i64), ApiError> {
    let before = match ctx.query_str("before").filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) => Some(
            parse_game_id(raw)
                .ok_or_else(|| ApiError::new(400, "invalid_cursor", "before must be a game id."))?,
        ),
    };
    let limit = match ctx.query_str("limit").filter(|s| !s.is_empty()) {
        None => LIST_DEFAULT,
        Some(raw) => parse_limit(raw, LIST_MAX)
            .ok_or_else(|| ApiError::new(400, "invalid_limit", format!("limit must be 1 to {LIST_MAX}.")))?,
    };
    Ok((before, limit))
}

async fn games_of(deps: Arc<PlayersDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let name = username_param(&ctx)?;
    // The player is looked up first: an unknown player is a 404 whatever the query says.
    let page = page_query(&ctx);
    let read = deps.store.read(move |db| -> Result<Result<Value, ApiError>, StoreError> {
        let Some(user) = active_player(db, &name)? else { return Ok(Err(no_such_player())) };
        let (before, limit) = match page {
            Ok(page) => page,
            Err(e) => return Ok(Err(e)),
        };
        let list = db.games().recent_for_user(user.id, limit, before)?;
        let next = match list.last() {
            Some(last) if list.len() as i64 == limit => Value::from(last.id),
            _ => Value::Null,
        };
        let games: Vec<Value> = list.iter().map(|g| Value::Object(game_summary(g, Some(user.id)))).collect();
        Ok(Ok(json!({ "username": user.username, "games": games, "next": next })))
    });
    match read.await {
        Ok(Ok(body)) => Ok(Answer::json(body)),
        Ok(Err(e)) => Err(e),
        Err(e) => store_failure(&deps.log, "games", e),
    }
}

#[cfg(test)]
mod tests;
