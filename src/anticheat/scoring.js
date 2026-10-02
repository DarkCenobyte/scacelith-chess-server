// Statistical assistance model: per-game engine features -> a player's integrity level.
//
// It only ever sets a level and writes the evidence (with the numbers that justify it) for a
// moderator. It never bans and never changes matchmaking. docs/ANTICHEAT.md explains the model;
// the summary:
//
// 0. Analysis profile. Features are only comparable between games analysed alike (engine,
//    network, depths, hash: analyzer.js, analysisProfile). A Population holds the games of one
//    profile, and a player is scored on their games of that profile only: changing the engine
//    or the depths restarts the statistics from the priors. Until a player has enough games of
//    the new profile to be judged (suspected.minGames), the level reached before stays.
// 1. Population. For every (category, 100-point rating bucket) the server keeps Welford
//    statistics (n, mean, M2) of each per-game metric, blended with hard-coded priors
//    (priors.js) worth PRIOR_GAMES games, so the model works from the first day and is replaced
//    progressively by the server's own data. Games of players already flagged high_confidence or
//    confirmed are left out of the population (no contamination), values are winsorised.
// 2. Per game, each metric becomes an oriented z-score (positive = more engine-like) against
//    the population of the same category at the player's rating. The rating is uncertain
//    (provisional players, smurfs, fast improvers), so the z-score is the SMALLEST one over the
//    rating band (+/-100, +/-400 while provisional): the benefit of the doubt.
// 3. Metrics are grouped into signals of different nature:
//      Q  move quality      mean of z(accuracy) and z(-ACPL)               (accuracy-type)
//      E  engine choice     z(T1 match in complex positions)               (accuracy-type)
//      J  sudden jump       recent quality vs the player's own history     (accuracy-type)
//      T  timing            mean of z(-time/complexity correlation), z(-CV of think times)
//    Over a window, a group's score is the moves-weighted mean z, shrunk with the empirical-Bayes
//    factor n/(n+k) (n = scored moves), and expressed in units of the between-player spread tau
//    (how far the player's estimated long-run level is from peers of the same rating, in units of
//    how much honest players differ from each other). Windows: last 30 analysed games and last 10.
// 4. Levels:
//      suspected        >= 5 games and an accuracy-type score >= 3.5 (one strong signal), or a
//                       jump J >= 2.5 together with a recent-window Q or E >= 2.5 (the same recent
//                       games are both far above peers and far above the player's own past);
//      high_confidence  >= 10 games, >= 300 scored moves, accuracy-type >= 3.0 AND timing >= 1.5
//                       AND their combination (A+T)/sqrt(2) >= 3.5: two independent kinds of
//                       evidence must agree (Q, E and J are not independent of each other);
//      confirmed        never set here (moderators, certain protocol cheats).
//    Timing alone never flags anyone: lag, premoves and personal style move it too much.
//    updatePlayerIntegrity adds memory: suspected stays until the score drops 0.5 below the
//    threshold, high_confidence never falls below suspected without a moderator, and a player a
//    moderator cleared is only flagged again on new evidence.
// Calibration (synthetic populations, src/anticheat/testing/synthetic.js): no honest player
// flagged out of 3000 x 4 sample sizes; full engine users flagged after a median of 10-19 games
// once the server has its own statistics (fewer on a fresh server, whose priors are inflated).

import { priorFor, timeClass, timeClassOfCategory, PRIOR_GAMES, PRIOR_METRICS } from './priors.js';
import { welfordAdd, clamp, mean } from './analysis/stats.js';
import { parseMaybeJson, levelRank, readIntegrity, writeIntegrity } from './util.js';

/** 100-point rating bucket (lower bound), clamped to 500..2900. */
export function bucketOfRating(rating) {
    const r = Number.isFinite(+rating) ? +rating : 1500;
    return clamp(Math.floor(r / 100) * 100, 500, 2900);
}

/** Model parameters (documented in docs/ANTICHEAT.md). */
export const MODEL = Object.freeze({
    version: 1,
    windowGames: 30,
    recentGames: 10,
    minMovesPerGame: 8,             // games with fewer scored moves do not count
    minComplexPerGame: 3,           // E uses games with at least this many complex positions
    shrinkMoves: 150,               // k of n/(n+k) for Q and T (n = scored moves)
    shrinkComplex: 50,              // k for E (n = complex positions)
    tau: Object.freeze({ Q: 0.4, E: 0.45, T: 0.5 }),
    zClamp: 6,                      // a single game cannot weigh more than 6 sd
    // jumpWithRecent: a jump J and the recent-window quality (population-relative) both at
    // least this high also make a player suspected (two views of the same recent games).
    suspected: Object.freeze({ minGames: 5, accuracyType: 3.5, jumpWithRecent: 2.5, hysteresis: 0.5 }),
    high: Object.freeze({ minGames: 10, minMoves: 300, accuracyType: 3.0, timing: 1.5, combined: 3.5 }),
    // J tests "the recent games beat the player's own history by MORE than honestImprovement
    // per-game sd" (a plausible honest improvement is not evidence), and needs the jump to last
    // (lastingFraction of the recent games above the old level) and to land above peers.
    jump: Object.freeze({ minRecent: 5, minEarlier: 8, honestImprovement: 0.75, minRecentLevel: 1.0, lastingFraction: 0.7, sdFloor: 0.6 }),
    ratingBand: Object.freeze({ established: 100, provisional: 400 }),
    provisionalGames: 30,
    population: Object.freeze({ minMoves: 10, winsorZ: 4, skipLevels: Object.freeze(['high_confidence', 'confirmed']) }),
    sdFloor: Object.freeze({ accuracy: 3, acpl: 5, t1Deep: 0.04, t1Fast: 0.04, t1Complex: 0.08, timeCorr: 0.12, timeCv: 0.12 }),
});

/** Orientation of each metric: +1 when a higher value looks more like an engine. */
export const METRIC_SIGN = Object.freeze({ accuracy: 1, acpl: -1, t1Deep: 1, t1Fast: 1, t1Complex: 1, timeCorr: -1, timeCv: -1 });

// ---- population -------------------------------------------------------------------------------

function normaliseStats(raw) {
    const v = parseMaybeJson(raw, null);
    const out = {};
    if (!v) return out;
    if (Array.isArray(v)) {
        for (const r of v) if (r && r.metric) out[r.metric] = { n: +r.n || 0, mean: +r.mean || 0, m2: +r.m2 || 0 };
        return out;
    }
    const src = v.metrics && typeof v.metrics === 'object' ? v.metrics : v;
    for (const m of PRIOR_METRICS) {
        const s = src[m];
        if (s && Number.isFinite(+s.n)) out[m] = { n: +s.n, mean: +s.mean || 0, m2: Number.isFinite(+s.m2) ? +s.m2 : (+s.variance || 0) * Math.max(0, +s.n - 1) };
    }
    return out;
}

/**
 * Population statistics of one analysis profile per (category, rating bucket), blended with the
 * priors.
 *
 * Store contract used: populationStats('<profile>|<category>|<bucket>') returns { <metric>: { n,
 * mean, m2 } } (arrays of { metric, n, mean, m2 } rows and JSON text are accepted too), and
 * updatePopulation([{ key: '<profile>|<category>|<bucket>|<metric>', value }], now) merges one
 * observation per metric into the stored running statistics (Welford, one transaction). Without
 * a profile (tests, tools) the keys start at the category.
 */
export class Population {
    /**
     * @param {object|null} store   needs store.integrity.populationStats / updatePopulation (null: priors only)
     * @param {{ profile?: string|null, reloadMs?: number, now?: () => number }} [o]
     *        profile: the analysis profile (features.profile) of the games it holds
     */
    constructor(store, { profile = null, reloadMs = 600000, now = Date.now } = {}) {
        this.store = store;
        this.profile = profile || null;
        this.reloadMs = reloadMs;
        this.now = now;
        this.cache = new Map();   // key -> { at, stats }
    }

    key(category, bucket) { return this.profile ? `${this.profile}|${category}|${bucket}` : `${category}|${bucket}`; }

    /** Whether a features record (or a sideOf record) was analysed with this population's profile. */
    holds(features) { return (features?.profile || null) === this.profile; }

    /** Raw server statistics of a bucket: { metric: { n, mean, m2 } }. */
    raw(category, bucket) {
        const key = this.key(category, bucket);
        const c = this.cache.get(key);
        if (c && this.now() - c.at < this.reloadMs) return c.stats;
        let stats = {};
        if (this.store?.integrity?.populationStats) {
            try { stats = normaliseStats(this.store.integrity.populationStats(key)); } catch { stats = {}; }
        }
        this.cache.set(key, { at: this.now(), stats });
        return stats;
    }

    /**
     * Effective { mean, sd, n } of a metric: the prior as PRIOR_GAMES pseudo-games pooled with the
     * server's data (pooled variance, floored).
     */
    effective(metric, category, bucket, tc = timeClassOfCategory(category)) {
        const p = priorFor(metric, bucket + 50, tc);
        const d = this.raw(category, bucket)[metric];
        const n0 = PRIOR_GAMES;
        let meanV = p.mean, varV = p.sd * p.sd, n = 0;
        if (d && d.n > 0) {
            n = d.n;
            const tot = n0 + d.n;
            meanV = (n0 * p.mean + d.n * d.mean) / tot;
            varV = (n0 * (p.sd * p.sd + (p.mean - meanV) ** 2) + d.m2 + d.n * (d.mean - meanV) ** 2) / tot;
        }
        return { mean: meanV, sd: Math.max(Math.sqrt(Math.max(0, varV)), MODEL.sdFloor[metric] || 0), n };
    }

    /**
     * Adds one player's per-game features to the bucket (winsorised at +/- winsorZ sd of the
     * current effective distribution) and writes it back.
     */
    update(category, bucket, side, tc) {
        const key = this.key(category, bucket);
        const stats = { ...this.raw(category, bucket) };
        const observations = [];
        for (const m of PRIOR_METRICS) {
            const x = side[m];
            if (x === null || x === undefined || !Number.isFinite(+x)) continue;
            if (m === 't1Complex' && (side.nComplex || 0) < MODEL.minComplexPerGame) continue;
            const eff = this.effective(m, category, bucket, tc);
            const w = MODEL.population.winsorZ * eff.sd;
            const value = clamp(+x, eff.mean - w, eff.mean + w);
            stats[m] = welfordAdd(stats[m], value);
            observations.push({ key: `${key}|${m}`, value });
        }
        if (!observations.length) return false;
        // The store merges the observations into what it holds (it is the source of truth when
        // the cache is reloaded or the process restarts); the cache only takes those it accepted.
        this.store?.integrity?.updatePopulation?.(observations, this.now());
        this.cache.set(key, { at: this.now(), stats });
        return true;
    }
}

// ---- per game ----------------------------------------------------------------------------------

/**
 * One player's side of an analysed game, normalised for scoring.
 * @param {object} features   record from analyzer.analyseGame (or read back from the store)
 * @param {number} userId
 * @returns {null | object}
 */
export function sideOf(features, userId) {
    const f = parseMaybeJson(features, null);
    if (!f || !f.white || !f.black) return null;
    const side = Number(f.white.userId) === Number(userId) ? f.white : Number(f.black.userId) === Number(userId) ? f.black : null;
    if (!side) return null;
    return {
        gameId: f.gameId, category: f.category || 'custom', baseMs: f.baseMs || 0, incMs: f.incMs || 0,
        endedAt: f.endedAt || f.analysedAt || 0, analysedAt: f.analysedAt || 0, profile: f.profile || null,
        ...side,
    };
}

function orientedZ(metric, x, g, pop) {
    const tc = g.baseMs ? timeClass(g.baseMs, g.incMs) : timeClassOfCategory(g.category);
    const rating = Number.isFinite(+g.rating) ? +g.rating : 1500;
    const provisional = Number.isFinite(+g.ratingGames) && +g.ratingGames < MODEL.provisionalGames;
    const band = provisional ? MODEL.ratingBand.provisional : MODEL.ratingBand.established;
    let best = Infinity;
    for (let b = bucketOfRating(rating - band); b <= bucketOfRating(rating + band); b += 100) {
        const st = pop.effective(metric, g.category, b, tc);
        const z = METRIC_SIGN[metric] * (x - st.mean) / st.sd;
        if (z < best) best = z;
    }
    return clamp(best, -MODEL.zClamp, MODEL.zClamp);
}

function present(v) { return v !== null && v !== undefined && Number.isFinite(+v); }

/**
 * Per-game group z-scores of one side.
 * @returns {{ zQ: number|null, zE: number|null, zT: number|null, n: number, nE: number, nT: number, z: object }}
 */
export function gameZ(g, pop) {
    const z = {};
    for (const m of ['accuracy', 'acpl', 't1Deep', 't1Fast', 'timeCorr', 'timeCv']) if (present(g[m])) z[m] = orientedZ(m, +g[m], g, pop);
    const nE = (g.nComplex || 0) >= MODEL.minComplexPerGame && present(g.t1Complex) ? g.nComplex : 0;
    if (nE) z.t1Complex = orientedZ('t1Complex', +g.t1Complex, g, pop);
    const q = [z.accuracy, z.acpl].filter((v) => v !== undefined);
    const t = [z.timeCorr, z.timeCv].filter((v) => v !== undefined);
    return {
        zQ: q.length ? mean(q) : null,
        zE: nE ? z.t1Complex : null,
        zT: present(g.timeCorr) && t.length ? mean(t) : null,
        n: g.n || 0, nE, nT: present(g.timeCorr) ? (g.nTimed || g.n || 0) : 0,
        z,
    };
}

// ---- per player --------------------------------------------------------------------------------

function groupScore(items, key, weightKey, k, tau) {
    let sw = 0, s = 0;
    for (const it of items) {
        const v = it.gz[key], w = it.gz[weightKey];
        if (v === null || !w) continue;
        sw += w; s += w * v;
    }
    if (!sw) return { score: 0, meanZ: 0, n: 0, shrink: 0 };
    const meanZ = s / sw;
    const shrink = sw / (sw + k);
    return { score: shrink * meanZ / tau, meanZ, n: sw, shrink };
}

function windowScores(items) {
    const Q = groupScore(items, 'zQ', 'n', MODEL.shrinkMoves, MODEL.tau.Q);
    const E = groupScore(items, 'zE', 'nE', MODEL.shrinkComplex, MODEL.tau.E);
    const T = groupScore(items, 'zT', 'nT', MODEL.shrinkMoves, MODEL.tau.T);
    const moves = items.reduce((a, it) => a + it.g.n, 0);
    return { games: items.length, moves, Q, E, T };
}

// Sudden lasting jump: quality of the recent games against the player's own earlier games.
function jumpScore(chrono) {
    const J = MODEL.jump;
    const withQ = chrono.filter((it) => it.gz.zQ !== null);
    const recent = withQ.slice(-MODEL.recentGames);
    const earlier = withQ.slice(0, Math.max(0, withQ.length - MODEL.recentGames));
    const res = { score: 0, effect: 0, recentMean: null, earlierMean: null, lasting: false, recentGames: recent.length, earlierGames: earlier.length };
    if (recent.length < J.minRecent || earlier.length < J.minEarlier) return res;
    const r = recent.map((it) => it.gz.zQ), e = earlier.map((it) => it.gz.zQ);
    const mr = mean(r), me = mean(e);
    const all = [...r.map((x) => x - mr), ...e.map((x) => x - me)];
    const pooledSd = Math.max(J.sdFloor, Math.sqrt(all.reduce((a, x) => a + x * x, 0) / Math.max(1, all.length - 2)));
    const se = pooledSd * Math.sqrt(1 / r.length + 1 / e.length);
    const t = (mr - me - J.honestImprovement) / se;
    const above = r.filter((x) => x > me + 0.5 * pooledSd).length / r.length;
    res.effect = mr - me;
    res.recentMean = mr;
    res.earlierMean = me;
    res.lasting = above >= J.lastingFraction;
    res.fractionAbove = above;
    res.t = t;
    // Only a jump UP to a level above peers counts (coming back from a bad streak is not suspicious).
    if (res.lasting && mr >= J.minRecentLevel && t > 0) res.score = t;
    return res;
}

const r2 = (x) => (x === null || x === undefined || !Number.isFinite(x) ? null : Math.round(x * 100) / 100);

/**
 * Scores a player from their analysed games of the population's analysis profile (the others
 * are left out).
 * @param {object[]} games   sideOf() records, any order (sorted newest first here)
 * @param {Population} pop
 * @returns {{ level: string, score: number, groups: object, windows: object, jump: object, reasons: string[], perGame: object[], games: number, moves: number }}
 */
export function scorePlayer(games, pop) {
    const usable = games
        .filter((g) => g && (g.n || 0) >= MODEL.minMovesPerGame && pop.holds(g))
        .sort((a, b) => (b.endedAt || 0) - (a.endedAt || 0) || (b.analysedAt || 0) - (a.analysedAt || 0))
        .slice(0, MODEL.windowGames);
    const items = usable.map((g) => ({ g, gz: gameZ(g, pop) }));
    const all = windowScores(items);
    const recent = windowScores(items.slice(0, MODEL.recentGames));
    const jump = jumpScore([...items].reverse());

    const best = (k) => Math.max(all[k].score, recent[k].score);
    const groups = { Q: best('Q'), E: best('E'), J: jump.score, T: best('T') };
    const accuracyType = Math.max(groups.Q, groups.E, groups.J);

    let level = 'none', trigger = null;
    const S = MODEL.suspected, H = MODEL.high;
    if (all.games >= S.minGames && accuracyType >= S.accuracyType) {
        level = 'suspected';
        trigger = `one accuracy-type signal >= ${S.accuracyType} over >= ${S.minGames} games`;
    }
    const recentA = Math.max(recent.Q.score, recent.E.score);
    if (level === 'none' && all.games >= S.minGames && jump.score >= S.jumpWithRecent && recentA >= S.jumpWithRecent) {
        level = 'suspected';
        trigger = `sudden lasting jump (J >= ${S.jumpWithRecent}) with recent games far above peers (>= ${S.jumpWithRecent})`;
    }
    let highWindow = null;
    for (const [name, w] of [['all', all], ['recent', recent]]) {
        if (w.games < H.minGames || w.moves < H.minMoves) continue;
        const a = Math.max(w.Q.score, w.E.score, jump.score);
        const t = w.T.score;
        if (a >= H.accuracyType && t >= H.timing && (a + t) / Math.SQRT2 >= H.combined) {
            level = 'high_confidence';
            highWindow = name;
            trigger = `accuracy-type ${a.toFixed(2)} and timing ${t.toFixed(2)} agree over the ${name === 'all' ? `last ${w.games}` : 'recent'} games (${w.moves} moves)`;
            break;
        }
    }
    const score = Math.max(accuracyType, (accuracyType + Math.max(0, groups.T)) / Math.SQRT2);

    const perGame = items.map(({ g, gz }) => ({
        gameId: g.gameId, category: g.category, rating: g.rating, endedAt: g.endedAt, n: g.n,
        accuracy: g.accuracy, acpl: g.acpl, t1Deep: g.t1Deep, t1Fast: g.t1Fast, t1Complex: g.t1Complex, nComplex: g.nComplex,
        timeCorr: g.timeCorr, timeCv: g.timeCv, zQ: r2(gz.zQ), zE: r2(gz.zE), zT: r2(gz.zT),
    }));
    return {
        level, score: r2(score) ?? 0, highWindow, trigger,
        groups: { Q: r2(groups.Q), E: r2(groups.E), J: r2(groups.J), T: r2(groups.T), accuracyType: r2(accuracyType) },
        windows: { all: summariseWindow(all), recent: summariseWindow(recent) },
        jump: { score: r2(jump.score), effect: r2(jump.effect), recentMean: r2(jump.recentMean), earlierMean: r2(jump.earlierMean), lasting: jump.lasting, fractionAbove: r2(jump.fractionAbove), recentGames: jump.recentGames, earlierGames: jump.earlierGames },
        reasons: explain(items, all, recent, jump, pop),
        perGame, games: all.games, moves: all.moves,
    };
}

function summariseWindow(w) {
    const g = (x) => ({ score: r2(x.score), meanZ: r2(x.meanZ), n: x.n, shrink: r2(x.shrink) });
    return { games: w.games, moves: w.moves, Q: g(w.Q), E: g(w.E), T: g(w.T) };
}

// Human-readable reasons with the raw numbers (player's weighted mean vs what peers of the same
// rating and time control show).
function explain(items, all, recent, jump, pop) {
    const out = [];
    if (!items.length) return out;
    const wmean = (key, wkey = 'n', filter = () => true) => {
        let s = 0, w = 0, se = 0;
        for (const { g } of items) {
            if (!present(g[key]) || !filter(g)) continue;
            const wt = wkey === 'n' ? g.n : g[wkey] || 0;
            if (!wt) continue;
            const tc = g.baseMs ? timeClass(g.baseMs, g.incMs) : timeClassOfCategory(g.category);
            s += wt * g[key];
            se += wt * pop.effective(key, g.category, bucketOfRating(g.rating ?? 1500), tc).mean;
            w += wt;
        }
        return w ? { v: s / w, expected: se / w } : null;
    };
    const fmt = (x, d = 1) => (x === null || x === undefined ? '-' : Number(x).toFixed(d));
    const acc = wmean('accuracy'), acpl = wmean('acpl'), t1c = wmean('t1Complex', 'nComplex', (g) => g.nComplex >= MODEL.minComplexPerGame);
    const t1d = wmean('t1Deep'), t1f = wmean('t1Fast');
    const tc = wmean('timeCorr', 'nTimed'), cv = wmean('timeCv', 'nTimed');
    const pick = (k) => (recent[k].score > all[k].score ? recent : all);
    {
        const w = pick('Q');
        out.push(`Move quality Q=${fmt(w.Q.score, 2)} over ${w.games} games / ${w.moves} scored moves (shrink ${fmt(w.Q.shrink, 2)}): accuracy ${fmt(acc?.v)} vs ${fmt(acc?.expected)} expected, ACPL ${fmt(acpl?.v)} vs ${fmt(acpl?.expected)}.`);
    }
    {
        const w = pick('E');
        out.push(`Engine choice in complex positions E=${fmt(w.E.score, 2)} over ${w.E.n} positions: T1 ${fmt(t1c ? t1c.v * 100 : null)}% vs ${fmt(t1c ? t1c.expected * 100 : null)}% expected (all positions: deep T1 ${fmt(t1d ? t1d.v * 100 : null)}%, shallow T1 ${fmt(t1f ? t1f.v * 100 : null)}%).`);
    }
    {
        const w = pick('T');
        out.push(`Timing T=${fmt(w.T.score, 2)} over ${w.T.n} timed moves: think time / complexity rank correlation ${fmt(tc?.v, 2)} vs ${fmt(tc?.expected, 2)} expected, think-time CV ${fmt(cv?.v, 2)} vs ${fmt(cv?.expected, 2)}.`);
    }
    if (jump.recentGames && jump.earlierGames) {
        out.push(`Own history J=${fmt(jump.score, 2)}: quality z ${fmt(jump.recentMean, 2)} in the last ${jump.recentGames} games vs ${fmt(jump.earlierMean, 2)} in the ${jump.earlierGames} before (${jump.lasting ? 'lasting' : 'not lasting'}).`);
    }
    return out;
}

// ---- integration with the store ------------------------------------------------------------------

// Below this score and without a level, only a compact summary is stored.
const NOTABLE_SCORE = 2;

/**
 * Reads a player's analysed games (store.analysis.forUser) as sideOf() records.
 * @param {object} store
 * @param {number} userId
 * @param {number} [limit]
 */
export function playerGames(store, userId, limit = MODEL.windowGames) {
    let rows = [];
    try { rows = store.analysis.forUser(userId, limit) || []; } catch { rows = []; }
    const out = [];
    for (const row of rows) {
        const f = parseMaybeJson(row?.features ?? row, null);
        const s = f && sideOf(f, userId);
        if (s) out.push(s);
    }
    return out;
}

/**
 * Recomputes a player's automatic integrity level after a new analysed game and stores it.
 *
 * Rules on top of scorePlayer: `confirmed` is never touched; a player whose recent games were
 * analysed with another profile keeps their level until they have MODEL.suspected.minGames games
 * of the population's profile (the statistics restarted: too few games to judge);
 * `high_confidence` does not fall back below `suspected` without a moderator; after a moderator
 * cleared the player the level only rises again on new evidence (high_confidence, or a score 1.0
 * above the cleared one with at least 5 games analysed since).
 * @param {{ store: object, userId: number, population: Population, now?: number, log?: object }} o
 *        population: the one of the analysis profile of the game just analysed
 * @returns {{ level: string, previous: string, score: number, result: object }}
 */
export function updatePlayerIntegrity({ store, userId, population, now = Date.now(), log = null }) {
    const prev = readIntegrity(store, userId);
    const games = playerGames(store, userId, MODEL.windowGames);
    const result = scorePlayer(games, population);
    const restarted = games.some((g) => !population.holds(g));
    let level = result.level;
    const ev = { ...prev.evidence };
    const review = ev.review || null;
    if (prev.level === 'confirmed') level = 'confirmed';
    else {
        // Statistics restarted by a new profile: too few of its games to judge yet.
        if (restarted && result.games < MODEL.suspected.minGames && levelRank(level) < levelRank(prev.level)) level = prev.level;
        if (prev.level === 'high_confidence' && levelRank(level) < levelRank('suspected')) level = 'suspected';
        // Hysteresis: a suspected player stays suspected until the evidence clearly recedes
        // (no flapping of the moderators' queue around the threshold).
        if (prev.level === 'suspected' && level === 'none' && result.groups.accuracyType >= MODEL.suspected.accuracyType - MODEL.suspected.hysteresis) level = 'suspected';
        if (review?.clearedAt && prev.level === 'none' && level === 'suspected') {
            const since = games.filter((g) => population.holds(g) && (g.analysedAt || g.endedAt || 0) > review.clearedAt).length;
            if (!(result.score >= (review.clearedScore || 0) + 1.0 && since >= 5)) level = 'none';
        }
    }
    // Every analysed player gets a row, so keep it small: the per-game numbers stay in the
    // analysis rows (bin/admin.js integrity show reads them there); the full explanation is only
    // kept when there is something to explain.
    const notable = level !== 'none' || result.score >= NOTABLE_SCORE;
    const profile = population.profile;
    ev.statistics = notable
        ? { model: MODEL.version, profile, computedAt: now, level: result.level, score: result.score, trigger: result.trigger, groups: result.groups,
            windows: result.windows, jump: result.jump, reasons: result.reasons, highWindow: result.highWindow, games: result.games, moves: result.moves }
        : { model: MODEL.version, profile, computedAt: now, level: result.level, score: result.score, groups: result.groups, games: result.games, moves: result.moves };
    if (notable && (!ev.peak || result.score > (ev.peak.score || 0))) ev.peak = { at: now, score: result.score, level: result.level, trigger: result.trigger, groups: result.groups, reasons: result.reasons };
    writeIntegrity(store, userId, { level, score: Math.max(result.score, 0), evidence: ev, updatedAt: now });
    if (level !== prev.level) log?.security?.('integrity.level', { userId, from: prev.level, to: level, score: result.score, groups: result.groups });
    return { level, previous: prev.level, score: result.score, result };
}

/**
 * Adds an analysed game to the population statistics (both sides), skipping players already
 * flagged high_confidence / confirmed and sides with too few scored moves. A game analysed with
 * another profile than the population's is refused.
 * @param {Population} population
 * @param {object} features
 * @param {(userId: number) => string} levelOf
 */
export function updatePopulationFromGame(population, features, levelOf = () => 'none') {
    const f = parseMaybeJson(features, null);
    if (!f || !population.holds(f)) return 0;
    let added = 0;
    for (const side of [f.white, f.black]) {
        if (!side || (side.n || 0) < MODEL.population.minMoves) continue;
        if (MODEL.population.skipLevels.includes(levelOf(side.userId))) continue;
        const tc = f.baseMs ? timeClass(f.baseMs, f.incMs) : timeClassOfCategory(f.category);
        if (population.update(f.category || 'custom', bucketOfRating(side.rating ?? 1500), side, tc)) added++;
    }
    return added;
}
