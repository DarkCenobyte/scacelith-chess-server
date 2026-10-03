//! Animated GIF of a game (docs/API.md section 11, DESIGN 5.9; the Node server's
//! `src/http/routes/gif.js`): the game played move by move on a board seen from above, answered
//! as the .gif file itself. The rendering threads, the cache and the renders in flight are the
//! [`GifService`]'s.
//!
//! ```text
//! GET  /api/v1/games/:id/gif?size=small|medium|large&orientation=white|black&delay=<ms>&coords=0|1
//!      a game of this server, with the names and ratings of its record
//! POST /api/v1/gif { pgn, size?, orientation?, delayMs?, coords? (boolean) }
//!      the first game of a PGN text (at most 65536 bytes of UTF-8)
//! ```
//!
//! Answers: 200 `image/gif` with `Content-Disposition: attachment; filename="scacelith-<id>.gif"`
//! (GET) or `"scacelith-game.gif"` (POST). Errors: 401 (both need a session: the quotas count
//! per account), 400 `invalid_option` {field}, 400 `invalid_game_id`, 404 `not_found`, 400
//! `invalid_request` {field} (POST body: unknown field, missing pgn), 400 `invalid_pgn` {line,
//! column}, 422 `game_too_long` (more than `GIF_MAX_PLIES` plies), 429 `rate_limited` (a quota),
//! 503 `server_busy` {retryAfter} (the queue is full or the wait ran out: every token of the
//! request is given back), 500 `render_failed`, 404 `gif_disabled` (`GIF_ENABLED=false`).
//!
//! Quotas: the route's `gif` limit (30 a minute per account, both routes together) on every
//! call; the render quotas ([`render_quotas`]) only when the GIF is neither cached nor being
//! rendered for another request.

use std::sync::Arc;
use std::time::Duration;

use scacelith_chess::{EndReason as ChessReason, PGN_LIMITS, PgnLimits, normalize_result, read_pgn};
use scacelith_gif::GameResult;
use serde_json::Value;

use super::games::{busy_answer, invalid_game_id, no_such_game, parse_game_id, result_text};
use crate::config::Config;
use crate::gifsvc::{
    DeliverError, GIF_BODY_FIELDS, GIF_BODY_LIMIT_BYTES, GIF_CONTENT_TYPE, GIF_DISABLED_MESSAGE,
    GIF_PGN_MAX_BYTES, GIF_ROUTE_RATE, GifError, GifJob, GifService, InvalidOption, JobPlayer, QueryOptions,
    QuotaSpec, RENDER_FAILED_MESSAGE, SERVER_BUSY_MESSAGE, busy_retry_after_secs, game_too_long_message,
    handler_timeout_ms, max_plies, options_from_body, options_from_query, render_quotas, tag_ending,
    tag_rating, tag_text,
};
use crate::http::json::js_keys;
use crate::http::{Answer, ApiError, AuthMode, Ctx, RateSpec, RouteOpts, Router};
use crate::log::Logger;
use crate::log_warn;
use crate::store::{ErrorKind, Store};

/// Longest player name taken from a PGN tag.
const TAG_NAME_MAX: usize = 48;

/// The services of the GIF routes.
#[derive(Clone)]
pub struct GifDeps {
    /// The configuration (`GIF_*`).
    pub config: Arc<Config>,
    /// The store (game records).
    pub store: Store,
    /// The GIF service (in the server: `GifService::new(&config, Arc::new(GameRenderer::<ChessRules>::new()))`).
    /// The routes close it when the API closes.
    pub gifs: GifService,
    /// The logger of the API.
    pub log: Logger,
}

/// A GIF quota as the HTTP framework takes it.
fn rate_of(q: &QuotaSpec) -> RateSpec {
    let mut rate = RateSpec::new(q.key, f64::from(q.limit), q.window_ms);
    if q.by_user {
        rate = rate.by_user();
    }
    if q.shared {
        rate = rate.shared();
    }
    if let Some(limit) = q.prefix_limit {
        rate = rate.prefix_limit(f64::from(limit));
    }
    rate
}

struct Gifs {
    deps: GifDeps,
    /// The render quotas, taken only for a new render.
    render_rates: Vec<RateSpec>,
    max_plies: usize,
}

/// Registers `GET /games/:id/gif` and `POST /gif`, and closes the GIF service with the API.
pub fn register(router: &mut Router, deps: GifDeps) {
    let config = deps.config.clone();
    let gifs = deps.gifs.clone();
    let state = Arc::new(Gifs {
        render_rates: render_quotas(&config).iter().map(rate_of).collect(),
        max_plies: max_plies(&config),
        deps,
    });
    let opts = || {
        RouteOpts::new()
            .auth(AuthMode::Required)
            .rate(rate_of(&GIF_ROUTE_RATE))
            .timeout(Duration::from_millis(handler_timeout_ms(&config)))
    };
    let s = state.clone();
    router.get("/games/:id/gif", opts(), move |ctx| game_gif(s.clone(), ctx));
    router.post(
        "/gif",
        // The PGN as a JSON string: escaping may double its bytes.
        opts().own_body_validation().body_limit(GIF_BODY_LIMIT_BYTES),
        move |ctx| pgn_gif(state.clone(), ctx),
    );
    router.on_close(move || async move { gifs.close() });
}

fn disabled() -> ApiError {
    ApiError::new(404, "gif_disabled", GIF_DISABLED_MESSAGE).refund_rate()
}

fn invalid_option(e: InvalidOption) -> ApiError {
    ApiError::new(400, "invalid_option", e.message).with_extra("field", e.field.into())
}

fn too_long(plies: Option<usize>, max_plies: usize) -> ApiError {
    ApiError::new(422, "game_too_long", game_too_long_message(plies, max_plies))
}

fn invalid_body(message: impl Into<std::borrow::Cow<'static, str>>, field: Option<&str>) -> ApiError {
    let e = ApiError::invalid_request(message);
    match field {
        Some(f) => e.with_extra("field", f.into()),
        None => e,
    }
}

async fn game_gif(gifs: Arc<Gifs>, ctx: Ctx) -> Result<Answer, ApiError> {
    let deps = &gifs.deps;
    if !deps.config.gif_enabled {
        return Err(disabled());
    }
    let query = QueryOptions {
        size: ctx.query_str("size"),
        orientation: ctx.query_str("orientation"),
        delay: ctx.query_str("delay"),
        coords: ctx.query_str("coords"),
    };
    let options = options_from_query(&query).map_err(invalid_option)?;
    let id = ctx.param("id").and_then(parse_game_id).ok_or_else(invalid_game_id)?;
    let game = match deps.store.games().by_id(id).await {
        Ok(Some(game)) => game,
        Ok(None) => return Err(no_such_game()),
        Err(e) if e.kind() == ErrorKind::Busy => return Ok(busy_answer()),
        Err(e) => return Err(ApiError::internal(e)),
    };
    if game.moves.len() > gifs.max_plies {
        return Err(too_long(Some(game.moves.len()), gifs.max_plies));
    }
    let g = game.summary;
    let job = GifJob {
        start_fen: None,
        moves: game.moves,
        white: JobPlayer { name: g.white_name, rating: g.white_rating },
        black: JobPlayer { name: g.black_name, rating: g.black_rating },
        result: GameResult::parse(result_text(g.status)),
        footer: ChessReason::from_u8(g.reason)
            .map(ChessReason::text)
            .filter(|t| !t.is_empty())
            .map(String::from),
        options,
    };
    deliver(&gifs, &ctx, job, format!("scacelith-{}.gif", g.id)).await
}

async fn pgn_gif(gifs: Arc<Gifs>, ctx: Ctx) -> Result<Answer, ApiError> {
    if !gifs.deps.config.gif_enabled {
        return Err(disabled());
    }
    let Value::Object(body) = &ctx.body else {
        return Err(invalid_body("The body must be a JSON object.", None));
    };
    if let Some(k) = js_keys(body).into_iter().find(|k| !GIF_BODY_FIELDS.contains(k)) {
        return Err(invalid_body(format!("unknown field \"{k}\""), Some(k)));
    }
    let pgn = match body.get("pgn") {
        Some(Value::String(pgn)) => pgn.clone(),
        None => return Err(invalid_body("\"pgn\" is required", Some("pgn"))),
        Some(_) => return Err(invalid_body("\"pgn\" must be a string", Some("pgn"))),
    };
    let options = options_from_body(body).map_err(invalid_option)?;
    let max_plies = gifs.max_plies;
    // Reading a PGN of 64 KiB (SAN resolved move by move) is kept off the runtime threads.
    let read = tokio::task::spawn_blocking(move || {
        read_pgn(&pgn, &PgnLimits { max_bytes: GIF_PGN_MAX_BYTES, max_plies, ..PGN_LIMITS })
    })
    .await
    .map_err(ApiError::internal)?;
    let game = match read {
        Ok(game) => game,
        // The reader stops at the first move beyond `max_plies`.
        Err(e) if e.message.starts_with("too many moves") => return Err(too_long(None, max_plies)),
        Err(e) => {
            return Err(ApiError::new(400, "invalid_pgn", e.message)
                .with_extra("line", e.line.into())
                .with_extra("column", e.column.into()));
        }
    };
    let player = |name: &str, elo: &str| JobPlayer {
        name: tag_text(game.last_tag(name).unwrap_or(""), TAG_NAME_MAX),
        rating: tag_rating(game.last_tag(elo)),
    };
    let job = GifJob {
        white: player("White", "WhiteElo"),
        black: player("Black", "BlackElo"),
        result: GameResult::parse(
            normalize_result(game.last_tag("Result").unwrap_or("")).unwrap_or(game.result),
        ),
        footer: tag_ending(game.last_tag("Termination")),
        start_fen: game.start_fen,
        moves: game.moves,
        options,
    };
    deliver(&gifs, &ctx, job, "scacelith-game.gif".into()).await
}

/// The cached GIF, the render in flight of the same GIF, or a new render (render quotas taken
/// first), as the answer.
async fn deliver(gifs: &Gifs, ctx: &Ctx, job: GifJob, filename: String) -> Result<Answer, ApiError> {
    let plies = job.moves.len();
    let take_quotas = || async { ctx.take_rates(&gifs.render_rates) };
    match gifs.deps.gifs.render_with_quotas(job, take_quotas).await {
        Ok(gif) => Ok(Answer::bytes(gif)
            .content_type(GIF_CONTENT_TYPE)
            .header("Content-Disposition", format!("attachment; filename=\"{filename}\""))),
        Err(DeliverError::Quota(e)) => Err(e),
        // The framework adds `Retry-After` from `retryAfter` and gives back every token.
        Err(DeliverError::Gif(GifError::Busy(_))) => {
            Err(ApiError::new(503, "server_busy", SERVER_BUSY_MESSAGE)
                .with_extra("retryAfter", busy_retry_after_secs().into())
                .refund_rate())
        }
        Err(DeliverError::Gif(GifError::RenderFailed(message))) => {
            log_warn!(gifs.deps.log, "GIF render failed", {
                "err": { "message": message }, "plies": plies, "route": ctx.route,
            });
            Err(ApiError::new(500, "render_failed", RENDER_FAILED_MESSAGE))
        }
    }
}

#[cfg(test)]
mod tests;
