//! Game records (DESIGN 5.9, docs/API.md; the Node server's `src/http/routes/players.js`), and
//! the helpers every read route of a game shares (ids, limits, summaries, the PGN export).
//!
//! ```text
//! GET /api/v1/games/:id      one game: players, result, UCI moves with times, PGN tags
//! GET /api/v1/games/:id/pgn  the same game as a PGN file (application/x-chess-pgn)
//! ```
//!
//! Both take an optional session (a token that is sent must be valid) and share the
//! `public_read` limit with the player routes: 60 requests a minute, per account when the request
//! carries a session, else per client address. When the session's player played the game,
//! `GET /games/:id` adds `you` (`white` | `black`) and `reportable` (whether `POST /reports` would
//! take a report of the opponent for this game now, asked of the reports service).
//!
//! The PGN ([`game_pgn`]): the game replayed with the chess rules (SAN), tags in this order:
//! Event (as the JSON's `pgn.Event`), Site (`SERVER_PUBLIC_HOST`), Date (UTC start), Round "-",
//! White, Black, Result ("*" for an aborted game), UTCDate, UTCTime (start, HH:MM:SS), WhiteElo /
//! BlackElo (ratings at the start, "-" when unknown), WhiteRatingDiff / BlackRatingDiff ("+8",
//! "-8", "+0"; in every rated game, never in a casual, custom or aborted one), TimeControl
//! ("180+2"), Termination, PlyCount, ScacelithGameId. Each move carries
//! `{[%clk h:mm:ss.f] [%emt h:mm:ss.f]}`: the mover's clock after the move and the time charged
//! for it, in tenths of a second (truncated), each left out when the record has no value. Stored
//! moves that do not replay to the stored ending answer 500 (logged).

use std::sync::Arc;

use scacelith_chess::{ChessGame, EndReason as ChessReason, GameStatus as ChessStatus, PgnComment, PgnTags};
use scacelith_protocol::{EndReason, GameStatus};
use serde_json::{Map, Value, json};

use super::reports::ReportDesk;
use crate::config::Config;
use crate::http::{Answer, ApiError, AuthMode, Ctx, RateSpec, RouteOpts, Router};
use crate::ids::{GameId, UserId};
use crate::log::{self, Logger, civil_from_days};
use crate::log_error;
use crate::store::{ErrorKind, Game, GameSummary, Store, StoreError, status};

/// The content type of a PGN download.
pub const PGN_CONTENT_TYPE: &str = "application/x-chess-pgn; charset=utf-8";

/// The largest game id or cursor (JavaScript's `Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_ID: u64 = (1 << 53) - 1;

/// The promotion letter of each piece code of a protocol move (`'' '' n b r q '' ''`).
const PROMOTION: [&str; 8] = ["", "", "n", "b", "r", "q", "", ""];

/// The `public_read` limit of the game and player routes: 60 a minute, all four together.
pub fn public_read_rate() -> RateSpec {
    RateSpec::new("public_read", 60.0, 60_000).by_user()
}

/// A game id in a path or a cursor: `^[1-9][0-9]{0,15}$` and a safe integer.
pub fn parse_game_id(raw: &str) -> Option<GameId> {
    let b = raw.as_bytes();
    let shape = (1..=16).contains(&b.len()) && b[0] != b'0' && b.iter().all(u8::is_ascii_digit);
    shape.then(|| raw.parse::<GameId>().ok()).flatten().filter(|&id| id <= MAX_SAFE_ID)
}

/// A page size: 1 to 3 digits and at least 1, capped at `max`.
pub fn parse_limit(raw: &str, max: i64) -> Option<i64> {
    let digits = (1..=3).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_digit());
    let n: i64 = if digits { raw.parse().ok()? } else { return None };
    (n >= 1).then(|| n.min(max))
}

/// 400 `invalid_game_id` "Invalid game id.".
pub fn invalid_game_id() -> ApiError {
    ApiError::new(400, "invalid_game_id", "Invalid game id.")
}

/// 404 `not_found` "No such game.".
pub fn no_such_game() -> ApiError {
    ApiError::not_found("No such game.")
}

/// The answer of a read the store could not serve now: 503 `busy` with `retryAfter` 1 in the
/// body (no `Retry-After` header, as the Node server answered it).
pub fn busy_answer() -> Answer {
    Answer::json(json!({ "error": "busy", "message": "Try again shortly.", "retryAfter": 1 })).status(503)
}

/// The answer of a failed store read of the route `what` ("profile", "games"...): logged as
/// "`what` failed", then 503 [`busy_answer`] when the store was busy, else 500.
pub fn store_failure(log: &Logger, what: &str, e: StoreError) -> Result<Answer, ApiError> {
    log_error!(log, &format!("{what} failed"), { "err": log::error(&e) });
    if e.kind() == ErrorKind::Busy { Ok(busy_answer()) } else { Err(ApiError::internal(e)) }
}

/// The PGN result of a game status ("*" for the others).
pub fn result_text(game_status: u8) -> &'static str {
    match game_status {
        status::WHITE_WINS => "1-0",
        status::BLACK_WINS => "0-1",
        status::DRAW => "1/2-1/2",
        _ => "*",
    }
}

/// JavaScript's `Math.round(ms / 1000)` for whole milliseconds.
fn round_secs(ms: i64) -> i64 {
    (ms + 500).div_euclid(1000)
}

/// The time control as text: "180+2".
pub fn time_control_text(base_ms: i64, inc_ms: i64) -> String {
    format!("{}+{}", round_secs(base_ms), round_secs(inc_ms))
}

/// The UTC date of `t` (ms since the epoch) as PGN writes it: "YYYY.MM.DD".
pub fn pgn_date(t: i64) -> String {
    let (y, m, d) = civil_from_days(t.div_euclid(86_400_000));
    format!("{y}.{m:02}.{d:02}")
}

/// The UTC time of `t` (ms since the epoch): "HH:MM:SS".
fn pgn_time(t: i64) -> String {
    let s = t.rem_euclid(86_400_000) / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// A clock value of a `[%clk]` / `[%emt]` command: h:mm:ss.f, tenths of a second, truncated.
pub fn pgn_clock(ms: i64) -> String {
    let tenths = ms.max(0) / 100;
    let s = tenths / 10;
    format!("{}:{:02}:{:02}.{}", s / 3600, s / 60 % 60, s % 60, tenths % 10)
}

fn square_name(sq: u16) -> [char; 2] {
    // Both values are below 8: the casts cannot truncate.
    [char::from(b'a' + (sq & 7) as u8), char::from(b'1' + (sq >> 3) as u8)]
}

/// UCI text of a protocol move (`from | to << 6 | promotion << 12`), without chess rules.
pub fn uci_of(m: u16) -> String {
    let mut s: String = square_name(m & 63).iter().chain(square_name((m >> 6) & 63).iter()).collect();
    s.push_str(PROMOTION[usize::from((m >> 12) & 7)]);
    s
}

/// One side of a summary: `{name, rating, ratingAfter, ratingDiff}`.
fn side(g: &GameSummary, white: bool) -> Value {
    let (name, rating) =
        if white { (&g.white_name, g.white_rating) } else { (&g.black_name, g.black_rating) };
    let change = g.rating_changes.as_ref().map(|c| if white { c.white } else { c.black });
    json!({
        "name": name,
        "rating": rating,
        "ratingAfter": change.map(|c| c.after),
        "ratingDiff": change.map(|c| c.after - c.before),
    })
}

/// The name of an end reason ("Checkmate"; "Unknown" for a value the protocol does not know).
fn termination(reason: u8) -> &'static str {
    EndReason::from_u8(reason).name()
}

/// The summary of a stored game in the lists (`GET /players/:username/games`,
/// `GET /account/games`): id, category, rated, timeControl, white / black `{name, rating,
/// ratingAfter, ratingDiff}`, color (the side of `viewer`; absent without one), status, reason,
/// result, termination, plies, startedAt, endedAt.
pub fn game_summary(g: &GameSummary, viewer: Option<UserId>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), g.id.into());
    m.insert("category".into(), g.category.clone().into());
    m.insert("rated".into(), g.rated.into());
    m.insert("timeControl".into(), time_control_text(g.base_ms, g.inc_ms).into());
    m.insert("white".into(), side(g, true));
    m.insert("black".into(), side(g, false));
    if let Some(user) = viewer {
        m.insert("color".into(), if g.white_id == user { "white" } else { "black" }.into());
    }
    m.insert("status".into(), g.status.into());
    m.insert("reason".into(), g.reason.into());
    m.insert("result".into(), result_text(g.status).into());
    m.insert("termination".into(), termination(g.reason).into());
    m.insert("plies".into(), g.ply_count.into());
    m.insert("startedAt".into(), g.started_at.into());
    m.insert("endedAt".into(), g.ended_at.into());
    m
}

/// The Event of a game: "<SERVER_NAME> rated 3+2", "<SERVER_NAME> casual custom".
fn event_name(g: &GameSummary, config: &Config) -> String {
    format!("{} {} {}", config.server_name, if g.rated { "rated" } else { "casual" }, g.category)
}

/// "+8", "-8", "+0".
fn rating_diff_text(before: i64, after: i64) -> String {
    let d = after - before;
    if d < 0 { d.to_string() } else { format!("+{d}") }
}

/// The record of `GET /games/:id` (without `you` / `reportable`).
fn game_record(game: &Game, config: &Config) -> Map<String, Value> {
    let g = &game.summary;
    let moves: Vec<Value> = game
        .moves
        .iter()
        .enumerate()
        .map(|(i, &m)| {
            json!({
                "uci": uci_of(m),
                "spentMs": game.spent_ms.get(i),
                "clockMs": game.clock_ms.get(i),
            })
        })
        .collect();
    let elo = |r: Option<i64>| r.map_or_else(|| Value::from("-"), Value::from);
    let mut body = game_summary(g, None);
    body.insert("baseMs".into(), g.base_ms.into());
    body.insert("incMs".into(), g.inc_ms.into());
    body.insert(
        "statusName".into(),
        GameStatus::from_u8(g.status).map_or("Unknown", GameStatus::name).into(),
    );
    body.insert("rematchOf".into(), g.rematch_of.into());
    body.insert("moves".into(), Value::Array(moves));
    body.insert(
        "pgn".into(),
        json!({
            "Event": event_name(g, config),
            "Site": config.server_public_host,
            "Date": pgn_date(g.started_at),
            "Round": "-",
            "White": g.white_name,
            "Black": g.black_name,
            "Result": result_text(g.status),
            "WhiteElo": elo(g.white_rating),
            "BlackElo": elo(g.black_rating),
            "TimeControl": time_control_text(g.base_ms, g.inc_ms),
            "Termination": termination(g.reason),
            "PlyCount": g.ply_count,
        }),
    );
    body
}

/// The PGN text of a stored game (module documentation): one game, '\n' line endings. `None`
/// when the stored moves are not legal from the start position, or lead to another ending than
/// the stored one, or the stored ending is not one the chess rules know.
pub fn game_pgn(game: &Game, config: &Config) -> Option<String> {
    let g = &game.summary;
    let mut cg = ChessGame::from_moves(None, &game.moves)?;
    // Resignation, flag fall, abandonment, abort... are not in the moves: the stored ending is
    // applied unless the moves ended the game by themselves, then they must agree.
    let (stored_status, stored_reason) = (ChessStatus::from_u8(g.status)?, ChessReason::from_u8(g.reason)?);
    if !cg.is_over() {
        cg.end(stored_status, stored_reason).ok()?;
    } else if cg.status() != stored_status || cg.reason() != stored_reason {
        return None;
    }
    let elo = |r: Option<i64>| r.map_or_else(|| "-".to_string(), |r| r.to_string());
    let mut after_result = vec![
        ("UTCDate".to_string(), pgn_date(g.started_at)),
        ("UTCTime".to_string(), pgn_time(g.started_at)),
        ("WhiteElo".to_string(), elo(g.white_rating)),
        ("BlackElo".to_string(), elo(g.black_rating)),
    ];
    if let Some(c) = &g.rating_changes {
        after_result.push(("WhiteRatingDiff".into(), rating_diff_text(c.white.before, c.white.after)));
        after_result.push(("BlackRatingDiff".into(), rating_diff_text(c.black.before, c.black.after)));
    }
    let comments = (0..cg.ply())
        .map(|i| {
            let mut words = Vec::with_capacity(2);
            if let Some(&ms) = game.clock_ms.get(i) {
                words.push(format!("[%clk {}]", pgn_clock(i64::from(ms))));
            }
            if let Some(&ms) = game.spent_ms.get(i) {
                words.push(format!("[%emt {}]", pgn_clock(i64::from(ms))));
            }
            (!words.is_empty()).then_some(PgnComment::Words(words))
        })
        .collect();
    let tags = PgnTags {
        event: Some(event_name(g, config)),
        site: Some(config.server_public_host.clone()),
        date: Some(pgn_date(g.started_at)),
        round: Some("-".into()),
        white: Some(g.white_name.clone()),
        black: Some(g.black_name.clone()),
        time_control: Some(time_control_text(g.base_ms, g.inc_ms)),
        after_result,
        extra: vec![("PlyCount".into(), cg.ply().to_string()), ("ScacelithGameId".into(), g.id.to_string())],
        comments,
    };
    Some(cg.pgn(&tags))
}

/// The services of the game routes.
#[derive(Clone)]
pub struct GamesDeps {
    /// The configuration (server name and public host of the PGN tags).
    pub config: Arc<Config>,
    /// The store.
    pub store: Store,
    /// The reports service, asked for `reportable` when a player reads their own game.
    pub reports: Arc<dyn ReportDesk>,
    /// The logger of the API.
    pub log: Logger,
}

/// Registers `GET /games/:id` and `GET /games/:id/pgn`.
pub fn register(router: &mut Router, deps: GamesDeps) {
    let deps = Arc::new(deps);
    let opts = || RouteOpts::new().auth(AuthMode::Optional).rate(public_read_rate());
    let d = deps.clone();
    router.get("/games/:id", opts(), move |ctx| game(d.clone(), ctx));
    router.get("/games/:id/pgn", opts(), move |ctx| pgn_file(deps.clone(), ctx));
}

/// Reads the game of the `:id` parameter: 400 for a malformed id, 404 for an unknown one.
async fn find_game(deps: &GamesDeps, ctx: &Ctx, what: &str) -> Result<Result<Game, Answer>, ApiError> {
    let id = ctx.param("id").and_then(parse_game_id).ok_or_else(invalid_game_id)?;
    match deps.store.games().by_id(id).await {
        Ok(Some(game)) => Ok(Ok(game)),
        Ok(None) => Err(no_such_game()),
        Err(e) => store_failure(&deps.log, what, e).map(Err),
    }
}

async fn game(deps: Arc<GamesDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let game = match find_game(&deps, &ctx, "game").await? {
        Ok(game) => game,
        Err(answer) => return Ok(answer),
    };
    let mut body = game_record(&game, &deps.config);
    let g = game.summary;
    if let Some(me) = ctx.user_id().filter(|&me| g.white_id == me || g.black_id == me) {
        body.insert("you".into(), if g.white_id == me { "white" } else { "black" }.into());
        let reportable = deps.reports.can_report(me, g, ctx.now_ms).await;
        body.insert("reportable".into(), reportable.into());
    }
    Ok(Answer::json(Value::Object(body)))
}

async fn pgn_file(deps: Arc<GamesDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let game = match find_game(&deps, &ctx, "pgn").await? {
        Ok(game) => game,
        Err(answer) => return Ok(answer),
    };
    // Replaying the moves and writing the SAN takes up to a few milliseconds: off the runtime.
    let config = deps.config.clone();
    let (game, text) = tokio::task::spawn_blocking(move || {
        let text = game_pgn(&game, &config);
        (game, text)
    })
    .await
    .map_err(ApiError::internal)?;
    let g = &game.summary;
    let Some(text) = text else {
        log_error!(deps.log, "stored game cannot be replayed", {
            "gameId": g.id, "plies": game.moves.len(), "status": g.status, "reason": g.reason,
        });
        return Err(ApiError::new(
            500,
            "internal_error",
            "The moves stored for this game cannot be replayed.",
        ));
    };
    Ok(Answer::text(text)
        .content_type(PGN_CONTENT_TYPE)
        .header("Content-Disposition", format!("attachment; filename=\"scacelith-{}.pgn\"", g.id)))
}

#[cfg(test)]
mod tests;
