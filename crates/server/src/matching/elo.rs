//! Elo ratings per time-control category, as FIDE computes them (FIDE Rating Regulations effective
//! from 1 March 2024): a pure mirror of the game's offline rating (`src/game/elo.h` / `elo.cpp`), so
//! a player's online and offline numbers mean the same thing. Both are checked against the same
//! vectors (`test/fixtures/elo-vectors.json`, written and checked by the `elo_vectors` test of this
//! module, read by the game's `tests/elo_tests.cpp`). DESIGN 6.6.
//!
//! * expected score: PD from FIDE's table 8.1.2 ([`FIDE_PD_TABLE`]) for the rating difference D,
//!   D counting at most 400 points (8.3.1); the higher-rated player gets PD, the lower 1 - PD.
//! * change: K x (score - PD), rounded to the nearest point (halves away from zero), computed in
//!   whole hundredths so that the languages agree to the last point.
//! * K: 40 until the player has `provisional_games` (30) counted games in the category, the
//!   counted games of the unrated phase included; 20 afterwards; 10 once the player has reached
//!   2400 (for good: the peak counts).
//! * unrated phase (8.2): a new record is unrated, with a working rating equal to the initial
//!   rating (used for pairing and as the opponent value of the other player). Each counted game
//!   adds the opponent's rating and the score; after [`UNRATED_GAMES`] games
//!   `Ru = Ra + dp(p)`, with `Ra = (sum of the opponents' ratings + 2 x 1800) / (n + 2)` and
//!   `p = (score + 1) / (n + 2)` rounded to hundredths (two hypothetical draws against 1800),
//!   `dp` from FIDE's table 8.1.1 ([`FIDE_DP_TABLE`]), rounded and capped at 2200. The peak
//!   becomes Ru.
//! * zero score (8.2.1, one game at a time): a game lost by an unrated player who has not scored
//!   yet in the category (no win, no draw) counts for neither player's rating (it counts in the
//!   games and the wins / draws / losses only).
//! * unrated opponent: a rated player's game against an unrated opponent does not change the
//!   rated player's rating (8.3) and is not a counted game.
//! * floor: a rating never drops below [`RATING_FLOOR`].
//!
//! Departures from FIDE needed by a game server: games are rated one by one against the ratings
//! before each game; a game between two unrated players counts for both at the other's working
//! rating (unless it is a zero score); FIDE's K = 40 for players under 18 does not apply.
//!
//! Integer arithmetic only (except the reported expected score).

use std::collections::HashMap;
use std::fmt;

use crate::config::Category;

/// Rating from which K drops to 10 for good.
pub const SENIOR_RATING: i64 = 2400;
/// Largest rating difference taken into account (FIDE 8.3.1).
pub const MAX_RATING_GAP: i64 = 400;
/// Ratings never drop below this.
pub const RATING_FLOOR: i64 = 100;
/// Counted games of the unrated phase before the first rating (FIDE 8.2).
pub const UNRATED_GAMES: i64 = 5;
/// Rating of the two hypothetical opponents drawn with in the first rating (FIDE 8.2).
pub const HYPOTHETICAL_OPPONENT: i64 = 1800;
/// Highest first rating (FIDE 8.2).
pub const MAX_INITIAL_RATING: i64 = 2200;
/// Initial rating of the game (`elo.h`).
pub const DEFAULT_INITIAL_RATING: i64 = 1500;
/// Provisional games of the game (`elo.h`).
pub const DEFAULT_PROVISIONAL_GAMES: i64 = 30;
/// Id of every non-official time control.
pub const CUSTOM_CATEGORY: &str = "custom";

/// FIDE table 8.1.2 as published: (highest D of the row, PD of the higher-rated player in
/// hundredths). D above 735 gives 1.00. The table is the normal distribution with a standard
/// deviation of 2000 / 7 rounded to hundredths, except at six differences where FIDE's rows keep
/// the neighbouring value (54, 343, 344, 358, 392 and 620): FIDE applies the table, so the table
/// is what counts. The 400-point rule caps D before the lookup.
pub const FIDE_PD_TABLE: [(i64, i64); 50] = [
    (3, 50),
    (10, 51),
    (17, 52),
    (25, 53),
    (32, 54),
    (39, 55),
    (46, 56),
    (53, 57),
    (61, 58),
    (68, 59),
    (76, 60),
    (83, 61),
    (91, 62),
    (98, 63),
    (106, 64),
    (113, 65),
    (121, 66),
    (129, 67),
    (137, 68),
    (145, 69),
    (153, 70),
    (162, 71),
    (170, 72),
    (179, 73),
    (188, 74),
    (197, 75),
    (206, 76),
    (215, 77),
    (225, 78),
    (235, 79),
    (245, 80),
    (256, 81),
    (267, 82),
    (278, 83),
    (290, 84),
    (302, 85),
    (315, 86),
    (328, 87),
    (344, 88),
    (357, 89),
    (374, 90),
    (391, 91),
    (411, 92),
    (432, 93),
    (456, 94),
    (484, 95),
    (517, 96),
    (559, 97),
    (619, 98),
    (735, 99),
];

/// FIDE table 8.1.1: dp for p = 1.00, 0.99 ... 0.50 (index 0 is p = 1.00, index 50 is p = 0.50).
/// Below 0.50, dp(p) = -dp(1 - p).
pub const FIDE_DP_TABLE: [i64; 51] = [
    800, 677, 589, 538, 501, 470, 444, 422, 401, 383, 366, 351, 336, 322, 309, 296, 284, 273, 262, 251, 240,
    230, 220, 211, 202, 193, 184, 175, 166, 158, 149, 141, 133, 125, 117, 110, 102, 95, 87, 80, 72, 65, 57,
    50, 43, 36, 29, 21, 14, 7, 0,
];

/// The two settings of the rating rules (INITIAL_RATING, PROVISIONAL_GAMES).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EloSettings {
    /// Working rating of an unrated record.
    pub initial_rating: i64,
    /// Counted games before K drops from 40 to 20 and the provisional mark goes.
    pub provisional_games: i64,
}

impl EloSettings {
    /// The game's constants (`elo.h`), used by the shared vectors whatever the configuration.
    pub const GAME: EloSettings =
        EloSettings { initial_rating: DEFAULT_INITIAL_RATING, provisional_games: DEFAULT_PROVISIONAL_GAMES };

    /// The settings of a loaded configuration.
    pub fn from_config(config: &crate::config::Config) -> EloSettings {
        EloSettings { initial_rating: config.initial_rating, provisional_games: config.provisional_games }
    }
}

impl Default for EloSettings {
    fn default() -> Self {
        EloSettings::GAME
    }
}

/// A player's rating record in one category.
///
/// `rated` is false during the unrated phase, which accumulates `unrated_games`, the sum of the
/// opponents' ratings `unrated_opponents` and the score in half points `unrated_half_points`.
/// `counted_games` equals `unrated_games` until the first rating. `reached_senior` is the
/// persisted form of the C++ `peak >= 2400` test (both are honoured).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub rating: i64,
    pub games: i64,
    pub wins: i64,
    pub draws: i64,
    pub losses: i64,
    pub peak: i64,
    pub reached_senior: bool,
    pub rated: bool,
    pub counted_games: i64,
    pub unrated_games: i64,
    pub unrated_opponents: i64,
    pub unrated_half_points: i64,
}

impl Record {
    /// A fresh record: unrated, the working rating `initial_rating`, no game.
    pub fn new(settings: &EloSettings) -> Record {
        let r = settings.initial_rating;
        Record {
            rating: r,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
            peak: r,
            reached_senior: false,
            rated: false,
            counted_games: 0,
            unrated_games: 0,
            unrated_opponents: 0,
            unrated_half_points: 0,
        }
    }

    /// The record with its invariants restored: the peak is at least the rating, counters are
    /// not negative, a rated record carries no unrated sums and at most `games` counted games, an
    /// unrated record counts the games of its unrated phase, and `reached_senior` holds once a
    /// rated peak reached 2400.
    pub fn normalized(&self) -> Record {
        // Every field is present: the settings' defaults are never used.
        PartialRecord::from(*self).normalize(&EloSettings::GAME)
    }

    /// Whether the record has no rating yet (its unrated phase).
    pub fn is_unrated(&self) -> bool {
        !self.rated
    }

    /// Whether the rating is shown as provisional ("1500?"): unrated, or fewer than
    /// `provisional_games` counted games in the category (K = 40; not on the leaderboard).
    pub fn is_provisional(&self, settings: &EloSettings) -> bool {
        !self.rated || self.counted_games < settings.provisional_games
    }

    /// Development coefficient of a rated player's next game: 10 once 2400 has been reached
    /// (checked first, as in `elo.cpp`), else 40 before `provisional_games` counted games, else 20.
    pub fn k_factor(&self, settings: &EloSettings) -> i64 {
        if self.reached_senior || self.peak >= SENIOR_RATING || self.rating >= SENIOR_RATING {
            return 10;
        }
        if self.counted_games < settings.provisional_games { 40 } else { 20 }
    }
}

/// A possibly partial record as it may be stored (a record written before a field existed):
/// missing fields take the defaults ([`PartialRecord::normalize`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PartialRecord {
    pub rating: Option<i64>,
    pub games: Option<i64>,
    pub wins: Option<i64>,
    pub draws: Option<i64>,
    pub losses: Option<i64>,
    pub peak: Option<i64>,
    pub reached_senior: Option<bool>,
    /// Absent on records stored before the unrated phase existed: rated when they have games.
    pub rated: Option<bool>,
    /// Absent on records stored before it existed: a rated record counts all its games.
    pub counted_games: Option<i64>,
    pub unrated_games: Option<i64>,
    pub unrated_opponents: Option<i64>,
    pub unrated_half_points: Option<i64>,
}

impl From<Record> for PartialRecord {
    fn from(r: Record) -> Self {
        PartialRecord {
            rating: Some(r.rating),
            games: Some(r.games),
            wins: Some(r.wins),
            draws: Some(r.draws),
            losses: Some(r.losses),
            peak: Some(r.peak),
            reached_senior: Some(r.reached_senior),
            rated: Some(r.rated),
            counted_games: Some(r.counted_games),
            unrated_games: Some(r.unrated_games),
            unrated_opponents: Some(r.unrated_opponents),
            unrated_half_points: Some(r.unrated_half_points),
        }
    }
}

impl PartialRecord {
    /// A complete record (missing fields take the defaults; the peak is at least the rating;
    /// without `rated`, a record with games is rated; the counted games are at most the games,
    /// and those of the unrated phase while unrated).
    pub fn normalize(&self, settings: &EloSettings) -> Record {
        let rating = self.rating.unwrap_or(settings.initial_rating);
        let peak = self.peak.unwrap_or(rating).max(rating);
        let games = self.games.unwrap_or(0).max(0);
        let rated = self.rated.unwrap_or(games > 0);
        let unrated_games = if rated { 0 } else { self.unrated_games.unwrap_or(0).max(0) };
        Record {
            rating,
            games,
            wins: self.wins.unwrap_or(0).max(0),
            draws: self.draws.unwrap_or(0).max(0),
            losses: self.losses.unwrap_or(0).max(0),
            peak,
            reached_senior: self.reached_senior.unwrap_or(false) || (rated && peak >= SENIOR_RATING),
            rated,
            counted_games: if rated {
                games.min(self.counted_games.unwrap_or(games).max(0))
            } else {
                unrated_games
            },
            unrated_games,
            unrated_opponents: if rated { 0 } else { self.unrated_opponents.unwrap_or(0).max(0) },
            unrated_half_points: if rated { 0 } else { self.unrated_half_points.unwrap_or(0).max(0) },
        }
    }

    /// Whether the record has no rating yet: `rated` when present, else no game.
    pub fn is_unrated(&self) -> bool {
        match self.rated {
            Some(rated) => !rated,
            None => self.games.unwrap_or(0) <= 0,
        }
    }

    /// Counted games: the field, else those of the unrated phase while unrated, else all games.
    pub fn counted_games(&self) -> i64 {
        match self.counted_games {
            Some(n) => n,
            None if self.is_unrated() => self.unrated_games.unwrap_or(0),
            None => self.games.unwrap_or(0),
        }
    }

    /// See [`Record::is_provisional`].
    pub fn is_provisional(&self, settings: &EloSettings) -> bool {
        self.is_unrated() || self.counted_games() < settings.provisional_games
    }

    /// See [`Record::k_factor`].
    pub fn k_factor(&self, settings: &EloSettings) -> i64 {
        let rating = self.rating.unwrap_or(settings.initial_rating);
        if self.reached_senior == Some(true)
            || self.peak.unwrap_or(0) >= SENIOR_RATING
            || rating >= SENIOR_RATING
        {
            return 10;
        }
        if self.counted_games() < settings.provisional_games { 40 } else { 20 }
    }
}

/// Integer division rounded to the nearest integer, halves away from zero (`std::lround(a / b)`).
/// `b` must be positive.
fn div_round(a: i64, b: i64) -> i64 {
    if a < 0 { -((-2 * a + b).div_euclid(2 * b)) } else { (2 * a + b).div_euclid(2 * b) }
}

/// PD of FIDE table 8.1.2, in hundredths, for a rating difference `d` (its absolute value; no
/// cap). Returns 50..=100.
pub fn scoring_probability(d: i64) -> i64 {
    let x = d.abs();
    FIDE_PD_TABLE.iter().find(|(hi, _)| x <= *hi).map_or(100, |(_, pd)| *pd)
}

/// dp of FIDE table 8.1.1 for a percentage score `p` (in hundredths, clamped to 0..=100).
/// Returns -800..=800.
pub fn rating_difference(p: i64) -> i64 {
    let q = p.clamp(0, 100);
    if q >= 50 { FIDE_DP_TABLE[(100 - q) as usize] } else { -FIDE_DP_TABLE[q as usize] }
}

/// Expected score in hundredths: the table's PD after the 400-point rule.
fn expected100(rating: i64, opponent: i64) -> i64 {
    let d = MAX_RATING_GAP.min((rating - opponent).abs());
    let pd = scoring_probability(d);
    if rating >= opponent { pd } else { 100 - pd }
}

/// Expected score (0..1) of a player rated `rating` against `opponent` (FIDE table 8.1.2).
pub fn expected_score(rating: i64, opponent: i64) -> f64 {
    expected100(rating, opponent) as f64 / 100.0
}

/// A score (clamped to 0..1) in half points: 2 win, 1 draw, 0 loss.
fn half_points(score: f64) -> i64 {
    (score.clamp(0.0, 1.0) * 2.0).round() as i64
}

/// First rating after the unrated phase (FIDE 8.2): Ra + dp(p) with the two hypothetical draws
/// against 1800, rounded, capped at [`MAX_INITIAL_RATING`] and floored at [`RATING_FLOOR`].
pub fn initial_rating(unrated_games: i64, unrated_opponents: i64, unrated_half_points: i64) -> i64 {
    let n = unrated_games + 2;
    // p = (score + 1) / (n + 2) = (half points + 2) / (2 (n + 2)), in hundredths rounded half up.
    let p = div_round(100 * (unrated_half_points + 2), 2 * n);
    let ru = div_round(unrated_opponents + 2 * HYPOTHETICAL_OPPONENT + rating_difference(p) * n, n);
    ru.clamp(RATING_FLOOR, MAX_INITIAL_RATING)
}

/// Rating change of the K formula for a rated record against a rated opponent, before the floor:
/// K x (score - PD), rounded.
pub fn rating_delta(record: &Record, opponent: i64, score: f64, settings: &EloSettings) -> i64 {
    div_round(
        record.k_factor(settings) * (50 * half_points(score) - expected100(record.rating, opponent)),
        100,
    )
}

/// One side of a rated game.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SideChange {
    /// Rating before the game (the protocol's `RatingChange.before`).
    pub before: i64,
    /// Rating after the game.
    pub after: i64,
    /// `after - before`.
    pub delta: i64,
    /// K applied; 0 when the K formula did not apply (unrated phase, unrated opponent).
    pub k: i64,
    /// Expected score of the K formula (reported only).
    pub expected: f64,
    /// Games played in the category after this one.
    pub games: i64,
    /// Whether the new rating is provisional.
    pub provisional: bool,
    /// The updated record to store.
    pub record: Record,
}

/// Both sides of a rated game.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GameChange {
    pub white: SideChange,
    pub black: SideChange,
}

/// Error of [`apply_game`]: the score is not a number in 0..=1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InvalidScore(pub f64);

impl fmt::Display for InvalidScore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "elo: score must be 0..1, got {}", self.0)
    }
}

impl std::error::Error for InvalidScore {}

// Whether a record scoring `half` half points makes it a zero score (FIDE 8.2.1): an unrated
// record that has not scored yet in the category loses.
fn zero_score(rec: &Record, half: i64) -> bool {
    !rec.rated && half == 0 && rec.wins + rec.draws == 0
}

// One side of a game; `opp` is the opponent's record before the game.
fn apply_side(rec: &Record, opp: &Record, score: f64, settings: &EloSettings) -> SideChange {
    let half = half_points(score);
    let mut record = Record { games: rec.games + 1, ..*rec };
    match half {
        2 => record.wins += 1,
        0 => record.losses += 1,
        _ => record.draws += 1,
    }
    let mut k = 0;
    if !rec.rated {
        // The zero-score rule: a zero score is disregarded, and so are the opponent's results
        // against it.
        if !zero_score(rec, half) && !zero_score(opp, 2 - half) {
            record.counted_games += 1;
            record.unrated_games += 1;
            record.unrated_opponents += opp.rating;
            record.unrated_half_points += half;
            if record.unrated_games >= UNRATED_GAMES {
                record.rating = initial_rating(
                    record.unrated_games,
                    record.unrated_opponents,
                    record.unrated_half_points,
                );
                record.peak = record.rating;
                record.rated = true;
                record.unrated_games = 0;
                record.unrated_opponents = 0;
                record.unrated_half_points = 0;
            }
        }
    } else if opp.rated {
        k = rec.k_factor(settings);
        record.counted_games += 1;
        record.rating = RATING_FLOOR.max(rec.rating + rating_delta(rec, opp.rating, score, settings));
        record.peak = rec.peak.max(record.rating);
    }
    record.reached_senior = rec.reached_senior || (record.rated && record.peak >= SENIOR_RATING);
    SideChange {
        before: rec.rating,
        after: record.rating,
        delta: record.rating - rec.rating,
        k,
        expected: expected_score(rec.rating, opp.rating),
        games: record.games,
        provisional: record.is_provisional(settings),
        record,
    }
}

/// Rates one game. Both changes are computed from the records before the game (normalized
/// first); the inputs are not modified. `score` is White's: 1, 0.5 or 0.
pub fn apply_game(
    white: &Record,
    black: &Record,
    score: f64,
    settings: &EloSettings,
) -> Result<GameChange, InvalidScore> {
    if !(0.0..=1.0).contains(&score) {
        return Err(InvalidScore(score));
    }
    let w = white.normalized();
    let b = black.normalized();
    Ok(GameChange {
        white: apply_side(&w, &b, score, settings),
        black: apply_side(&b, &w, 1.0 - score, settings),
    })
}

/// The official categories (`RATED_CATEGORIES`) indexed by id and by time control.
#[derive(Clone, Debug, Default)]
pub struct Categories {
    list: Vec<Category>,
    by_tc: HashMap<(i64, i64), usize>,
    by_id: HashMap<String, usize>,
}

impl Categories {
    /// Indexes a list of categories (the configuration's `categories`).
    pub fn new(list: &[Category]) -> Categories {
        let mut c = Categories { list: list.to_vec(), by_tc: HashMap::new(), by_id: HashMap::new() };
        for (i, cat) in c.list.iter().enumerate() {
            c.by_tc.insert((cat.base_ms, cat.inc_ms), i);
            c.by_id.insert(cat.id.clone(), i);
        }
        c
    }

    /// The categories of a loaded configuration.
    pub fn from_config(config: &crate::config::Config) -> Categories {
        Categories::new(&config.categories)
    }

    /// Category id of a time control: the official id (`3+2`) or [`CUSTOM_CATEGORY`].
    pub fn category_of(&self, base_ms: i64, inc_ms: i64) -> &str {
        self.by_tc.get(&(base_ms, inc_ms)).map_or(CUSTOM_CATEGORY, |&i| self.list[i].id.as_str())
    }

    /// The official category with this id (`None` for an unknown id and for `custom`).
    pub fn parse(&self, id: &str) -> Option<&Category> {
        self.by_id.get(id).map(|&i| &self.list[i])
    }

    /// Whether the id names an official (rated) category.
    pub fn is_official(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    /// The categories in configuration order.
    pub fn iter(&self) -> impl Iterator<Item = &Category> {
        self.list.iter()
    }
}

#[cfg(test)]
mod tests;
