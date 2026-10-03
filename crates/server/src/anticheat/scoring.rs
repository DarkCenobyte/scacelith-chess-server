//! Statistical assistance model, version 1: per-game engine features become a player's integrity
//! level. It only ever proposes a level and the evidence (with the numbers that justify it) for a
//! moderator; it never bans and never changes matchmaking. docs/ANTICHEAT.md explains the model:
//!
//! 0. Analysis profile. Features are only comparable between games analysed alike
//!    ([`super::analysis::analyzer::analysis_profile`]). A [`Population`] holds the games of one
//!    profile, and a player is scored on their games of that profile only.
//! 1. Population. For every (category, 100-point rating bucket) the server keeps Welford
//!    statistics of each per-game metric, blended with the priors ([`super::priors`]) worth
//!    [`PRIOR_GAMES`] games. Games of players already flagged high_confidence or confirmed are
//!    left out of the population, values are winsorised.
//! 2. Per game, each metric becomes an oriented z-score (positive = more engine-like) against the
//!    population of the same category at the player's rating: the smallest one over the rating
//!    band (+/-100, +/-400 while provisional), the benefit of the doubt.
//! 3. Metrics are grouped into signals: Q move quality (accuracy, ACPL), E engine choice in
//!    complex positions, J sudden jump against the player's own history, T timing. A group's
//!    score over a window (last 30 and last 10 analysed games) is the moves-weighted mean z,
//!    shrunk with n/(n+k) and expressed in units of the between-player spread tau.
//! 4. Levels: suspected (>= 5 games and one accuracy-type score >= 3.5, or a jump J >= 2.5 with a
//!    recent-window Q or E >= 2.5); high_confidence (>= 10 games and >= 300 scored moves,
//!    accuracy-type >= 3.0 and timing >= 1.5 and (A+T)/sqrt(2) >= 3.5); confirmed is never set
//!    here. Timing alone never flags anyone. The memory rules on top (hysteresis, moderator
//!    reviews) are [`super::integrity::player_level`].
//!
//! Every computation keeps the former server's operation order: scores, evidence and stored
//! statistics are bit-identical.

use std::collections::HashMap;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use super::analysis::stats::{Welford, clamp, mean};
use super::integrity::{IntegrityLevel, parse_maybe_json};
use super::num::{js_number, js_round, js_to_number, js_truthy, json_num, json_opt, to_fixed};
use super::priors::{Metric, PRIOR_GAMES, TimeClass, prior_for, time_class, time_class_of_category};
use crate::clock::SharedClock;
use crate::ids::UserId;

/// Parameters of the model (docs/ANTICHEAT.md).
pub mod model {
    /// Model version, stored with the evidence.
    pub const VERSION: u32 = 1;
    /// Games of the long window.
    pub const WINDOW_GAMES: usize = 30;
    /// Games of the recent window.
    pub const RECENT_GAMES: usize = 10;
    /// Games with fewer scored moves do not count.
    pub const MIN_MOVES_PER_GAME: f64 = 8.0;
    /// E uses games with at least this many complex positions.
    pub const MIN_COMPLEX_PER_GAME: f64 = 3.0;
    /// k of n/(n+k) for Q and T (n = scored moves).
    pub const SHRINK_MOVES: f64 = 150.0;
    /// k for E (n = complex positions).
    pub const SHRINK_COMPLEX: f64 = 50.0;
    /// Between-player spread of Q.
    pub const TAU_Q: f64 = 0.4;
    /// Between-player spread of E.
    pub const TAU_E: f64 = 0.45;
    /// Between-player spread of T.
    pub const TAU_T: f64 = 0.5;
    /// A single game cannot weigh more than this many standard deviations.
    pub const Z_CLAMP: f64 = 6.0;

    /// Rules of the `suspected` level.
    pub mod suspected {
        pub const MIN_GAMES: usize = 5;
        pub const ACCURACY_TYPE: f64 = 3.5;
        /// A jump J and the recent-window quality both at least this high.
        pub const JUMP_WITH_RECENT: f64 = 2.5;
        /// A suspected player stays suspected until the score drops this much below the threshold.
        pub const HYSTERESIS: f64 = 0.5;
    }

    /// Rules of the `high_confidence` level.
    pub mod high {
        pub const MIN_GAMES: usize = 10;
        pub const MIN_MOVES: f64 = 300.0;
        pub const ACCURACY_TYPE: f64 = 3.0;
        pub const TIMING: f64 = 1.5;
        pub const COMBINED: f64 = 3.5;
    }

    /// The jump signal J: the recent games beat the player's own history by more than a plausible
    /// honest improvement, the jump lasts, and it lands above peers.
    pub mod jump {
        pub const MIN_RECENT: usize = 5;
        pub const MIN_EARLIER: usize = 8;
        pub const HONEST_IMPROVEMENT: f64 = 0.75;
        pub const MIN_RECENT_LEVEL: f64 = 1.0;
        pub const LASTING_FRACTION: f64 = 0.7;
        pub const SD_FLOOR: f64 = 0.6;
    }

    /// Rating band of an established player.
    pub const RATING_BAND_ESTABLISHED: f64 = 100.0;
    /// Rating band of a provisional player.
    pub const RATING_BAND_PROVISIONAL: f64 = 400.0;
    /// Below this many rating games a player is provisional.
    pub const PROVISIONAL_GAMES: f64 = 30.0;
    /// A side needs this many scored moves to join the population.
    pub const POPULATION_MIN_MOVES: f64 = 10.0;
    /// Population values are winsorised at this many standard deviations.
    pub const WINSOR_Z: f64 = 4.0;
}

/// Population levels left out of the population statistics.
pub const POPULATION_SKIP_LEVELS: [IntegrityLevel; 2] =
    [IntegrityLevel::HighConfidence, IntegrityLevel::Confirmed];

/// Floor of the effective standard deviation of a metric.
pub fn sd_floor(metric: Metric) -> f64 {
    match metric {
        Metric::Accuracy => 3.0,
        Metric::Acpl => 5.0,
        Metric::T1Deep | Metric::T1Fast => 0.04,
        Metric::T1Complex => 0.08,
        Metric::TimeCorr | Metric::TimeCv => 0.12,
    }
}

/// Orientation of a metric: +1 when a higher value looks more like an engine.
pub fn metric_sign(metric: Metric) -> f64 {
    match metric {
        Metric::Accuracy | Metric::T1Deep | Metric::T1Fast | Metric::T1Complex => 1.0,
        Metric::Acpl | Metric::TimeCorr | Metric::TimeCv => -1.0,
    }
}

/// 100-point rating bucket (lower bound), clamped to 500..=2900 (1500 for a non-finite rating).
pub fn bucket_of_rating(rating: f64) -> i64 {
    let r = if rating.is_finite() { rating } else { 1500.0 };
    clamp((r / 100.0).floor() * 100.0, 500.0, 2900.0) as i64
}

// ---- population -------------------------------------------------------------------------------

/// Server statistics of one (profile, category, bucket), per metric.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BucketStats {
    metrics: [Option<Welford>; 7],
}

impl BucketStats {
    /// The statistics of a metric.
    pub fn get(&self, metric: Metric) -> Option<Welford> {
        self.metrics[metric.index()]
    }

    /// Sets the statistics of a metric.
    pub fn set(&mut self, metric: Metric, stats: Welford) {
        self.metrics[metric.index()] = Some(stats);
    }

    /// Whether no metric has statistics.
    pub fn is_empty(&self) -> bool {
        self.metrics.iter().all(Option::is_none)
    }

    /// Statistics in a stored JSON shape: `{ <metric>: { n, mean, m2 } }` (or under `metrics`, `m2`
    /// falling back to `variance * (n - 1)`), an array of `{ metric, n, mean, m2 }` rows, or JSON
    /// text of either.
    pub fn from_json(raw: &Value) -> BucketStats {
        let mut out = BucketStats::default();
        let Some(v) = parse_maybe_json(raw) else { return out };
        // `+x || 0`.
        let num = |o: &Value, k: &str| {
            let x = js_to_number(o.get(k));
            if x.is_nan() { 0.0 } else { x }
        };
        if let Value::Array(rows) = &*v {
            for r in rows {
                let Some(metric) = r.get("metric").and_then(Value::as_str).and_then(Metric::parse) else {
                    continue;
                };
                out.set(metric, Welford { n: num(r, "n"), mean: num(r, "mean"), m2: num(r, "m2") });
            }
            return out;
        }
        let src = v.get("metrics").filter(|m| m.is_object()).unwrap_or(&v);
        for metric in Metric::ALL {
            let Some(s) = src.get(metric.name()).filter(|s| s.is_object()) else { continue };
            let n = js_to_number(s.get("n"));
            if !n.is_finite() {
                continue;
            }
            let m2 = match js_to_number(s.get("m2")) {
                m2 if m2.is_finite() => m2,
                _ => num(s, "variance") * (n - 1.0).max(0.0),
            };
            out.set(metric, Welford { n, mean: num(s, "mean"), m2 });
        }
        out
    }
}

/// Error of a population statistics read.
pub type SourceError = Box<dyn std::error::Error + Send + Sync>;

/// Where a [`Population`] reads the stored statistics: the store's integrity tables, through
/// whatever connection or transaction the caller holds (it is passed to each computation).
pub trait PopulationSource {
    /// The stored statistics of `key` (`<profile>|<category>|<bucket>`, or `<category>|<bucket>`
    /// without a profile). A failed read leaves the bucket on the priors until the next reload.
    fn population_stats(&self, key: &str) -> Result<BucketStats, SourceError>;
}

/// A source without stored statistics: the priors and what the population learned in memory.
#[derive(Clone, Copy, Debug, Default)]
pub struct PriorsOnly;

impl PopulationSource for PriorsOnly {
    fn population_stats(&self, _key: &str) -> Result<BucketStats, SourceError> {
        Ok(BucketStats::default())
    }
}

/// One value to merge into the stored running statistics of `key`
/// (`<profile>|<category>|<bucket>|<metric>`), as one observation of count 1.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub key: String,
    pub value: f64,
}

/// Effective distribution of a metric: the prior as [`PRIOR_GAMES`] pseudo-games pooled with the
/// server's data.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Effective {
    pub mean: f64,
    pub sd: f64,
    /// Games of server data in it.
    pub n: f64,
}

struct Cached {
    at: i64,
    stats: BucketStats,
}

/// Population statistics of one analysis profile per (category, rating bucket), blended with
/// the priors. Stored statistics are read from the [`PopulationSource`] given to each
/// computation, through a cache reloaded every `reload_ms`. [`Population::update`] updates the
/// cache and returns the observations the caller stores (in one store write job): the store is
/// the source of truth, and when that write fails [`Population::invalidate`] drops the cached
/// view. One population can be shared by the analysis loops (it is `Sync`).
pub struct Population {
    profile: Option<String>,
    reload_ms: i64,
    clock: SharedClock,
    cache: Mutex<HashMap<String, Cached>>,
}

impl std::fmt::Debug for Population {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Population")
            .field("profile", &self.profile)
            .field("reload_ms", &self.reload_ms)
            .finish()
    }
}

impl Population {
    /// Default cache lifetime of a bucket's statistics.
    pub const RELOAD_MS: i64 = 600_000;

    /// A population of the games analysed with `profile` (`None`: tests and tools).
    pub fn new(profile: Option<String>, clock: SharedClock) -> Population {
        Population {
            profile: profile.filter(|p| !p.is_empty()),
            reload_ms: Self::RELOAD_MS,
            clock,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Changes the cache lifetime.
    pub fn with_reload_ms(mut self, reload_ms: i64) -> Population {
        self.reload_ms = reload_ms;
        self
    }

    /// The analysis profile of the games it holds.
    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// Statistics key of a bucket.
    pub fn key(&self, category: &str, bucket: i64) -> String {
        match &self.profile {
            Some(p) => format!("{p}|{category}|{bucket}"),
            None => format!("{category}|{bucket}"),
        }
    }

    /// Whether a record analysed with `profile` belongs to this population.
    pub fn holds(&self, profile: Option<&str>) -> bool {
        profile.filter(|p| !p.is_empty()) == self.profile.as_deref()
    }

    /// Server statistics of a bucket (empty when the source fails: priors only).
    pub fn raw(&self, src: &dyn PopulationSource, category: &str, bucket: i64) -> BucketStats {
        let key = self.key(category, bucket);
        let now = self.clock.wall_ms();
        if let Some(c) = self.cache.lock().get(&key)
            && now - c.at < self.reload_ms
        {
            return c.stats;
        }
        // Read without the lock: another analysis loop may be using another bucket.
        let stats = src.population_stats(&key).unwrap_or_default();
        self.cache.lock().insert(key, Cached { at: now, stats });
        stats
    }

    /// Effective mean and standard deviation of a metric (pooled variance, floored).
    pub fn effective(
        &self,
        src: &dyn PopulationSource,
        metric: Metric,
        category: &str,
        bucket: i64,
        tc: TimeClass,
    ) -> Effective {
        let p = prior_for(metric, (bucket + 50) as f64, tc);
        let d = self.raw(src, category, bucket).get(metric);
        let n0 = PRIOR_GAMES;
        let (mut mean_v, mut var_v, mut n) = (p.mean, p.sd * p.sd, 0.0);
        if let Some(d) = d.filter(|d| d.n > 0.0) {
            n = d.n;
            let tot = n0 + d.n;
            mean_v = (n0 * p.mean + d.n * d.mean) / tot;
            let dp = p.mean - mean_v;
            let dd = d.mean - mean_v;
            var_v = (n0 * (p.sd * p.sd + dp * dp) + d.m2 + d.n * (dd * dd)) / tot;
        }
        Effective { mean: mean_v, sd: var_v.max(0.0).sqrt().max(sd_floor(metric)), n }
    }

    /// Adds one player's per-game features to a bucket (winsorised at +/-[`model::WINSOR_Z`] sd of
    /// the current effective distribution) and returns the observations to store (empty when the
    /// side has no usable metric).
    pub fn update(
        &self,
        src: &dyn PopulationSource,
        category: &str,
        bucket: i64,
        side: &SideRecord,
        tc: TimeClass,
    ) -> Vec<Observation> {
        let key = self.key(category, bucket);
        let mut stats = self.raw(src, category, bucket);
        let mut observations = Vec::new();
        for metric in Metric::ALL {
            let Some(x) = side.value(metric) else { continue };
            if metric == Metric::T1Complex && side.n_complex < model::MIN_COMPLEX_PER_GAME {
                continue;
            }
            let eff = self.effective(src, metric, category, bucket, tc);
            let w = model::WINSOR_Z * eff.sd;
            let value = clamp(x, eff.mean - w, eff.mean + w);
            let mut w = stats.get(metric).unwrap_or_default();
            w.push(value);
            stats.set(metric, w);
            observations.push(Observation { key: format!("{key}|{}", metric.name()), value });
        }
        if !observations.is_empty() {
            self.cache.lock().insert(key, Cached { at: self.clock.wall_ms(), stats });
        }
        observations
    }

    /// Forgets the cached statistics (they are read again from the source).
    pub fn invalidate(&self) {
        self.cache.lock().clear();
    }
}

// A population with the statistics source of one computation.
#[derive(Clone, Copy)]
struct View<'a> {
    pop: &'a Population,
    src: &'a dyn PopulationSource,
}

impl View<'_> {
    fn effective(&self, metric: Metric, category: &str, bucket: i64, tc: TimeClass) -> Effective {
        self.pop.effective(self.src, metric, category, bucket, tc)
    }
}

// ---- per game ----------------------------------------------------------------------------------

/// A number field of a stored record, read as JavaScript read it: the former server told an
/// absent field from a `null` one (`+null` is 0, `null ?? 1500` is 1500).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum NumField {
    #[default]
    Absent,
    Null,
    Number(f64),
}

impl NumField {
    fn of(v: Option<&Value>) -> NumField {
        match v {
            None => NumField::Absent,
            Some(Value::Null) => NumField::Null,
            Some(v) => v.as_f64().map_or(NumField::Absent, NumField::Number),
        }
    }

    /// `+x`: absent is NaN, null is 0.
    pub fn to_number(self) -> f64 {
        match self {
            NumField::Absent => f64::NAN,
            NumField::Null => 0.0,
            NumField::Number(x) => x,
        }
    }

    /// `x ?? default`.
    pub fn or(self, default: f64) -> f64 {
        match self {
            NumField::Number(x) => x,
            _ => default,
        }
    }

    fn to_json(self) -> Value {
        match self {
            NumField::Number(x) => json_num(x),
            _ => Value::Null,
        }
    }
}

/// One player's side of an analysed game, as the scoring reads it ([`side_of`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SideRecord {
    pub game_id: f64,
    pub category: String,
    pub base_ms: f64,
    pub inc_ms: f64,
    /// End of the game (the analysis time when unknown).
    pub ended_at: f64,
    pub analysed_at: f64,
    pub profile: Option<String>,
    pub user_id: UserId,
    pub rating: NumField,
    pub rating_games: NumField,
    /// Scored moves.
    pub n: f64,
    pub n_complex: f64,
    pub n_timed: f64,
    /// Metric values by [`Metric::index`] (finite numbers only).
    pub values: [Option<f64>; 7],
}

impl SideRecord {
    /// The value of a metric, when present.
    pub fn value(&self, metric: Metric) -> Option<f64> {
        self.values[metric.index()]
    }

    /// Sets the value of a metric.
    pub fn set_value(&mut self, metric: Metric, value: Option<f64>) {
        self.values[metric.index()] = value.filter(|v| v.is_finite());
    }

    fn time_class(&self) -> TimeClass {
        if self.base_ms != 0.0 {
            time_class(self.base_ms, self.inc_ms)
        } else {
            time_class_of_category(&self.category)
        }
    }
}

// `x || 0` of a JSON field.
fn num_or_zero(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).filter(|x| *x != 0.0 && !x.is_nan()).unwrap_or(0.0)
}

fn side_fields(base: SideRecord, side: &Value) -> SideRecord {
    let mut s = SideRecord {
        user_id: side.get("userId").and_then(Value::as_u64).and_then(|u| u32::try_from(u).ok()).unwrap_or(0),
        rating: NumField::of(side.get("rating")),
        rating_games: NumField::of(side.get("ratingGames")),
        n: num_or_zero(side.get("n")),
        n_complex: num_or_zero(side.get("nComplex")),
        n_timed: num_or_zero(side.get("nTimed")),
        ..base
    };
    for metric in Metric::ALL {
        s.set_value(metric, side.get(metric.name()).and_then(Value::as_f64));
    }
    s
}

fn record_base(f: &Value) -> SideRecord {
    let str_or = |k: &str, d: &str| {
        f.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(d).to_string()
    };
    let analysed_at = num_or_zero(f.get("analysedAt"));
    let ended = num_or_zero(f.get("endedAt"));
    SideRecord {
        game_id: f.get("gameId").and_then(Value::as_f64).unwrap_or(0.0),
        category: str_or("category", "custom"),
        base_ms: num_or_zero(f.get("baseMs")),
        inc_ms: num_or_zero(f.get("incMs")),
        ended_at: if ended != 0.0 { ended } else { analysed_at },
        analysed_at,
        profile: f.get("profile").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
        ..SideRecord::default()
    }
}

/// One player's side of a features record (as stored: an object or JSON text), `None` when the
/// record has no such player or is unreadable.
pub fn side_of(features: &Value, user_id: UserId) -> Option<SideRecord> {
    let f = parse_maybe_json(features)?;
    let (white, black) = (f.get("white")?, f.get("black")?);
    if !white.is_object() || !black.is_object() {
        return None;
    }
    let is = |s: &Value| s.get("userId").and_then(Value::as_f64) == Some(f64::from(user_id));
    let side = if is(white) {
        white
    } else if is(black) {
        black
    } else {
        return None;
    };
    Some(side_fields(record_base(&f), side))
}

/// One player's side of a features record as JSON, the way the former server displayed it
/// (`admin integrity show`): the game fields (`gameId`, `category`, `baseMs`, `incMs`, `endedAt`,
/// `analysedAt`, `profile`) followed by every field of the side.
pub fn side_json(features: &Value, user_id: UserId) -> Option<Value> {
    let f = parse_maybe_json(features)?;
    let is = |k: &str| {
        f.get(k)
            .filter(|s| s.is_object() && s.get("userId").and_then(Value::as_f64) == Some(f64::from(user_id)))
    };
    let (white, black) = (f.get("white")?, f.get("black")?);
    if !white.is_object() || !black.is_object() {
        return None;
    }
    let side = is("white").or_else(|| is("black"))?.as_object()?;
    // `f[k] || fallback`.
    let or = |k: &str, fallback: Value| f.get(k).filter(|v| js_truthy(Some(v))).cloned().unwrap_or(fallback);
    let mut out = Map::new();
    if let Some(id) = f.get("gameId") {
        out.insert("gameId".into(), id.clone());
    }
    out.insert("category".into(), or("category", json!("custom")));
    out.insert("baseMs".into(), or("baseMs", json!(0)));
    out.insert("incMs".into(), or("incMs", json!(0)));
    out.insert("endedAt".into(), or("endedAt", or("analysedAt", json!(0))));
    out.insert("analysedAt".into(), or("analysedAt", json!(0)));
    out.insert("profile".into(), or("profile", Value::Null));
    for (k, v) in side {
        out.insert(k.clone(), v.clone());
    }
    Some(Value::Object(out))
}

fn oriented_z(metric: Metric, x: f64, g: &SideRecord, pop: View<'_>) -> f64 {
    let tc = g.time_class();
    let r = g.rating.to_number();
    let rating = if r.is_finite() { r } else { 1500.0 };
    let rg = g.rating_games.to_number();
    let provisional = rg.is_finite() && rg < model::PROVISIONAL_GAMES;
    let band = if provisional { model::RATING_BAND_PROVISIONAL } else { model::RATING_BAND_ESTABLISHED };
    let mut best = f64::INFINITY;
    let mut b = bucket_of_rating(rating - band);
    let last = bucket_of_rating(rating + band);
    while b <= last {
        let st = pop.effective(metric, &g.category, b, tc);
        let z = metric_sign(metric) * (x - st.mean) / st.sd;
        if z < best {
            best = z;
        }
        b += 100;
    }
    clamp(best, -model::Z_CLAMP, model::Z_CLAMP)
}

/// Per-game group z-scores of one side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GameZ {
    /// Move quality: mean of z(accuracy) and z(ACPL).
    pub zq: Option<f64>,
    /// Engine choice in complex positions.
    pub ze: Option<f64>,
    /// Timing: mean of z(time correlation) and z(time CV).
    pub zt: Option<f64>,
    /// Weight of Q (scored moves).
    pub n: f64,
    /// Weight of E (complex positions).
    pub n_e: f64,
    /// Weight of T (timed moves).
    pub n_t: f64,
    /// Oriented z-score of each metric, by [`Metric::index`].
    pub z: [Option<f64>; 7],
}

/// Per-game group z-scores of one side against the population.
pub fn game_z(g: &SideRecord, pop: &Population, src: &dyn PopulationSource) -> GameZ {
    game_z_in(g, View { pop, src })
}

fn game_z_in(g: &SideRecord, pop: View<'_>) -> GameZ {
    let mut z = [None; 7];
    for metric in
        [Metric::Accuracy, Metric::Acpl, Metric::T1Deep, Metric::T1Fast, Metric::TimeCorr, Metric::TimeCv]
    {
        if let Some(x) = g.value(metric) {
            z[metric.index()] = Some(oriented_z(metric, x, g, pop));
        }
    }
    let n_e = match g.value(Metric::T1Complex) {
        Some(_) if g.n_complex >= model::MIN_COMPLEX_PER_GAME => g.n_complex,
        _ => 0.0,
    };
    if n_e != 0.0 {
        z[Metric::T1Complex.index()] =
            g.value(Metric::T1Complex).map(|x| oriented_z(Metric::T1Complex, x, g, pop));
    }
    let q: Vec<f64> = [Metric::Accuracy, Metric::Acpl].iter().filter_map(|m| z[m.index()]).collect();
    let t: Vec<f64> = [Metric::TimeCorr, Metric::TimeCv].iter().filter_map(|m| z[m.index()]).collect();
    let timed = g.value(Metric::TimeCorr).is_some();
    GameZ {
        zq: (!q.is_empty()).then(|| mean(&q)),
        ze: if n_e != 0.0 { z[Metric::T1Complex.index()] } else { None },
        zt: (timed && !t.is_empty()).then(|| mean(&t)),
        n: g.n,
        n_e,
        n_t: if timed { [g.n_timed, g.n].into_iter().find(|&v| v != 0.0).unwrap_or(0.0) } else { 0.0 },
        z,
    }
}

// ---- per player --------------------------------------------------------------------------------

/// Score of one signal over a window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GroupScore {
    /// Shrunk mean z in units of tau.
    pub score: f64,
    /// Weighted mean z.
    pub mean_z: f64,
    /// Total weight (moves or complex positions).
    pub n: f64,
    /// n/(n+k).
    pub shrink: f64,
}

struct Item<'a> {
    g: &'a SideRecord,
    gz: GameZ,
}

fn group_score(items: &[Item<'_>], value: fn(&GameZ) -> (Option<f64>, f64), k: f64, tau: f64) -> GroupScore {
    let (mut sw, mut s) = (0.0, 0.0);
    for it in items {
        let (v, w) = value(&it.gz);
        let Some(v) = v else { continue };
        if w == 0.0 || w.is_nan() {
            continue;
        }
        sw += w;
        s += w * v;
    }
    if sw == 0.0 {
        return GroupScore::default();
    }
    let mean_z = s / sw;
    let shrink = sw / (sw + k);
    GroupScore { score: shrink * mean_z / tau, mean_z, n: sw, shrink }
}

/// Scores of the three window signals.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowScores {
    pub games: usize,
    pub moves: f64,
    pub q: GroupScore,
    pub e: GroupScore,
    pub t: GroupScore,
}

fn window_scores(items: &[Item<'_>]) -> WindowScores {
    WindowScores {
        games: items.len(),
        moves: items.iter().fold(0.0, |a, it| a + it.g.n),
        q: group_score(items, |z| (z.zq, z.n), model::SHRINK_MOVES, model::TAU_Q),
        e: group_score(items, |z| (z.ze, z.n_e), model::SHRINK_COMPLEX, model::TAU_E),
        t: group_score(items, |z| (z.zt, z.n_t), model::SHRINK_MOVES, model::TAU_T),
    }
}

/// The sudden lasting jump signal J.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JumpScore {
    pub score: f64,
    pub effect: f64,
    pub recent_mean: Option<f64>,
    pub earlier_mean: Option<f64>,
    pub lasting: bool,
    pub fraction_above: Option<f64>,
    pub t: Option<f64>,
    pub recent_games: usize,
    pub earlier_games: usize,
}

// Quality of the recent games against the player's own earlier games (chrono: oldest first).
fn jump_score(chrono: &[&Item<'_>]) -> JumpScore {
    use model::jump as j;
    let with_q: Vec<f64> = chrono.iter().filter_map(|it| it.gz.zq).collect();
    let split = with_q.len().saturating_sub(model::RECENT_GAMES);
    let (earlier, recent) = with_q.split_at(split);
    let mut res =
        JumpScore { recent_games: recent.len(), earlier_games: earlier.len(), ..JumpScore::default() };
    if recent.len() < j::MIN_RECENT || earlier.len() < j::MIN_EARLIER {
        return res;
    }
    let (mr, me) = (mean(recent), mean(earlier));
    let all: Vec<f64> = recent.iter().map(|x| x - mr).chain(earlier.iter().map(|x| x - me)).collect();
    let ss = all.iter().fold(0.0, |a, x| a + x * x);
    let pooled_sd = j::SD_FLOOR.max((ss / 1usize.max(all.len().saturating_sub(2)) as f64).sqrt());
    let se = pooled_sd * (1.0 / recent.len() as f64 + 1.0 / earlier.len() as f64).sqrt();
    let t = (mr - me - j::HONEST_IMPROVEMENT) / se;
    let above = recent.iter().filter(|&&x| x > me + 0.5 * pooled_sd).count() as f64 / recent.len() as f64;
    res.effect = mr - me;
    res.recent_mean = Some(mr);
    res.earlier_mean = Some(me);
    res.lasting = above >= j::LASTING_FRACTION;
    res.fraction_above = Some(above);
    res.t = Some(t);
    // Only a jump up to a level above peers counts (coming back from a bad streak is not
    // suspicious).
    if res.lasting && mr >= j::MIN_RECENT_LEVEL && t > 0.0 {
        res.score = t;
    }
    res
}

/// `Math.round(x * 100) / 100`, `None` for a non-finite value.
pub fn r2(x: f64) -> Option<f64> {
    x.is_finite().then(|| js_round(x * 100.0) / 100.0)
}

/// The rounded signals of a player.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Groups {
    pub q: Option<f64>,
    pub e: Option<f64>,
    pub j: Option<f64>,
    pub t: Option<f64>,
    pub accuracy_type: Option<f64>,
}

impl Groups {
    /// `{ Q, E, J, T, accuracyType }`.
    pub fn to_json(&self) -> Value {
        json!({ "Q": json_opt(self.q), "E": json_opt(self.e), "J": json_opt(self.j), "T": json_opt(self.t),
            "accuracyType": json_opt(self.accuracy_type) })
    }
}

/// A rounded window summary.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowSummary {
    pub games: usize,
    pub moves: f64,
    pub q: [Option<f64>; 4],
    pub e: [Option<f64>; 4],
    pub t: [Option<f64>; 4],
}

impl WindowSummary {
    fn of(w: &WindowScores) -> WindowSummary {
        let g = |x: &GroupScore| [r2(x.score), r2(x.mean_z), Some(x.n), r2(x.shrink)];
        WindowSummary { games: w.games, moves: w.moves, q: g(&w.q), e: g(&w.e), t: g(&w.t) }
    }

    /// `{ games, moves, Q, E, T }`, each group `{ score, meanZ, n, shrink }`.
    pub fn to_json(&self) -> Value {
        let g = |x: &[Option<f64>; 4]| json!({ "score": json_opt(x[0]), "meanZ": json_opt(x[1]), "n": json_opt(x[2]), "shrink": json_opt(x[3]) });
        json!({ "games": self.games, "moves": json_num(self.moves), "Q": g(&self.q), "E": g(&self.e), "T": g(&self.t) })
    }
}

/// The scoring of a player from their analysed games.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerScore {
    /// The model's level (never `confirmed`).
    pub level: IntegrityLevel,
    /// Overall score, rounded to 2 decimals.
    pub score: f64,
    /// Window that made the player high_confidence (`all` or `recent`).
    pub high_window: Option<&'static str>,
    /// Why the level was reached.
    pub trigger: Option<String>,
    pub groups: Groups,
    pub window_all: WindowSummary,
    pub window_recent: WindowSummary,
    pub jump: JumpScore,
    /// Human-readable reasons with the raw numbers.
    pub reasons: Vec<String>,
    /// The scored games, newest first.
    pub per_game: Vec<PerGame>,
    pub games: usize,
    pub moves: f64,
}

/// One scored game in a [`PlayerScore`].
#[derive(Clone, Debug, PartialEq)]
pub struct PerGame {
    pub game_id: f64,
    pub category: String,
    pub rating: NumField,
    pub ended_at: f64,
    pub n: f64,
    pub n_complex: f64,
    pub values: [Option<f64>; 7],
    pub zq: Option<f64>,
    pub ze: Option<f64>,
    pub zt: Option<f64>,
}

impl PerGame {
    /// `{ gameId, category, rating, endedAt, n, accuracy, acpl, t1Deep, t1Fast, t1Complex,
    /// nComplex, timeCorr, timeCv, zQ, zE, zT }` (missing values are `null`, an absent rating is
    /// left out).
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        let v = |metric: Metric| json_opt(self.values[metric.index()]);
        m.insert("gameId".into(), json_num(self.game_id));
        m.insert("category".into(), json!(self.category));
        if self.rating != NumField::Absent {
            m.insert("rating".into(), self.rating.to_json());
        }
        m.insert("endedAt".into(), json_num(self.ended_at));
        m.insert("n".into(), json_num(self.n));
        for metric in [Metric::Accuracy, Metric::Acpl, Metric::T1Deep, Metric::T1Fast, Metric::T1Complex] {
            m.insert(metric.name().into(), v(metric));
        }
        m.insert("nComplex".into(), json_num(self.n_complex));
        m.insert("timeCorr".into(), v(Metric::TimeCorr));
        m.insert("timeCv".into(), v(Metric::TimeCv));
        m.insert("zQ".into(), json_opt(self.zq));
        m.insert("zE".into(), json_opt(self.ze));
        m.insert("zT".into(), json_opt(self.zt));
        Value::Object(m)
    }
}

impl PlayerScore {
    /// `{ all, recent }` window summaries.
    pub fn windows_json(&self) -> Value {
        json!({ "all": self.window_all.to_json(), "recent": self.window_recent.to_json() })
    }

    /// The jump summary, rounded.
    pub fn jump_json(&self) -> Value {
        let j = &self.jump;
        let r = |x: Option<f64>| json_opt(x.and_then(r2));
        json!({ "score": r(Some(j.score)), "effect": r(Some(j.effect)), "recentMean": r(j.recent_mean),
            "earlierMean": r(j.earlier_mean), "lasting": j.lasting, "fractionAbove": r(j.fraction_above),
            "recentGames": j.recent_games, "earlierGames": j.earlier_games })
    }

    /// The trigger (`null` when none).
    pub fn trigger_json(&self) -> Value {
        self.trigger.as_ref().map_or(Value::Null, |t| json!(t))
    }

    /// The reasons as a JSON array.
    pub fn reasons_json(&self) -> Value {
        json!(self.reasons)
    }

    /// The whole result in the former shape.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("level".into(), json!(self.level.as_str()));
        m.insert("score".into(), json_num(self.score));
        m.insert("highWindow".into(), self.high_window.map_or(Value::Null, |w| json!(w)));
        m.insert("trigger".into(), self.trigger_json());
        m.insert("groups".into(), self.groups.to_json());
        m.insert("windows".into(), self.windows_json());
        m.insert("jump".into(), self.jump_json());
        m.insert("reasons".into(), self.reasons_json());
        m.insert("perGame".into(), Value::Array(self.per_game.iter().map(PerGame::to_json).collect()));
        m.insert("games".into(), json!(self.games));
        m.insert("moves".into(), json_num(self.moves));
        Value::Object(m)
    }
}

/// Scores a player from their analysed games of the population's analysis profile (the others
/// are left out), in any order.
pub fn score_player(games: &[SideRecord], pop: &Population, src: &dyn PopulationSource) -> PlayerScore {
    let view = View { pop, src };
    let mut usable: Vec<&SideRecord> = games
        .iter()
        .filter(|g| g.n >= model::MIN_MOVES_PER_GAME && pop.holds(g.profile.as_deref()))
        .collect();
    usable.sort_by(|a, b| {
        let by_end = b.ended_at - a.ended_at;
        let d = if by_end != 0.0 { by_end } else { b.analysed_at - a.analysed_at };
        d.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal)
    });
    usable.truncate(model::WINDOW_GAMES);
    let items: Vec<Item<'_>> = usable.iter().map(|g| Item { g, gz: game_z_in(g, view) }).collect();
    let all = window_scores(&items);
    let recent = window_scores(&items[..items.len().min(model::RECENT_GAMES)]);
    let chrono: Vec<&Item<'_>> = items.iter().rev().collect();
    let jump = jump_score(&chrono);

    let q = all.q.score.max(recent.q.score);
    let e = all.e.score.max(recent.e.score);
    let t = all.t.score.max(recent.t.score);
    let accuracy_type = q.max(e).max(jump.score);

    use model::{high as h, suspected as s};
    let (mut level, mut trigger) = (IntegrityLevel::None, None);
    if all.games >= s::MIN_GAMES && accuracy_type >= s::ACCURACY_TYPE {
        level = IntegrityLevel::Suspected;
        trigger = Some(format!(
            "one accuracy-type signal >= {} over >= {} games",
            js_number(s::ACCURACY_TYPE),
            s::MIN_GAMES
        ));
    }
    let recent_a = recent.q.score.max(recent.e.score);
    if level == IntegrityLevel::None
        && all.games >= s::MIN_GAMES
        && jump.score >= s::JUMP_WITH_RECENT
        && recent_a >= s::JUMP_WITH_RECENT
    {
        level = IntegrityLevel::Suspected;
        let j = js_number(s::JUMP_WITH_RECENT);
        trigger = Some(format!("sudden lasting jump (J >= {j}) with recent games far above peers (>= {j})"));
    }
    let mut high_window = None;
    for (name, w) in [("all", &all), ("recent", &recent)] {
        if w.games < h::MIN_GAMES || w.moves < h::MIN_MOVES {
            continue;
        }
        let a = w.q.score.max(w.e.score).max(jump.score);
        let tw = w.t.score;
        if a >= h::ACCURACY_TYPE && tw >= h::TIMING && (a + tw) / std::f64::consts::SQRT_2 >= h::COMBINED {
            level = IntegrityLevel::HighConfidence;
            high_window = Some(name);
            let which = if name == "all" { format!("last {}", w.games) } else { "recent".to_string() };
            trigger = Some(format!(
                "accuracy-type {} and timing {} agree over the {which} games ({} moves)",
                to_fixed(a, 2),
                to_fixed(tw, 2),
                js_number(w.moves)
            ));
            break;
        }
    }
    let score = accuracy_type.max((accuracy_type + t.max(0.0)) / std::f64::consts::SQRT_2);

    let per_game = items
        .iter()
        .map(|it| PerGame {
            game_id: it.g.game_id,
            category: it.g.category.clone(),
            rating: it.g.rating,
            ended_at: it.g.ended_at,
            n: it.g.n,
            n_complex: it.g.n_complex,
            values: it.g.values,
            zq: it.gz.zq.and_then(r2),
            ze: it.gz.ze.and_then(r2),
            zt: it.gz.zt.and_then(r2),
        })
        .collect();
    PlayerScore {
        level,
        score: r2(score).unwrap_or(0.0),
        high_window,
        trigger,
        groups: Groups { q: r2(q), e: r2(e), j: r2(jump.score), t: r2(t), accuracy_type: r2(accuracy_type) },
        window_all: WindowSummary::of(&all),
        window_recent: WindowSummary::of(&recent),
        reasons: explain(&items, &all, &recent, &jump, view),
        jump,
        per_game,
        games: all.games,
        moves: all.moves,
    }
}

// A weighted mean of the player's values and of what peers show (rating and time control).
struct WeightedMean {
    v: f64,
    expected: f64,
}

fn weighted_mean(
    items: &[Item<'_>],
    pop: View<'_>,
    metric: Metric,
    weight: fn(&SideRecord) -> f64,
    keep: fn(&SideRecord) -> bool,
) -> Option<WeightedMean> {
    let (mut s, mut w, mut se) = (0.0, 0.0, 0.0);
    for it in items {
        let g = it.g;
        let Some(x) = g.value(metric) else { continue };
        if !keep(g) {
            continue;
        }
        let wt = weight(g);
        if wt == 0.0 || wt.is_nan() {
            continue;
        }
        let bucket = bucket_of_rating(g.rating.or(1500.0));
        s += wt * x;
        se += wt * pop.effective(metric, &g.category, bucket, g.time_class()).mean;
        w += wt;
    }
    (w != 0.0).then(|| WeightedMean { v: s / w, expected: se / w })
}

// Human-readable reasons with the raw numbers (the player's weighted mean against what peers of
// the same rating and time control show).
fn explain(
    items: &[Item<'_>],
    all: &WindowScores,
    recent: &WindowScores,
    jump: &JumpScore,
    pop: View<'_>,
) -> Vec<String> {
    let mut out = Vec::new();
    if items.is_empty() {
        return out;
    }
    let fmt = |x: Option<f64>, d: usize| x.map_or_else(|| "-".to_string(), |x| to_fixed(x, d));
    let every = |_: &SideRecord| true;
    let wm = |m, weight, keep| weighted_mean(items, pop, m, weight, keep);
    let acc = wm(Metric::Accuracy, |g: &SideRecord| g.n, every);
    let acpl = wm(Metric::Acpl, |g: &SideRecord| g.n, every);
    let t1c = wm(
        Metric::T1Complex,
        |g: &SideRecord| g.n_complex,
        |g: &SideRecord| g.n_complex >= model::MIN_COMPLEX_PER_GAME,
    );
    let t1d = wm(Metric::T1Deep, |g: &SideRecord| g.n, every);
    let t1f = wm(Metric::T1Fast, |g: &SideRecord| g.n, every);
    let tc = wm(Metric::TimeCorr, |g: &SideRecord| g.n_timed, every);
    let cv = wm(Metric::TimeCv, |g: &SideRecord| g.n_timed, every);
    let v = |x: &Option<WeightedMean>| x.as_ref().map(|x| x.v);
    let ex = |x: &Option<WeightedMean>| x.as_ref().map(|x| x.expected);
    let pct = |x: Option<f64>| x.map(|x| x * 100.0);
    let pick = |r: f64, a: f64| if r > a { recent } else { all };

    let w = pick(recent.q.score, all.q.score);
    out.push(format!(
        "Move quality Q={} over {} games / {} scored moves (shrink {}): accuracy {} vs {} expected, ACPL {} vs {}.",
        fmt(Some(w.q.score), 2),
        w.games,
        js_number(w.moves),
        fmt(Some(w.q.shrink), 2),
        fmt(v(&acc), 1),
        fmt(ex(&acc), 1),
        fmt(v(&acpl), 1),
        fmt(ex(&acpl), 1)
    ));
    let w = pick(recent.e.score, all.e.score);
    out.push(format!(
        "Engine choice in complex positions E={} over {} positions: T1 {}% vs {}% expected (all positions: deep T1 {}%, shallow T1 {}%).",
        fmt(Some(w.e.score), 2),
        js_number(w.e.n),
        fmt(pct(v(&t1c)), 1),
        fmt(pct(ex(&t1c)), 1),
        fmt(pct(v(&t1d)), 1),
        fmt(pct(v(&t1f)), 1)
    ));
    let w = pick(recent.t.score, all.t.score);
    out.push(format!(
        "Timing T={} over {} timed moves: think time / complexity rank correlation {} vs {} expected, think-time CV {} vs {}.",
        fmt(Some(w.t.score), 2),
        js_number(w.t.n),
        fmt(v(&tc), 2),
        fmt(ex(&tc), 2),
        fmt(v(&cv), 2),
        fmt(ex(&cv), 2)
    ));
    if jump.recent_games != 0 && jump.earlier_games != 0 {
        out.push(format!(
            "Own history J={}: quality z {} in the last {} games vs {} in the {} before ({}).",
            fmt(Some(jump.score), 2),
            fmt(jump.recent_mean, 2),
            jump.recent_games,
            fmt(jump.earlier_mean, 2),
            jump.earlier_games,
            if jump.lasting { "lasting" } else { "not lasting" }
        ));
    }
    out
}

/// Result of [`update_population_from_game`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PopulationUpdate {
    /// Sides added to the population.
    pub added: usize,
    /// What the caller stores, in one write job.
    pub observations: Vec<Observation>,
}

/// Adds an analysed game (a features record, as stored) to the population statistics, both
/// sides, skipping players already flagged high_confidence or confirmed (`level_of`) and sides
/// with too few scored moves. A game analysed with another profile than the population's is
/// refused.
pub fn update_population_from_game(
    pop: &Population,
    src: &dyn PopulationSource,
    features: &Value,
    mut level_of: impl FnMut(UserId) -> IntegrityLevel,
) -> PopulationUpdate {
    let mut out = PopulationUpdate::default();
    let Some(f) = parse_maybe_json(features) else { return out };
    if !f.is_object() {
        return out;
    }
    let base = record_base(&f);
    if !pop.holds(base.profile.as_deref()) {
        return out;
    }
    for key in ["white", "black"] {
        let Some(side) = f.get(key).filter(|s| s.is_object()) else { continue };
        let s = side_fields(base.clone(), side);
        if s.n < model::POPULATION_MIN_MOVES || POPULATION_SKIP_LEVELS.contains(&level_of(s.user_id)) {
            continue;
        }
        let observations =
            pop.update(src, &s.category, bucket_of_rating(s.rating.or(1500.0)), &s, s.time_class());
        if !observations.is_empty() {
            out.added += 1;
            out.observations.extend(observations);
        }
    }
    out
}

#[cfg(test)]
mod tests;
