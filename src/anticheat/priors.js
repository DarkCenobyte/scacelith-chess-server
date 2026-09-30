// Prior population statistics of the per-game analysis metrics, used until the server has its
// own data (they are blended with it as PRIOR_GAMES pseudo-games, so real data replaces them
// progressively, bucket by bucket).
//
// Orders of magnitude taken from published human data: lichess' accuracy and average
// centipawn loss by rating (lichess blog / database studies), top-1 engine agreement of human
// players by rating in non-trivial positions (Regan & Haworth's intrinsic-ratings work, and
// Ferreira's / Guid & Bratko's engine-matching studies), and the general finding that humans
// spend more time on harder decisions. Our metrics exclude opening, forced and decided moves,
// which lowers accuracy and T1 compared with whole-game figures; the numbers below account for
// that roughly. Accuracy is derived from the ACPL row through the relation our own pipeline
// shows on engine-analysed games, so both quality metrics describe the same player: accuracy ~
// 100 - 0.275 ACPL up to an ACPL of about 90, flattening above (about 74 at 110, 70 at 150; the
// games of test/unit/anticheat.engine.test.js analysed by Stockfish 19 at 9/15, 9/16 and 10/18,
// 100 - 0.26 ACPL with Stockfish 16 at 10/18).
// They are deliberately conservative: standard deviations are inflated
// (SD_INFLATION) so that, before real data exists, z-scores are smaller than they should be.

import { clamp } from './analysis/stats.js';

const RATINGS = [600, 1000, 1500, 2000, 2500, 2900];

// [mean at each rating of RATINGS], [per-game standard deviation at each rating]
const TABLE = {
    accuracy:  { mean: [66, 74, 80, 86, 91, 93],               sd: [10, 9, 8, 6.5, 5, 4.5] },
    acpl:      { mean: [150, 110, 75, 50, 32, 24],             sd: [70, 55, 40, 28, 18, 14] },
    t1Deep:    { mean: [0.30, 0.35, 0.42, 0.49, 0.56, 0.60],   sd: [0.10, 0.10, 0.10, 0.10, 0.09, 0.09] },
    t1Fast:    { mean: [0.30, 0.35, 0.41, 0.47, 0.53, 0.56],   sd: [0.10, 0.10, 0.10, 0.10, 0.09, 0.09] },
    t1Complex: { mean: [0.28, 0.32, 0.37, 0.42, 0.47, 0.50],   sd: [0.17, 0.17, 0.17, 0.17, 0.16, 0.16] },
    timeCorr:  { mean: [0.15, 0.17, 0.20, 0.22, 0.24, 0.25],   sd: [0.25, 0.25, 0.25, 0.25, 0.25, 0.25] },
    timeCv:    { mean: [1.00, 1.00, 1.00, 1.00, 1.00, 1.00],   sd: [0.35, 0.35, 0.35, 0.35, 0.35, 0.35] },
};

// Faster games: humans are less accurate, agree less with the engine, and have less room to
// spend time where it matters.
const TIME_CLASS = {
    bullet:    { accuracy: -6, acplMul: 1.35, t1: -0.05, timeCorr: -0.07, timeCv: -0.15 },
    blitz:     { accuracy: -2, acplMul: 1.10, t1: -0.02, timeCorr: -0.02, timeCv: -0.05 },
    rapid:     { accuracy: 0,  acplMul: 1.00, t1: 0,     timeCorr: 0,     timeCv: 0 },
    classical: { accuracy: 2,  acplMul: 0.90, t1: 0.02,  timeCorr: 0.02,  timeCv: 0.05 },
};

/** Weight of the prior, in games, when blended with the server's own statistics. */
export const PRIOR_GAMES = 40;
/** Prior standard deviations are multiplied by this (conservative until real data exists). */
export const SD_INFLATION = 1.25;
/** Metric names of the model. */
export const PRIOR_METRICS = Object.freeze(Object.keys(TABLE));

/**
 * Time class of a time control, from the estimated game duration base + 40 increments
 * (lichess' convention): < 3 min bullet, < 10 blitz, < 30 rapid, else classical.
 * @param {number} baseMs
 * @param {number} incMs
 */
export function timeClass(baseMs, incMs) {
    const est = (baseMs || 0) / 1000 + 40 * (incMs || 0) / 1000;
    if (est < 180) return 'bullet';
    if (est < 600) return 'blitz';
    if (est < 1800) return 'rapid';
    return 'classical';
}

/** Time class of a category id such as '3+2' ('custom' counts as rapid). */
export function timeClassOfCategory(category) {
    const m = /^(\d+)\+(\d+)$/.exec(String(category));
    if (!m) return 'rapid';
    return timeClass(+m[1] * 60000, +m[2] * 1000);
}

function interp(arr, rating) {
    const r = clamp(rating, RATINGS[0], RATINGS[RATINGS.length - 1]);
    for (let i = 0; i < RATINGS.length - 1; i++) {
        if (r <= RATINGS[i + 1]) {
            const t = (r - RATINGS[i]) / (RATINGS[i + 1] - RATINGS[i]);
            return arr[i] + t * (arr[i + 1] - arr[i]);
        }
    }
    return arr[arr.length - 1];
}

/**
 * Prior { mean, sd } of a metric for players of `rating` in a time class.
 * @param {string} metric
 * @param {number} rating
 * @param {string} tc   'bullet' | 'blitz' | 'rapid' | 'classical'
 */
export function priorFor(metric, rating, tc = 'rapid') {
    const row = TABLE[metric];
    if (!row) throw new Error(`no prior for metric ${metric}`);
    const adj = TIME_CLASS[tc] || TIME_CLASS.rapid;
    let mean = interp(row.mean, rating);
    let sd = interp(row.sd, rating) * SD_INFLATION;
    switch (metric) {
        case 'accuracy': mean = clamp(mean + adj.accuracy, 0, 100); break;
        case 'acpl': mean *= adj.acplMul; sd *= adj.acplMul; break;
        case 't1Deep': case 't1Fast': case 't1Complex': mean = clamp(mean + adj.t1, 0, 1); break;
        case 'timeCorr': mean += adj.timeCorr; break;
        case 'timeCv': mean += adj.timeCv; break;
        default: break;
    }
    return { mean, sd };
}
