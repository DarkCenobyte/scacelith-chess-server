//! Test support of the read routes (info, players, leaderboard, games, account games, reports,
//! GIF): an in-memory store, the API with a manual clock and fake sessions, game records, and a
//! reports service following the Node server's rules over the store.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use scacelith_chess::ChessGame;
use scacelith_protocol::uci_to_move;

use super::reports::{ReportDesk, ReportOutcome, ReportRequest};
use crate::clock::{Clock, ManualClock, SharedClock};
use crate::config::{Config, test_config};
use crate::http::router::BoxFuture;
use crate::http::testing::TestApi;
use crate::http::{Api, ApiError, AuthInfo, Authenticator, Router};
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::matching::elo::{EloSettings, Record, SideChange, apply_game};
use crate::store::tests::support::{new_user, options, store_with};
use crate::store::{
    GameOutcome, GameRecord, GameSummary, NewReport, RatingFn, RatingRecord, SideOutcome, Store, StoreError,
    StoreOptions, status,
};

/// A day in milliseconds.
pub const DAY: i64 = 86_400_000;
/// An hour in milliseconds.
pub const HOUR: i64 = 3_600_000;

/// The sessions of the fake authenticator: token to session.
pub type Sessions = Arc<Mutex<HashMap<String, AuthInfo>>>;

/// Accepts the tokens of [`Sessions`].
pub struct FakeAuth(pub Sessions);

impl Authenticator for FakeAuth {
    async fn validate_token(&self, token: &str) -> Result<Option<AuthInfo>, ApiError> {
        Ok(self.0.lock().get(token).cloned())
    }
}

/// A signed-in player.
#[derive(Debug, Clone)]
pub struct Player {
    pub id: UserId,
    pub name: String,
    pub token: String,
}

/// A test configuration with these keys changed.
pub fn config(overrides: &[(&str, &str)]) -> Config {
    test_config(overrides).expect("a valid test configuration")
}

/// A migrated in-memory store with the ±10 rating function of the Node route tests.
pub async fn memory_store(config: &Config) -> Store {
    store_with(config, options(None)).await
}

/// The server's Elo rules ([`crate::matching::elo`]) as the store's rating function.
fn server_rating(config: &Config) -> RatingFn {
    let settings = EloSettings::from_config(config);
    Arc::new(move |w: &RatingRecord, b: &RatingRecord, score: f64| {
        let to_elo = |r: &RatingRecord| Record {
            rating: r.rating,
            games: r.games,
            wins: r.wins,
            draws: r.draws,
            losses: r.losses,
            peak: r.peak,
            reached_senior: r.reached_senior,
            rated: r.rated,
            counted_games: r.counted_games,
            unrated_games: r.unrated_games,
            unrated_opponents: r.unrated_opponents,
            unrated_half_points: r.unrated_half_points,
        };
        let side = |c: SideChange| SideOutcome {
            before: c.before,
            after: c.after,
            k: Some(c.k),
            record: RatingRecord {
                rating: c.record.rating,
                games: c.record.games,
                wins: c.record.wins,
                draws: c.record.draws,
                losses: c.record.losses,
                peak: c.record.peak,
                reached_senior: c.record.reached_senior,
                rated: c.record.rated,
                counted_games: c.record.counted_games,
                unrated_games: c.record.unrated_games,
                unrated_opponents: c.record.unrated_opponents,
                unrated_half_points: c.record.unrated_half_points,
            },
        };
        let change = apply_game(&to_elo(w), &to_elo(b), score, &settings).expect("a score of 1, 0.5 or 0");
        GameOutcome { white: side(change.white), black: side(change.black) }
    })
}

/// A migrated in-memory store with the server's own rating rules.
pub async fn elo_store(config: &Config) -> Store {
    store_with(config, StoreOptions { rating: Some(server_rating(config)), ..StoreOptions::default() }).await
}

/// The API under test.
pub struct Server {
    pub t: TestApi,
    pub store: Store,
    pub config: Arc<Config>,
    pub clock: Arc<ManualClock>,
    pub sessions: Sessions,
}

impl Server {
    /// The routes `build` registers, over `store`, at wall time `now`.
    pub fn start(
        config: Config,
        store: Store,
        now: i64,
        build: impl FnOnce(&mut Router, &Arc<Config>),
    ) -> Server {
        let config = Arc::new(config);
        let clock = ManualClock::new(1_000_000.0, now);
        let shared: SharedClock = clock.clone() as Arc<dyn Clock>;
        let sessions: Sessions = Arc::default();
        let mut router = Router::new();
        build(&mut router, &config);
        let api = Api::builder(config.clone(), router)
            .clock(shared)
            .authenticator(FakeAuth(sessions.clone()))
            .build();
        Server { t: TestApi::new(api), store, config, clock, sessions }
    }

    /// A session for the account `id`: token `sct_<name, lower case, padded with x to 43>`.
    pub fn login(&self, id: UserId, name: &str) -> String {
        let token = format!("sct_{:x<43}", name.to_lowercase());
        let info = AuthInfo {
            user_id: id,
            username: name.into(),
            session_id: i64::from(id),
            email_verified: true,
            token_hash: Some(format!("h-{name}")),
        };
        self.sessions.lock().insert(token.clone(), info);
        token
    }

    /// A new account created at `created_at`, signed in.
    pub async fn user_at(&self, name: &str, created_at: i64) -> Player {
        let mut u = new_user(name, Some(&format!("{}@example.org", name.to_lowercase())));
        u.created_at = created_at;
        let id = self.store.users().create(u).await.expect("a new account");
        Player { id, name: name.into(), token: self.login(id, name) }
    }

    /// A new account, signed in.
    pub async fn user(&self, name: &str) -> Player {
        self.user_at(name, 1_000).await
    }

    /// Commits finished games.
    pub async fn commit(&self, records: Vec<GameRecord>) {
        self.store.finish_batch(records).await.expect("the games are committed");
    }

    /// Moves the clock of the API (and of its rates) forward.
    pub fn advance(&self, ms: i64) {
        self.clock.advance(ms as f64);
    }

    /// Sets the wall time of the API.
    pub fn set_now(&self, wall_ms: i64) {
        self.clock.set_wall(wall_ms);
    }
}

/// The protocol moves of space-separated UCI moves, checked legal from the start position.
pub fn moves_of(uci: &str) -> Vec<u16> {
    let mut game = ChessGame::new(None).expect("the start position");
    uci.split_whitespace()
        .map(|u| {
            let m = uci_to_move(u).unwrap_or_else(|| panic!("UCI {u}"));
            game.play(m).unwrap_or_else(|e| panic!("{u} is not legal: {e:?}"));
            m
        })
        .collect()
}

/// A finished rated 3+2 game won by White on resignation, with clocks for each move.
pub fn record(id: GameId, white: &Player, black: &Player, moves: Vec<u16>) -> GameRecord {
    let n = moves.len();
    GameRecord {
        id,
        category: "3+2".into(),
        rated: true,
        base_ms: 180_000,
        inc_ms: 2_000,
        white_id: white.id,
        black_id: black.id,
        white_name: white.name.clone(),
        black_name: black.name.clone(),
        white_rating: Some(1500),
        black_rating: Some(1500),
        started_at: Some(1_000_000),
        ended_at: Some(1_600_000),
        status: status::WHITE_WINS,
        reason: 2,
        rematch_of: None,
        flags: 0,
        moves,
        spent_ms: Some(vec![0; n]),
        clock_ms: Some(vec![180_000; n]),
    }
}

/// A player that only exists in game records (`id`, `name`).
pub fn someone(id: UserId, name: &str) -> Player {
    Player { id, name: name.into(), token: String::new() }
}

/// The reports service of the tests: the Node server's eligibility rules over the store, every
/// report weighing 1.
pub struct StoreDesk {
    pub store: Store,
    pub reports_per_day: i64,
}

/// The opponent `user` may report for `game`: the user played it against another account, and
/// it ended within the last 7 days (not in the future).
fn reportable_opponent(game: &GameSummary, user: UserId, now: i64) -> Option<(UserId, String)> {
    let (opponent, name) = if user == game.white_id {
        (game.black_id, &game.black_name)
    } else if user == game.black_id {
        (game.white_id, &game.white_name)
    } else {
        return None;
    };
    let ended = game.ended_at;
    let in_window = ended != 0 && ended >= now - 7 * DAY && ended <= now + 60_000;
    (opponent != 0 && opponent != user && in_window).then(|| (opponent, name.clone()))
}

impl ReportDesk for StoreDesk {
    fn file(
        &self,
        reporter: AuthInfo,
        report: ReportRequest,
        now_ms: i64,
    ) -> BoxFuture<Result<ReportOutcome, ApiError>> {
        let store = self.store.clone();
        let per_day = self.reports_per_day;
        Box::pin(async move {
            let user = reporter.user_id;
            let outcome = store
                .write(move |db| -> Result<ReportOutcome, StoreError> {
                    if db.reports().count_by_reporter_since(user, now_ms - DAY)? >= per_day {
                        return Ok(ReportOutcome::LimitReached);
                    }
                    let game = db.games().by_id(report.game_id)?;
                    let Some((opponent, name)) =
                        game.and_then(|g| reportable_opponent(&g.summary, user, now_ms))
                    else {
                        return Ok(ReportOutcome::NotAllowed);
                    };
                    let want = report.reported.to_lowercase();
                    let renamed = || -> Result<bool, StoreError> {
                        Ok(db.users().by_id(opponent)?.is_some_and(|u| u.username.to_lowercase() == want))
                    };
                    if name.to_lowercase() != want && !renamed()? {
                        return Ok(ReportOutcome::NotAllowed);
                    }
                    if !db.reports().exists(user, opponent, Some(report.game_id))? {
                        db.reports().create(&NewReport {
                            reporter_id: user,
                            reported_id: opponent,
                            game_id: Some(report.game_id),
                            category: report.category,
                            comment: Some(report.comment).filter(|c| !c.is_empty()),
                            weight: 1.0,
                            at: now_ms,
                        })?;
                    }
                    Ok(ReportOutcome::Received)
                })
                .await;
            outcome.map_err(ApiError::internal)
        })
    }

    fn can_report(&self, user: UserId, game: GameSummary, now_ms: i64) -> BoxFuture<bool> {
        let store = self.store.clone();
        let per_day = self.reports_per_day;
        Box::pin(async move {
            let Some((opponent, _)) = reportable_opponent(&game, user, now_ms) else { return false };
            store
                .read(move |db| -> Result<bool, StoreError> {
                    Ok(db.reports().count_by_reporter_since(user, now_ms - DAY)? < per_day
                        && !db.reports().exists(user, opponent, Some(game.id))?)
                })
                .await
                .unwrap_or(false)
        })
    }
}

/// A logger for the routes of one test (its records found by component).
pub fn test_logger(name: &str) -> Logger {
    Logger::root().child(name)
}
