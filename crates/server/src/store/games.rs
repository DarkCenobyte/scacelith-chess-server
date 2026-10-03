//! Finished games: the records the host commits, and the history queries of the account, profile
//! and anti-cheat (the commit itself is in `commit.rs`).

use rusqlite::{Row, params};

use super::db::Db;
use super::error::Result;
use super::values::{NO_CURSOR, unpack_u16, unpack_u32};
use crate::ids::{GameId, UserId};

/// Protocol `GameStatus` values of a finished game.
pub mod status {
    /// White won.
    pub const WHITE_WINS: u8 = 1;
    /// Black won.
    pub const BLACK_WINS: u8 = 2;
    /// Draw.
    pub const DRAW: u8 = 3;
    /// Aborted (never rated, never analysed).
    pub const ABORTED: u8 = 4;
}

/// Bits of [`GameRecord::flags`].
pub mod flags {
    /// The game was created as rated.
    pub const RATED_REQUESTED: i64 = 1;
    /// The game was recovered from the journal after a restart.
    pub const RECOVERED: i64 = 2;
    /// Ended by forfeit.
    pub const FORFEIT: i64 = 4;
    /// The clock was pressed by hand at least once.
    pub const MANUAL_CLOCK: i64 = 8;
}

/// A finished game, as the host commits it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameRecord {
    /// Game id (1 .. 2^53).
    pub id: GameId,
    /// Category id (`3+2`...; `custom` is never rated).
    pub category: String,
    /// Created as rated (ratings are applied only to played-out games outside `custom`).
    pub rated: bool,
    pub base_ms: i64,
    pub inc_ms: i64,
    pub white_id: UserId,
    pub black_id: UserId,
    pub white_name: String,
    pub black_name: String,
    /// Ratings shown at the start.
    pub white_rating: Option<i64>,
    pub black_rating: Option<i64>,
    /// `None`: the end time.
    pub started_at: Option<i64>,
    /// `None`: the commit time.
    pub ended_at: Option<i64>,
    /// [`status`]: 1 to 4.
    pub status: u8,
    /// Protocol `EndReason`.
    pub reason: u8,
    pub rematch_of: Option<GameId>,
    /// [`flags`].
    pub flags: i64,
    /// Encoded moves, one per ply.
    pub moves: Vec<u16>,
    /// Time charged per ply (ms), when recorded.
    pub spent_ms: Option<Vec<u32>>,
    /// Mover's clock after each ply (ms), when recorded.
    pub clock_ms: Option<Vec<u32>>,
}

/// Ratings of one side before and after a game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatingDelta {
    pub before: i64,
    pub after: i64,
}

/// Rating changes of a rated game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatingChanges {
    pub white: RatingDelta,
    pub black: RatingDelta,
}

/// A stored game without its move list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameSummary {
    pub id: GameId,
    pub category: String,
    pub rated: bool,
    pub base_ms: i64,
    pub inc_ms: i64,
    pub white_id: UserId,
    pub black_id: UserId,
    pub white_name: String,
    pub black_name: String,
    pub white_rating: Option<i64>,
    pub black_rating: Option<i64>,
    pub started_at: i64,
    pub ended_at: i64,
    pub status: u8,
    pub reason: u8,
    pub ply_count: i64,
    pub rematch_of: Option<GameId>,
    pub flags: i64,
    /// `None` when the game was not rated.
    pub rating_changes: Option<RatingChanges>,
}

/// A stored game with its move list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub summary: GameSummary,
    pub moves: Vec<u16>,
    /// Empty when not recorded.
    pub spent_ms: Vec<u32>,
    /// Empty when not recorded.
    pub clock_ms: Vec<u32>,
}

/// The result of a game from a player's side, as the history filter asks for it. An aborted game
/// never matches a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultFilter {
    Win,
    Loss,
    Draw,
}

impl ResultFilter {
    /// `win`, `loss` or `draw`.
    pub fn parse(s: &str) -> Option<ResultFilter> {
        match s {
            "win" => Some(ResultFilter::Win),
            "loss" => Some(ResultFilter::Loss),
            "draw" => Some(ResultFilter::Draw),
            _ => None,
        }
    }

    /// The status the result needs as White and as Black.
    fn statuses(self) -> (u8, u8) {
        match self {
            ResultFilter::Win => (status::WHITE_WINS, status::BLACK_WINS),
            ResultFilter::Loss => (status::BLACK_WINS, status::WHITE_WINS),
            ResultFilter::Draw => (status::DRAW, status::DRAW),
        }
    }
}

/// A filter of a player's games (every field optional).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameFilter {
    /// A category id (`custom` included).
    pub category: Option<String>,
    pub rated: Option<bool>,
    pub result: Option<ResultFilter>,
}

impl GameFilter {
    fn is_empty(&self) -> bool {
        self.category.is_none() && self.rated.is_none() && self.result.is_none()
    }
}

/// Columns of a [`GameSummary`], in the order `to_summary` reads them.
pub(crate) const GAME_SUMMARY_COLS: &str = "id, category, rated, base_ms, inc_ms, white_id, black_id, white_name, \
     black_name, white_rating, black_rating, started_at, ended_at, status, reason, ply_count, white_before, white_after, \
     black_before, black_after, rematch_of, flags";

/// A player's games newest first, filtered on the player's own rows of each colour: `?1` the
/// player, `?2` the exclusive id cursor, `?3` the category (NULL: any), `?4` rated 0/1 (NULL:
/// any), `?5` / `?6` the status asked as White / as Black (NULL: any), `?7` the limit. One range
/// scan of `games_white` and of `games_black`, never a scan of the table (tested).
pub const GAMES_FOR_USER_SQL: &str = "SELECT id, category, rated, base_ms, inc_ms, white_id, black_id, white_name, \
     black_name, white_rating, black_rating, started_at, ended_at, status, reason, ply_count, white_before, white_after, \
     black_before, black_after, rematch_of, flags FROM games WHERE id IN (
    SELECT id FROM games WHERE white_id = ?1 AND id < ?2 AND (?3 IS NULL OR category = ?3) AND (?4 IS NULL OR rated = ?4) AND (?5 IS NULL OR status = ?5)
    UNION SELECT id FROM games WHERE black_id = ?1 AND id < ?2 AND (?3 IS NULL OR category = ?3) AND (?4 IS NULL OR rated = ?4) AND (?6 IS NULL OR status = ?6)
    ORDER BY id DESC LIMIT ?7) ORDER BY id DESC";

/// The count of [`GAMES_FOR_USER_SQL`] (`?2` and `?7` unused).
pub const GAMES_COUNT_FOR_USER_SQL: &str = "SELECT (SELECT count(*) FROM games WHERE white_id = ?1 AND (?3 IS NULL OR category = ?3) AND (?4 IS NULL OR rated = ?4) AND (?5 IS NULL OR status = ?5))
    + (SELECT count(*) FROM games WHERE black_id = ?1 AND (?3 IS NULL OR category = ?3) AND (?4 IS NULL OR rated = ?4) AND (?6 IS NULL OR status = ?6)) AS n";

const RECENT_FOR_USER_SQL: &str = "SELECT id, category, rated, base_ms, inc_ms, white_id, black_id, white_name, \
     black_name, white_rating, black_rating, started_at, ended_at, status, reason, ply_count, white_before, white_after, \
     black_before, black_after, rematch_of, flags FROM games WHERE id IN (
    SELECT id FROM games WHERE white_id = ?1 AND id < ?2 UNION SELECT id FROM games WHERE black_id = ?1 AND id < ?2
    ORDER BY id DESC LIMIT ?3) ORDER BY id DESC";

/// Reads a summary from the columns of `GAME_SUMMARY_COLS`.
pub(crate) fn to_summary(r: &Row<'_>) -> rusqlite::Result<GameSummary> {
    let white_before: Option<i64> = r.get(16)?;
    let rating_changes = match white_before {
        None => None,
        Some(before) => Some(RatingChanges {
            white: RatingDelta { before, after: r.get(17)? },
            black: RatingDelta { before: r.get(18)?, after: r.get(19)? },
        }),
    };
    Ok(GameSummary {
        id: r.get::<_, i64>(0)? as GameId,
        category: r.get(1)?,
        rated: r.get(2)?,
        base_ms: r.get(3)?,
        inc_ms: r.get(4)?,
        white_id: r.get(5)?,
        black_id: r.get(6)?,
        white_name: r.get(7)?,
        black_name: r.get(8)?,
        white_rating: r.get(9)?,
        black_rating: r.get(10)?,
        started_at: r.get(11)?,
        ended_at: r.get(12)?,
        status: r.get(13)?,
        reason: r.get(14)?,
        ply_count: r.get(15)?,
        rematch_of: r.get::<_, Option<i64>>(20)?.map(|g| g as GameId),
        flags: r.get(21)?,
        rating_changes,
    })
}

/// A game id as an SQL integer (ids are below 2^53).
pub(crate) fn sql_id(id: GameId) -> i64 {
    i64::try_from(id).unwrap_or(i64::MAX)
}

/// The finished games table.
#[derive(Debug, Clone, Copy)]
pub struct Games<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Games<'_> {
    /// A game with its move list.
    pub fn by_id(&self, id: GameId) -> Result<Option<Game>> {
        self.db.one(
            &format!("SELECT {GAME_SUMMARY_COLS}, moves, spent, clocks FROM games WHERE id = ?1"),
            [sql_id(id)],
            |r| {
                Ok(Game {
                    summary: to_summary(r)?,
                    moves: unpack_u16(r.get_ref(22)?.as_blob_or_null()?),
                    spent_ms: unpack_u32(r.get_ref(23)?.as_blob_or_null()?),
                    clock_ms: unpack_u32(r.get_ref(24)?.as_blob_or_null()?),
                })
            },
        )
    }

    /// The largest stored game id, 0 without games (a shard's new ids come after it).
    pub fn last_id(&self) -> Result<GameId> {
        let id: Option<i64> = self.db.one("SELECT max(id) FROM games", [], |r| r.get(0))?.flatten();
        Ok(id.unwrap_or(0) as GameId)
    }

    /// A player's games, newest first, before the game id `before` (exclusive; `None`: from the
    /// newest).
    pub fn recent_for_user(
        &self,
        user_id: UserId,
        limit: i64,
        before: Option<GameId>,
    ) -> Result<Vec<GameSummary>> {
        self.db.all(
            RECENT_FOR_USER_SQL,
            params![user_id, before.map_or(NO_CURSOR, sql_id), limit],
            to_summary,
        )
    }

    /// Games between two players (both colour orders) ended at `since` or later; only the rated
    /// ones with `rated_only`.
    pub fn count_between(&self, a: UserId, b: UserId, since: i64, rated_only: bool) -> Result<i64> {
        let sql = if rated_only {
            "SELECT count(*) FROM games WHERE ((white_id = ?1 AND black_id = ?2) OR (white_id = ?2 AND black_id = ?1))
             AND ended_at >= ?3 AND rated = 1"
        } else {
            "SELECT count(*) FROM games WHERE ((white_id = ?1 AND black_id = ?2) OR (white_id = ?2 AND black_id = ?1))
             AND ended_at >= ?3"
        };
        self.db.count(sql, params![a, b, since])
    }

    /// A player's games matching `filter`, newest first, before the game id `before` (exclusive).
    /// `limit` is not capped here (the route caps it).
    pub fn list_for_user(
        &self,
        user_id: UserId,
        before: Option<GameId>,
        limit: i64,
        filter: &GameFilter,
    ) -> Result<Vec<GameSummary>> {
        let n = limit.max(0);
        if filter.is_empty() {
            return self.recent_for_user(user_id, n, before);
        }
        let (white, black) = filter.result.map(ResultFilter::statuses).unzip();
        self.db.all(
            GAMES_FOR_USER_SQL,
            params![
                user_id,
                before.map_or(NO_CURSOR, sql_id),
                filter.category,
                filter.rated,
                white,
                black,
                n
            ],
            to_summary,
        )
    }

    /// A player's games (both colours); with a filter, those matching it.
    pub fn count_for_user(&self, user_id: UserId, filter: Option<&GameFilter>) -> Result<i64> {
        match filter.filter(|f| !f.is_empty()) {
            Some(f) => {
                let (white, black) = f.result.map(ResultFilter::statuses).unzip();
                self.db.count(
                    GAMES_COUNT_FOR_USER_SQL,
                    params![user_id, Option::<i64>::None, f.category, f.rated, white, black],
                )
            }
            None => self.db.count(
                "SELECT (SELECT count(*) FROM games WHERE white_id = ?1) + (SELECT count(*) FROM games WHERE black_id = ?1)",
                [user_id],
            ),
        }
    }
}
