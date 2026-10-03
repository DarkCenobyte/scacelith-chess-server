//! Engine analysis of one finished game: per-move evaluations turned into per-player features.
//!
//! Positions analysed (the game always starts from the initial position):
//! * deep pass: every position from ply `2 * OPENING_PLIES_PER_SIDE` to the final one, depth
//!   `ANALYSIS_DEPTH_DEEP`, MultiPV 3, one `ucinewgame` for the game and the positions in game
//!   order (the hash carries over, as in any game analysis); a search that the node limit
//!   ([`DEEP_NODE_LIMIT`]) ends is repeated from an empty hash, without the limit;
//! * shallow pass: the positions actually scored, depth `ANALYSIS_DEPTH_FAST`, MultiPV 1, with the
//!   hash cleared before each search so the answer is really a shallow engine's choice.
//!
//! One thread, a fixed depth, a node limit and a controlled hash make the analysis reproducible:
//! running it again with the same engine version and network gives the same numbers. Every
//! record names its analysis profile ([`analysis_profile`]), and only records of the same profile
//! are ever compared or pooled.
//!
//! A move is scored unless it is among the first [`OPENING_PLIES_PER_SIDE`] moves of its side, the
//! position before it is already decided (|best eval| > [`DECIDED_CP`] from the mover's view) or it
//! is forced (MultiPV returns a single line). For each scored move: win-probability loss and move
//! accuracy (lichess' logistic model), centipawn loss capped at [`CP_LOSS_CAP`], deep-best match
//! (T1), shallow-best match, top-3 match, complexity (moves within [`GOOD_MARGIN_CP`] of the best
//! among the top 3, gap between the two best, whether the shallow and deep choices differ) and
//! the clock time the server charged.
//!
//! The analysis needs no chess rules: the engine checks legality and the moves are sent as UCI
//! text ([`super::moves::move_to_uci`]).

use std::collections::BTreeMap;
use std::future::Future;

use serde_json::{Map, Value, json};

use super::engine::{EngineError, PvLine, SearchOptions, SearchResult};
use super::moves::move_to_uci;
use super::stats::{coefficient_of_variation, harmonic_mean, mean, move_accuracy, spearman, win_percent};
use crate::anticheat::num::{js_round, json_num, json_opt, round_to};
use crate::clock::Clock;
use crate::ids::{GameId, UserId};

/// Version of these rules (part of the analysis profile). 2: the deep pass's node limit.
pub const ANALYSIS_VERSION: u32 = 2;
/// Moves of each side skipped as opening theory.
pub const OPENING_PLIES_PER_SIDE: usize = 8;
/// A position whose best evaluation is beyond this is decided: its move is not scored.
pub const DECIDED_CP: i64 = 600;
/// Moves within this of the best are good moves (complexity).
pub const GOOD_MARGIN_CP: i64 = 50;
/// Centipawn loss cap of one move.
pub const CP_LOSS_CAP: i64 = 1000;
/// Evaluations are clamped to this before computing a loss.
pub const EVAL_CLAMP_CP: i64 = 1000;
/// Value of a mate (mate in n: `MATE_CP - 10 n`).
pub const MATE_CP: i64 = 10000;
/// Time features need at least this many scored moves with a clock time.
pub const MIN_TIMED_MOVES: usize = 8;
/// Lines of the deep pass.
pub const MULTI_PV: u32 = 3;
/// Node limit of a deep search: a fixed-depth search can blow up on the hash the game's earlier
/// positions left (Stockfish 19 at depth 15 once spent over 200 million nodes on a position it
/// searches in half a million from an empty hash). A count of nodes, unlike a time, is the same on
/// every machine, so the analysis stays reproducible.
pub const DEEP_NODE_LIMIT: u64 = 25_000_000;

/// Bits of a scored move's flags ([`ScoredMove::flags`]).
pub mod flags {
    /// The deep search's best move.
    pub const T1: u8 = 1;
    /// The shallow search's best move.
    pub const FAST: u8 = 2;
    /// One of the deep search's top three lines.
    pub const TOP3: u8 = 4;
    /// At least two good moves.
    pub const COMPLEX: u8 = 8;
    /// The shallow and deep choices differ.
    pub const TRICKY: u8 = 16;
}

/// What the analysis needs from an engine (implemented by [`super::engine::UciEngine`]; tests use
/// scripted engines).
pub trait AnalysisEngine {
    /// The engine's name (learnt when it started).
    fn name(&self) -> &str;
    /// Its evaluation network(s), `+`-joined.
    fn net(&self) -> Option<String>;
    /// Its hash size (part of the profile).
    fn hash_mb(&self) -> Option<u32>;
    /// Starts a new game (clears the engine's game state).
    fn new_game(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send;
    /// Empties the transposition table.
    fn clear_hash(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send;
    /// Searches the position after `moves` (UCI, from the initial position).
    fn analyse(
        &mut self,
        moves: &[String],
        opts: &SearchOptions,
    ) -> impl Future<Output = Result<SearchResult, EngineError>> + Send;
}

/// Analysis profile of a record: what must be equal for two games' features to be comparable
/// (engine and version, network, depths, hash size, version of these rules), e.g.
/// `Stockfish 19; nn-1a298aa575a0.nnue; depth 9/15; hash 32; analysis 2`. It keys the population
/// statistics already stored: the format is frozen.
pub fn analysis_profile(
    engine: &str,
    net: Option<&str>,
    depth_fast: u32,
    depth_deep: u32,
    hash_mb: Option<u32>,
) -> String {
    let mut parts = vec![if engine.is_empty() { "engine".to_string() } else { engine.to_string() }];
    if let Some(net) = net.filter(|n| !n.is_empty()) {
        parts.push(net.to_string());
    }
    parts.push(format!("depth {depth_fast}/{depth_deep}"));
    if let Some(hash) = hash_mb.filter(|&h| h != 0) {
        parts.push(format!("hash {hash}"));
    }
    parts.push(format!("analysis {ANALYSIS_VERSION}"));
    // '|' separates the parts of the population statistics' keys.
    parts.join("; ").replace('|', "/")
}

/// Centipawn value of an engine line from the side to move's point of view (mate in n:
/// `+/-(MATE_CP - 10 n)`; mate 0 means the side to move is mated). A line without a score
/// counts 0.
pub fn line_cp(line: &PvLine) -> i64 {
    match line.mate {
        Some(0) => -MATE_CP,
        Some(m) if m > 0 => MATE_CP - 10 * m,
        Some(m) => -MATE_CP - 10 * m,
        None => line.cp.unwrap_or(0),
    }
}

fn clamp_eval(cp: i64) -> i64 {
    cp.clamp(-EVAL_CLAMP_CP, EVAL_CLAMP_CP)
}

/// The searches of one game, by position (the position after `p` plies).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Positions {
    pub deep: BTreeMap<usize, SearchResult>,
    pub fast: BTreeMap<usize, SearchResult>,
}

/// Runs the engine over a game (`uci`: its moves). Any engine error fails the whole game.
pub async fn analyse_positions<E: AnalysisEngine>(
    engine: &mut E,
    uci: &[String],
    depth_fast: u32,
    depth_deep: u32,
) -> Result<Positions, EngineError> {
    let from_ply = 2 * OPENING_PLIES_PER_SIDE;
    let mut pos = Positions::default();
    let n = uci.len();
    if n <= from_ply {
        return Ok(pos);
    }
    engine.new_game().await?;
    let limited =
        SearchOptions { depth: depth_deep, multi_pv: MULTI_PV, fen: None, nodes: Some(DEEP_NODE_LIMIT) };
    for p in from_ply..=n {
        let moves = &uci[..p];
        let mut d = engine.analyse(moves, &limited).await?;
        if d.node_limited {
            engine.clear_hash().await?;
            d = engine.analyse(moves, &SearchOptions { nodes: None, ..limited.clone() }).await?;
        }
        pos.deep.insert(p, d);
    }
    let shallow = SearchOptions::depth(depth_fast);
    for p in from_ply..n {
        let Some(d) = pos.deep.get(&p) else { continue };
        if d.lines.len() <= 1 || line_cp(&d.lines[0]).abs() > DECIDED_CP {
            continue; // forced (or no data), or decided
        }
        engine.clear_hash().await?;
        let f = engine.analyse(&uci[..p], &shallow).await?;
        pos.fast.insert(p, f);
    }
    Ok(pos)
}

/// Moves of a side left out of its features, by reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    pub opening: u32,
    pub forced: u32,
    pub decided: u32,
    pub missing: u32,
}

/// One scored move: `[ply, centipawn loss, flags, good moves, spent ms or -1]` in the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScoredMove {
    pub ply: u32,
    pub loss: i64,
    /// [`flags`] bits.
    pub flags: u8,
    pub n_good: u32,
    /// Clock time the server charged, -1 when unknown.
    pub spent_ms: i64,
}

/// Per-player features of one analysed game.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SideFeatures {
    pub user_id: UserId,
    /// Rating at the time of the game.
    pub rating: Option<i64>,
    /// Counted games behind the rating (0 while unrated); `None` when unknown (written `null`).
    pub rating_games: Option<i64>,
    /// Scored moves.
    pub n: u32,
    pub accuracy: Option<f64>,
    pub acpl: Option<f64>,
    /// Mean win-probability loss.
    pub wpl: Option<f64>,
    pub t1_deep: Option<f64>,
    pub t1_fast: Option<f64>,
    pub top3: Option<f64>,
    /// Complex positions among the scored moves.
    pub n_complex: u32,
    pub t1_complex: Option<f64>,
    pub time_corr: Option<f64>,
    pub time_cv: Option<f64>,
    /// Scored moves with a clock time.
    pub n_timed: u32,
    pub mean_spent_ms: Option<i64>,
    pub skipped: Skipped,
    pub moves: Vec<ScoredMove>,
}

impl SideFeatures {
    /// The side as stored, in the former key order.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("userId".into(), json!(self.user_id));
        m.insert("rating".into(), self.rating.map_or(Value::Null, Value::from));
        m.insert("ratingGames".into(), self.rating_games.map_or(Value::Null, Value::from));
        m.insert("n".into(), json!(self.n));
        m.insert("accuracy".into(), json_opt(self.accuracy));
        m.insert("acpl".into(), json_opt(self.acpl));
        m.insert("wpl".into(), json_opt(self.wpl));
        m.insert("t1Deep".into(), json_opt(self.t1_deep));
        m.insert("t1Fast".into(), json_opt(self.t1_fast));
        m.insert("top3".into(), json_opt(self.top3));
        m.insert("nComplex".into(), json!(self.n_complex));
        m.insert("t1Complex".into(), json_opt(self.t1_complex));
        m.insert("timeCorr".into(), json_opt(self.time_corr));
        m.insert("timeCv".into(), json_opt(self.time_cv));
        m.insert("nTimed".into(), json!(self.n_timed));
        m.insert("meanSpentMs".into(), self.mean_spent_ms.map_or(Value::Null, Value::from));
        let s = &self.skipped;
        m.insert(
            "skipped".into(),
            json!({ "opening": s.opening, "forced": s.forced, "decided": s.decided, "missing": s.missing }),
        );
        let moves: Vec<Value> =
            self.moves.iter().map(|mv| json!([mv.ply, mv.loss, mv.flags, mv.n_good, mv.spent_ms])).collect();
        m.insert("moves".into(), Value::Array(moves));
        Value::Object(m)
    }
}

/// Turns position analyses into per-player features (white, black), with the identity fields
/// left empty. Pure; see the module documentation for the rules.
pub fn compute_features(moves: &[u16], spent_ms: &[u32], pos: &Positions) -> [SideFeatures; 2] {
    let mut sides = [SideFeatures::default(), SideFeatures::default()];
    let mut acc: [Vec<f64>; 2] = Default::default();
    let mut cpl: [Vec<f64>; 2] = Default::default();
    let mut wpl: [Vec<f64>; 2] = Default::default();
    let (mut t1d, mut t1f, mut top3, mut cx, mut cx_hit) =
        ([0u32; 2], [0u32; 2], [0u32; 2], [0u32; 2], [0u32; 2]);
    let mut times: [Vec<f64>; 2] = Default::default();
    let mut cplx: [Vec<f64>; 2] = Default::default();
    for (p, &mv) in moves.iter().enumerate() {
        let s = p & 1;
        let side = &mut sides[s];
        if p < 2 * OPENING_PLIES_PER_SIDE {
            side.skipped.opening += 1;
            continue;
        }
        let Some(before) = pos.deep.get(&p).filter(|d| !d.lines.is_empty()) else {
            side.skipped.missing += 1;
            continue;
        };
        if before.lines.len() == 1 {
            side.skipped.forced += 1;
            continue;
        }
        let best_cp = line_cp(&before.lines[0]);
        if best_cp.abs() > DECIDED_CP {
            side.skipped.decided += 1;
            continue;
        }
        let played = move_to_uci(mv);
        let played_line = before.lines.iter().find(|l| l.mv.as_deref() == Some(played.as_str()));
        let played_cp = match played_line {
            Some(l) => Some(line_cp(l)),
            None => pos.deep.get(&(p + 1)).and_then(|after| after.lines.first()).map(|l| -line_cp(l)),
        };
        let (Some(played_cp), Some(fast)) = (played_cp, pos.fast.get(&p)) else {
            side.skipped.missing += 1;
            continue;
        };

        let loss = (clamp_eval(best_cp) - clamp_eval(played_cp)).clamp(0, CP_LOSS_CAP);
        let (w_before, w_after) = (win_percent(best_cp as f64), win_percent(played_cp as f64));
        let a = move_accuracy(w_before, w_after);
        let deep_best = before.lines[0].mv.as_deref().or(before.bestmove.as_deref());
        let is_t1 = Some(played.as_str()) == deep_best;
        let is_fast = Some(played.as_str()) == fast.bestmove.as_deref();
        let in_top3 = played_line.is_some();
        let n_good = before.lines.iter().filter(|l| best_cp - line_cp(l) <= GOOD_MARGIN_CP).count() as u32;
        let gap = CP_LOSS_CAP.min(best_cp - line_cp(&before.lines[1]));
        let complex = n_good >= 2;
        let tricky = fast.bestmove.as_deref() != deep_best;
        // Continuous complexity used for the time correlation: several good moves, a shallow
        // search that is misled, and a small gap between the two best moves all make a decision
        // harder for a human.
        let complexity =
            (f64::from(n_good) - 1.0) + if tricky { 1.0 } else { 0.0 } + (1.0 - gap.min(200) as f64 / 200.0);

        acc[s].push(a);
        cpl[s].push(loss as f64);
        wpl[s].push((w_before - w_after).max(0.0));
        t1d[s] += u32::from(is_t1);
        t1f[s] += u32::from(is_fast);
        top3[s] += u32::from(in_top3);
        if complex {
            cx[s] += 1;
            cx_hit[s] += u32::from(is_t1);
        }
        let spent = spent_ms.get(p).copied();
        if let Some(spent) = spent {
            times[s].push(f64::from(spent));
            cplx[s].push(complexity);
        }
        let mut bits = 0;
        for (on, bit) in [
            (is_t1, flags::T1),
            (is_fast, flags::FAST),
            (in_top3, flags::TOP3),
            (complex, flags::COMPLEX),
            (tricky, flags::TRICKY),
        ] {
            if on {
                bits |= bit;
            }
        }
        side.moves.push(ScoredMove {
            ply: p as u32,
            loss,
            flags: bits,
            n_good,
            spent_ms: spent.map_or(-1, i64::from),
        });
    }
    for (s, side) in sides.iter_mut().enumerate() {
        let n = acc[s].len();
        side.n = n as u32;
        if n == 0 {
            continue;
        }
        let nf = n as f64;
        side.accuracy = Some(round_to((mean(&acc[s]) + harmonic_mean(&acc[s], 1.0)) / 2.0, 2));
        side.acpl = Some(round_to(mean(&cpl[s]), 1));
        side.wpl = Some(round_to(mean(&wpl[s]), 2));
        side.t1_deep = Some(round_to(f64::from(t1d[s]) / nf, 4));
        side.t1_fast = Some(round_to(f64::from(t1f[s]) / nf, 4));
        side.top3 = Some(round_to(f64::from(top3[s]) / nf, 4));
        side.n_complex = cx[s];
        side.t1_complex = (cx[s] > 0).then(|| round_to(f64::from(cx_hit[s]) / f64::from(cx[s]), 4));
        side.n_timed = times[s].len() as u32;
        if times[s].len() >= MIN_TIMED_MOVES {
            side.time_corr = spearman(&times[s], &cplx[s]).map(|r| round_to(r, 4));
            side.time_cv = coefficient_of_variation(&times[s]).map(|cv| round_to(cv, 4));
        }
        side.mean_spent_ms = (!times[s].is_empty()).then(|| js_round(mean(&times[s])) as i64);
    }
    sides
}

/// The finished game to analyse.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameRecord {
    pub id: GameId,
    /// Category id (`custom` for a custom time control).
    pub category: String,
    pub rated: bool,
    pub base_ms: i64,
    pub inc_ms: i64,
    pub white_id: UserId,
    pub black_id: UserId,
    pub white_rating: Option<i64>,
    pub black_rating: Option<i64>,
    pub ended_at: Option<i64>,
    /// u16 protocol moves ([`super::moves::moves_from_le`] decodes the stored column).
    pub moves: Vec<u16>,
    /// Clock time charged for each move ([`super::moves::times_from_le`]).
    pub spent_ms: Vec<u32>,
}

/// Context of a player at analysis time, added to their features.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SideContext {
    /// Counted games behind the player's rating in the category (0 while unrated, `None` when
    /// unknown): it sets the width of the rating band of the scoring.
    pub rating_games: Option<i64>,
}

/// Depths of the two passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Depths {
    /// `ANALYSIS_DEPTH_FAST`
    pub fast: u32,
    /// `ANALYSIS_DEPTH_DEEP`
    pub deep: u32,
}

/// The features record of an analysed game, stored with the analysis job.
#[derive(Clone, Debug, PartialEq)]
pub struct GameFeatures {
    pub game_id: GameId,
    pub category: String,
    pub base_ms: i64,
    pub inc_ms: i64,
    pub plies: usize,
    pub ended_at: Option<i64>,
    pub engine: String,
    pub net: Option<String>,
    pub hash_mb: Option<u32>,
    pub depth_fast: u32,
    pub depth_deep: u32,
    pub profile: String,
    pub analysed_at: i64,
    pub duration_ms: i64,
    pub white: SideFeatures,
    pub black: SideFeatures,
}

impl GameFeatures {
    /// The record as stored, in the former key order (read back by
    /// [`crate::anticheat::scoring::side_of`]).
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("v".into(), json!(ANALYSIS_VERSION));
        m.insert("gameId".into(), json_num(self.game_id as f64));
        m.insert("category".into(), json!(self.category));
        m.insert("baseMs".into(), json!(self.base_ms));
        m.insert("incMs".into(), json!(self.inc_ms));
        m.insert("plies".into(), json!(self.plies));
        m.insert("endedAt".into(), self.ended_at.map_or(Value::Null, Value::from));
        m.insert("engine".into(), json!(self.engine));
        m.insert("net".into(), self.net.as_ref().map_or(Value::Null, |n| json!(n)));
        m.insert("hashMb".into(), self.hash_mb.map_or(Value::Null, Value::from));
        m.insert("depthFast".into(), json!(self.depth_fast));
        m.insert("depthDeep".into(), json!(self.depth_deep));
        m.insert("profile".into(), json!(self.profile));
        m.insert("analysedAt".into(), json!(self.analysed_at));
        m.insert("durationMs".into(), json!(self.duration_ms));
        m.insert("white".into(), self.white.to_json());
        m.insert("black".into(), self.black.to_json());
        Value::Object(m)
    }
}

/// Analyses a finished game and returns its features record. The engine's name, network and
/// hash (learnt when it started) make the profile.
pub async fn analyse_game<E: AnalysisEngine>(
    engine: &mut E,
    record: &GameRecord,
    depths: Depths,
    context: [SideContext; 2],
    clock: &dyn Clock,
) -> Result<GameFeatures, EngineError> {
    let uci: Vec<String> = record.moves.iter().map(|&m| move_to_uci(m)).collect();
    let started = clock.wall_ms();
    let pos = analyse_positions(engine, &uci, depths.fast, depths.deep).await?;
    let [mut white, mut black] = compute_features(&record.moves, &record.spent_ms, &pos);
    white.user_id = record.white_id;
    white.rating = record.white_rating;
    white.rating_games = context[0].rating_games;
    black.user_id = record.black_id;
    black.rating = record.black_rating;
    black.rating_games = context[1].rating_games;
    let name = if engine.name().is_empty() { "engine".to_string() } else { engine.name().to_string() };
    let (net, hash_mb) = (engine.net(), engine.hash_mb());
    let analysed_at = clock.wall_ms();
    Ok(GameFeatures {
        game_id: record.id,
        category: record.category.clone(),
        base_ms: record.base_ms,
        inc_ms: record.inc_ms,
        plies: record.moves.len(),
        ended_at: record.ended_at,
        profile: analysis_profile(&name, net.as_deref(), depths.fast, depths.deep, hash_mb),
        engine: name,
        net,
        hash_mb,
        depth_fast: depths.fast,
        depth_deep: depths.deep,
        analysed_at,
        duration_ms: analysed_at - started,
        white,
        black,
    })
}

#[cfg(test)]
mod tests;
