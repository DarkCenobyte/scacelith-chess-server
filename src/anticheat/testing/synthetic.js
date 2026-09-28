// Synthetic per-game features for tests and calibration of the scoring model (not used by the
// server). Honest players are drawn from the prior population (true per-game sd = prior sd /
// SD_INFLATION) with a persistent personal offset `theta` (in per-game sd units, between-player
// spread TAU); assisted players mix engine-like values into a fraction of their moves.

import { priorFor, SD_INFLATION, timeClassOfCategory } from '../priors.js';
import { clamp } from '../analysis/stats.js';

/** Deterministic PRNG (mulberry32). */
export function rng(seed) {
    let a = seed >>> 0;
    return () => {
        a = (a + 0x6D2B79F5) >>> 0;
        let t = a;
        t = Math.imul(t ^ (t >>> 15), t | 1);
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
}

/** Standard normal draw. */
export function gauss(r) {
    let u = 0, v = 0;
    while (u === 0) u = r();
    while (v === 0) v = r();
    return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * v);
}

/**
 * Engine-assisted per-game values (a relay of engine moves with random delays), as measured with
 * Stockfish 16 in test/unit/anticheat.engine.test.js (assisted depth 12, analysis 6/10).
 */
export const ENGINE_PROFILE = Object.freeze({
    accuracy: { mean: 98, sd: 1.5 }, acpl: { mean: 7, sd: 4 }, t1Deep: { mean: 0.7, sd: 0.1 }, t1Fast: { mean: 0.5, sd: 0.1 },
    t1Complex: { mean: 0.6, sd: 0.15 }, timeCorr: { mean: 0.0, sd: 0.2 }, timeCv: { mean: 0.26, sd: 0.08 },
});

const LIMITS = { accuracy: [0, 100], acpl: [0, 1000], t1Deep: [0, 1], t1Fast: [0, 1], t1Complex: [0, 1], timeCorr: [-1, 1], timeCv: [0, 5] };

/**
 * One side of an analysed game.
 * @param {object} o
 * @param {() => number} o.r        PRNG
 * @param {number} o.userId
 * @param {number} o.rating
 * @param {string} [o.category='5+0']
 * @param {number} [o.theta=0]       personal offset in per-game sd units (positive = stronger than the rating)
 * @param {number} [o.timeStyle=0]   personal timing offset in per-game sd units (positive = flatter, less correlated)
 * @param {number} [o.engine=0]      fraction of engine moves (0..1)
 * @param {boolean} [o.engineTiming] relay timing even when engine < 1 (default: engine > 0)
 * @param {number} [o.n=25]          scored moves
 * @param {number} [o.nComplex=8]
 * @param {number} [o.gameId]
 * @param {number} [o.endedAt]
 * @param {number} [o.ratingGames=100]
 */
export function syntheticSide({ r, userId, rating, category = '5+0', theta = 0, timeStyle = 0, engine = 0, engineTiming = engine > 0, n = 25, nComplex = 8, gameId = 0, endedAt = 0, ratingGames = 100, tau = 0.4 }) {
    const tc = timeClassOfCategory(category);
    const side = { userId, rating, ratingGames, n, nComplex, nTimed: n };
    const within = Math.sqrt(1 - tau * tau);
    const common = gauss(r);            // metrics of one game move together
    for (const m of Object.keys(LIMITS)) {
        const p = priorFor(m, rating, tc);
        const sd = p.sd / SD_INFLATION;
        const sign = m === 'acpl' || m === 'timeCorr' || m === 'timeCv' ? -1 : 1;
        const timing = m === 'timeCorr' || m === 'timeCv';
        const shared = timing ? gauss(r) : 0.6 * common + 0.8 * gauss(r);
        const human = p.mean + sd * sign * ((timing ? timeStyle : theta) + within * shared);
        const e = ENGINE_PROFILE[m];
        const eng = e.mean + e.sd * gauss(r);
        const frac = timing ? (engineTiming ? engine : 0) : engine;
        side[m] = clamp(frac * eng + (1 - frac) * human, LIMITS[m][0], LIMITS[m][1]);
    }
    return { gameId, category, baseMs: 0, incMs: 0, endedAt, analysedAt: endedAt, ...side };
}

/**
 * A player's history of `games` synthetic games, oldest first.
 * @param {object} o   syntheticSide options plus games, and engineFrom (index of the first assisted game)
 */
export function syntheticHistory({ games, engineFrom = Infinity, engine = 1, startAt = 1_800_000_000_000, ...o }) {
    const out = [];
    for (let i = 0; i < games; i++) {
        out.push(syntheticSide({ ...o, engine: i >= engineFrom ? engine : 0, gameId: (o.userId || 1) * 100000 + i, endedAt: startAt + i * 3600000 }));
    }
    return out;
}

/**
 * Feeds `count` honest synthetic games into a Population (as the server's own data would).
 * @param {import('../scoring.js').Population} pop
 * @param {{ r: () => number, count?: number, categories?: string[], minRating?: number, maxRating?: number }} o
 */
export function learnPopulation(pop, { r, count = 20000, categories = ['5+0'], minRating = 700, maxRating = 2600 }) {
    for (let i = 0; i < count; i++) {
        const rating = minRating + Math.floor(r() * (maxRating - minRating));
        const category = categories[i % categories.length];
        const s = syntheticSide({ r, userId: 1, rating, category, theta: 0.4 * gauss(r), timeStyle: 0.5 * gauss(r) });
        pop.update(category, Math.max(500, Math.min(2900, Math.floor(rating / 100) * 100)), s, timeClassOfCategory(category));
    }
    return pop;
}
