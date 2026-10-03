//! The store side of the statistical model: integrity records, a player's analysed games, the
//! integrity update after a new analysed game and the population update of an ordinary game.
//!
//! The pure rules live in [`super::scoring`] and [`super::integrity`]; the functions here run them
//! inside store jobs. A player's update is split in two jobs so that the scoring (the costly
//! part) runs on a reader: [`score_player_games`] reads the games and scores them, then
//! [`apply_player_score`] reads the record as it is at the write and applies the memory rules to
//! it, so that a ban or a review committed between the two jobs is never overwritten.

use serde_json::{Map, Value};

use super::analysis::stats::Welford;
use super::integrity::{IntegrityLevel, IntegrityRecord, LevelUpdate, player_level};
use super::priors::Metric;
use super::scoring::{
    BucketStats, PlayerScore, Population, PopulationSource, SideRecord, SourceError, model, score_player,
    side_of, update_population_from_game,
};
use crate::ids::UserId;
use crate::store::{self, Db, IntegrityUpdate, PopulationUpdate, Sample, StoreError};

/// The store's form of a level.
pub fn to_store_level(level: IntegrityLevel) -> store::IntegrityLevel {
    match level {
        IntegrityLevel::None => store::IntegrityLevel::None,
        IntegrityLevel::Suspected => store::IntegrityLevel::Suspected,
        IntegrityLevel::HighConfidence => store::IntegrityLevel::HighConfidence,
        IntegrityLevel::Confirmed => store::IntegrityLevel::Confirmed,
    }
}

/// The anti-cheat's form of a stored level.
pub fn from_store_level(level: store::IntegrityLevel) -> IntegrityLevel {
    match level {
        store::IntegrityLevel::None => IntegrityLevel::None,
        store::IntegrityLevel::Suspected => IntegrityLevel::Suspected,
        store::IntegrityLevel::HighConfidence => IntegrityLevel::HighConfidence,
        store::IntegrityLevel::Confirmed => IntegrityLevel::Confirmed,
    }
}

/// A player's integrity record with the defaults of a player never scored.
pub fn read_integrity(db: &Db<'_>, user: UserId) -> Result<IntegrityRecord, StoreError> {
    let r = db.integrity().get(user)?;
    Ok(IntegrityRecord::from_stored(Some(r.level.as_str()), Some(r.score), r.evidence.as_ref()))
}

/// A player's level; a store error counts as `none` (callers that only weigh a player).
pub fn level_of(db: &Db<'_>, user: UserId) -> IntegrityLevel {
    db.integrity().get(user).map(|r| from_store_level(r.level)).unwrap_or_default()
}

/// Writes a player's level, score and evidence (the review fields are kept).
pub fn write_integrity(
    db: &Db<'_>,
    user: UserId,
    level: IntegrityLevel,
    score: f64,
    evidence: Map<String, Value>,
    at: i64,
) -> Result<(), StoreError> {
    db.integrity().set(
        user,
        &IntegrityUpdate {
            level: Some(to_store_level(level)),
            score: Some(score),
            evidence: Some(Some(Value::Object(evidence))),
            updated_at: Some(at),
            ..IntegrityUpdate::default()
        },
    )
}

/// The population statistics stored in the database, read on the job's connection.
pub struct DbSource<'a, 'c>(pub &'a Db<'c>);

impl PopulationSource for DbSource<'_, '_> {
    fn population_stats(&self, key: &str) -> Result<BucketStats, SourceError> {
        let mut out = BucketStats::default();
        for (name, s) in self.0.integrity().population_stats(key)? {
            if let Some(metric) = Metric::parse(&name) {
                out.set(metric, Welford { n: s.n as f64, mean: s.mean, m2: s.m2 });
            }
        }
        Ok(out)
    }
}

/// A player's latest analysed games ([`model::WINDOW_GAMES`]), newest first, as scoring records.
/// A store error gives no game (the player is scored on nothing, as before).
pub fn player_games(db: &Db<'_>, user: UserId) -> Vec<SideRecord> {
    let rows = db.analysis().for_user(user, model::WINDOW_GAMES as i64, true).unwrap_or_default();
    rows.iter().filter_map(|row| row.features.as_ref().and_then(|f| side_of(f, user))).collect()
}

/// A player's games and their scoring against `population` (a read job).
pub fn score_player_games(
    db: &Db<'_>,
    user: UserId,
    population: &Population,
) -> (Vec<SideRecord>, PlayerScore) {
    let games = player_games(db, user);
    let result = score_player(&games, population, &DbSource(db));
    (games, result)
}

/// Applies the memory rules to the record as it is now and writes it back (inside the caller's
/// write job). The caller logs `integrity.level` when the level changed, after the commit.
pub fn apply_player_score(
    db: &Db<'_>,
    user: UserId,
    population: &Population,
    games: &[SideRecord],
    result: &PlayerScore,
    now: i64,
) -> Result<LevelUpdate, StoreError> {
    let prev = read_integrity(db, user)?;
    let update = player_level(&prev, games, result, population, now);
    write_integrity(db, user, update.level, update.score, update.evidence.clone(), now)?;
    Ok(update)
}

/// Adds an analysed game of the ordinary random sample to the population statistics (inside the
/// caller's write job): the players' levels as stored, the observations merged one by one in
/// their order. Returns the sides added.
pub fn add_game_to_population(
    db: &Db<'_>,
    population: &Population,
    features: &Value,
    now: i64,
) -> Result<usize, StoreError> {
    let update = update_population_from_game(population, &DbSource(db), features, |u| level_of(db, u));
    let updates: Vec<PopulationUpdate> = update
        .observations
        .into_iter()
        .map(|o| PopulationUpdate { key: o.key, sample: Sample::Values(vec![o.value]) })
        .collect();
    db.integrity().update_population(&updates, now)?;
    Ok(update.added)
}
