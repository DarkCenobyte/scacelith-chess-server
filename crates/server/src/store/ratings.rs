//! Rating records and the contract of the rating function (the Elo of `matching`).

use std::sync::Arc;

use rusqlite::{Row, params};

use super::db::Db;
use super::error::Result;
use crate::ids::UserId;

/// A player's rating record in one category (the fields of the Elo module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatingRecord {
    pub rating: i64,
    /// Games played (counted or not).
    pub games: i64,
    pub wins: i64,
    pub draws: i64,
    pub losses: i64,
    pub peak: i64,
    pub reached_senior: bool,
    /// `false` during the unrated phase.
    pub rated: bool,
    /// Games that entered the rating (K, provisional mark, leaderboard).
    pub counted_games: i64,
    pub unrated_games: i64,
    /// Sum of the opponents' ratings over the unrated phase.
    pub unrated_opponents: i64,
    /// Score of the unrated phase in half points.
    pub unrated_half_points: i64,
}

impl RatingRecord {
    /// The record of a player who has not played in the category.
    pub fn initial(rating: i64) -> RatingRecord {
        RatingRecord {
            rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
            peak: rating,
            reached_senior: false,
            rated: false,
            counted_games: 0,
            unrated_games: 0,
            unrated_opponents: 0,
            unrated_half_points: 0,
        }
    }

    /// Whether the rating is provisional: unrated, or fewer than `provisional_games` counted games.
    pub fn is_provisional(&self, provisional_games: i64) -> bool {
        !self.rated || self.counted_games < provisional_games
    }
}

/// One side of a rated game, as the rating function computed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideOutcome {
    /// Rating before the game.
    pub before: i64,
    /// Rating after the game.
    pub after: i64,
    /// Development coefficient of the change (0: no K-formula change; `None`: not given).
    pub k: Option<i64>,
    /// The record after the game, as stored.
    pub record: RatingRecord,
}

/// Both sides of a rated game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameOutcome {
    pub white: SideOutcome,
    pub black: SideOutcome,
}

/// The rating function: `(white record, black record, white's score 1 / 0.5 / 0)` to the
/// outcome of both sides, computed from the records before the game.
pub type RatingFn = Arc<dyn Fn(&RatingRecord, &RatingRecord, f64) -> GameOutcome + Send + Sync>;

/// A record of [`Ratings::for_user`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryRating {
    pub category: String,
    pub record: RatingRecord,
    pub provisional: bool,
    pub updated_at: i64,
}

/// A leaderboard row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderboardRow {
    pub user_id: UserId,
    pub username: String,
    pub rating: i64,
    pub games: i64,
    pub wins: i64,
    pub draws: i64,
    pub losses: i64,
    pub peak: i64,
}

const RATING_COLS: &str = "rating, games, wins, draws, losses, peak, reached_senior, rated, counted_games, \
     unrated_games, unrated_opponents, unrated_half_points";

/// Reads a record from the 12 columns of `RATING_COLS` starting at `at`.
fn to_record(r: &Row<'_>, at: usize) -> rusqlite::Result<RatingRecord> {
    Ok(RatingRecord {
        rating: r.get(at)?,
        games: r.get(at + 1)?,
        wins: r.get(at + 2)?,
        draws: r.get(at + 3)?,
        losses: r.get(at + 4)?,
        peak: r.get(at + 5)?,
        reached_senior: r.get(at + 6)?,
        rated: r.get(at + 7)?,
        counted_games: r.get(at + 8)?,
        unrated_games: r.get(at + 9)?,
        unrated_opponents: r.get(at + 10)?,
        unrated_half_points: r.get(at + 11)?,
    })
}

/// The ratings table.
#[derive(Debug, Clone, Copy)]
pub struct Ratings<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Ratings<'_> {
    /// The record of a player in a category (the initial record when there is none).
    pub fn get(&self, user_id: UserId, category: &str) -> Result<RatingRecord> {
        let rec = self.db.one(
            &format!("SELECT {RATING_COLS} FROM ratings WHERE user_id = ?1 AND category = ?2"),
            params![user_id, category],
            |r| to_record(r, 0),
        )?;
        Ok(rec.unwrap_or_else(|| RatingRecord::initial(self.db.ctx().initial_rating)))
    }

    /// Every record of a player, by category.
    pub fn for_user(&self, user_id: UserId) -> Result<Vec<CategoryRating>> {
        let provisional_games = self.db.ctx().provisional_games;
        self.db.all(
            &format!(
                "SELECT category, {RATING_COLS}, updated_at FROM ratings WHERE user_id = ?1 ORDER BY category"
            ),
            [user_id],
            |r| {
                let record = to_record(r, 1)?;
                Ok(CategoryRating {
                    category: r.get(0)?,
                    record,
                    provisional: record.is_provisional(provisional_games),
                    updated_at: r.get(13)?,
                })
            },
        )
    }

    /// The best rated records of a category with at least `min_games` counted games (`None`:
    /// `PROVISIONAL_GAMES`), leaving out deleted accounts and confirmed cheaters.
    pub fn leaderboard(
        &self,
        category: &str,
        limit: i64,
        min_games: Option<i64>,
    ) -> Result<Vec<LeaderboardRow>> {
        let min_games = min_games.unwrap_or(self.db.ctx().provisional_games);
        self.db.all(
            "SELECT r.user_id, u.username, r.rating, r.games, r.wins, r.draws, r.losses, r.peak
             FROM ratings r JOIN users u ON u.id = r.user_id LEFT JOIN player_integrity pi ON pi.user_id = r.user_id
             WHERE r.category = ?1 AND r.rated = 1 AND r.counted_games >= ?2 AND u.status = 'active'
             AND (pi.level IS NULL OR pi.level <> 'confirmed')
             ORDER BY r.rating DESC, r.games DESC, r.user_id LIMIT ?3",
            params![category, min_games, limit],
            |r| {
                Ok(LeaderboardRow {
                    user_id: r.get(0)?,
                    username: r.get(1)?,
                    rating: r.get(2)?,
                    games: r.get(3)?,
                    wins: r.get(4)?,
                    draws: r.get(5)?,
                    losses: r.get(6)?,
                    peak: r.get(7)?,
                })
            },
        )
    }

    /// Stores a record (insert or replace).
    pub fn put(&self, user_id: UserId, category: &str, rec: &RatingRecord, now: i64) -> Result<()> {
        self.db.exec(
            &format!(
                "INSERT INTO ratings (user_id, category, {RATING_COLS}, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT (user_id, category) DO UPDATE SET rating = excluded.rating, games = excluded.games,
                 wins = excluded.wins, draws = excluded.draws, losses = excluded.losses, peak = excluded.peak,
                 reached_senior = excluded.reached_senior, rated = excluded.rated,
                 counted_games = excluded.counted_games, unrated_games = excluded.unrated_games,
                 unrated_opponents = excluded.unrated_opponents, unrated_half_points = excluded.unrated_half_points,
                 updated_at = excluded.updated_at"
            ),
            params![
                user_id,
                category,
                rec.rating,
                rec.games,
                rec.wins,
                rec.draws,
                rec.losses,
                rec.peak,
                rec.reached_senior,
                rec.rated,
                rec.counted_games,
                rec.unrated_games,
                rec.unrated_opponents,
                rec.unrated_half_points,
                now,
            ],
        )?;
        Ok(())
    }
}
