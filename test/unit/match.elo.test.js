import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { testConfig } from '../../src/config.js';
import {
    expectedScore, kFactor, applyGame, defaultRecord, normalizeRecord, isProvisional, isUnrated, ratingDelta, initialRating,
    scoringProbability, ratingDifference, FIDE_PD_TABLE, FIDE_DP_TABLE,
    categoryOf, parseCategory, isOfficialCategory, SENIOR_RATING, RATING_FLOOR, UNRATED_GAMES, MAX_INITIAL_RATING,
} from '../../src/match/elo.js';
import { VECTORS_PATH, renderVectors, cppRecord } from '../../tools/gen-elo-vectors.js';

const cfg = testConfig();

// Standard normal CDF: erf by its Taylor series (|x| < 3) or its continued fraction (double precision).
function phi(z) {
    const x = Math.abs(z) / Math.SQRT2;
    let erf;
    if (x < 3) {
        let t = x, sum = x;
        for (let n = 1; n < 200; n++) {
            t *= -x * x / n;
            const add = t / (2 * n + 1);
            sum += add;
            if (Math.abs(add) < 1e-18) break;
        }
        erf = 2 / Math.sqrt(Math.PI) * sum;
    } else {
        let f = 0;
        for (let k = 60; k >= 1; k--) f = k / 2 / (x + f);
        erf = 1 - Math.exp(-x * x) / Math.sqrt(Math.PI) / (x + f);
    }
    return z < 0 ? 0.5 * (1 - erf) : 0.5 * (1 + erf);
}

function rated(rating, games, peak = rating) {
    return { rating, games, wins: 0, draws: 0, losses: 0, peak: Math.max(rating, peak), reachedSenior: Math.max(rating, peak) >= SENIOR_RATING, rated: true,
        countedGames: games };
}

// A fresh record after the given scores against `opponent` (a record), and the changes seen.
function play(scores, opponent, start = defaultRecord(cfg)) {
    let r = start;
    const changes = [];
    for (const s of scores) {
        const res = applyGame(r, opponent, s, cfg);
        changes.push({ before: res.white.before, after: res.white.after, k: res.white.k, opponentAfter: res.black.after });
        r = res.white.record;
    }
    return { record: r, changes };
}

test('elo: FIDE table 8.1.2 against the normal distribution (sigma = 2000/7): only the published exceptions differ', () => {
    // FIDE's table is the normal CDF with sigma = 2000/7 rounded to hundredths, with six
    // differences set otherwise in the published rows: D = 54, 358, 392 and 620 start the next row
    // although the CDF is still just under the rounding point (57.495, 89.490, 91.497 and
    // 98.4997 %), and the 0.88 row runs to 344 although the CDF reaches 88.5 % at D = 343
    // (88.503 %) and 344 (88.57 %). FIDE applies the published table, so the table is kept as
    // published (both elo.js and elo.cpp).
    const mismatches = [];
    for (let d = 0; d <= 800; d++) {
        const cdf = Math.round(100 * phi(d / (2000 / 7)));
        const table = scoringProbability(d);
        if (cdf !== table) {
            mismatches.push(d);
            assert.equal(Math.abs(cdf - table), 1, `D ${d}: at most one hundredth apart`);
        }
    }
    assert.deepEqual(mismatches, [54, 343, 344, 358, 392, 620]);
    // The published rows up to the 400-point cap, literally.
    const published = [[0, 3, 50], [4, 10, 51], [11, 17, 52], [18, 25, 53], [26, 32, 54], [33, 39, 55], [40, 46, 56], [47, 53, 57],
        [54, 61, 58], [62, 68, 59], [69, 76, 60], [77, 83, 61], [84, 91, 62], [92, 98, 63], [99, 106, 64], [107, 113, 65],
        [114, 121, 66], [122, 129, 67], [130, 137, 68], [138, 145, 69], [146, 153, 70], [154, 162, 71], [163, 170, 72],
        [171, 179, 73], [180, 188, 74], [189, 197, 75], [198, 206, 76], [207, 215, 77], [216, 225, 78], [226, 235, 79],
        [236, 245, 80], [246, 256, 81], [257, 267, 82], [268, 278, 83], [279, 290, 84], [291, 302, 85], [303, 315, 86],
        [316, 328, 87], [329, 344, 88], [345, 357, 89], [358, 374, 90], [375, 391, 91], [392, 400, 92]];
    for (const [lo, hi, pd] of published) for (let d = lo; d <= hi; d++) assert.equal(scoringProbability(d), pd, `D ${d}`);
    // Rows contiguous from 0, PD rising by one hundredth per row; above 735: 1.00.
    let lo = 0;
    FIDE_PD_TABLE.forEach(([hi, pd], i) => {
        assert.equal(pd, 50 + i);
        assert.ok(hi >= lo);
        lo = hi + 1;
    });
    assert.equal(lo, 736);
    assert.equal(scoringProbability(735), 99);
    assert.equal(scoringProbability(736), 100);
    assert.equal(scoringProbability(-60), 58);
});

test('elo: FIDE table 8.1.1 (dp) is the middle of each row of table 8.1.2', () => {
    assert.equal(FIDE_DP_TABLE.length, 51);
    assert.equal(ratingDifference(100), 800);
    assert.equal(ratingDifference(50), 0);
    let lo = 0;
    for (const [hi, pd] of FIDE_PD_TABLE) {
        if (pd > 50) assert.equal(ratingDifference(pd), Math.floor((lo + hi) / 2), `p ${pd}`);
        lo = hi + 1;
    }
    for (let p = 0; p <= 100; p++) assert.equal(ratingDifference(p), -ratingDifference(100 - p) || 0, `p ${p} mirrors 1 - p`);
    assert.deepEqual([ratingDifference(99), ratingDifference(86), ratingDifference(64), ratingDifference(36), ratingDifference(0)],
        [677, 309, 102, -102, -800]);
    assert.equal(ratingDifference(150), 800);
    assert.equal(ratingDifference(-3), -800);
});

test('elo: expected score from the table, capped at a 400-point difference, symmetric', () => {
    assert.equal(expectedScore(1500, 1500), 0.5);
    assert.equal(expectedScore(1503, 1500), 0.5);
    assert.equal(expectedScore(1504, 1500), 0.51);
    assert.equal(expectedScore(1500, 1700), 0.24);     // D 200: 0.76 for the higher-rated
    assert.equal(expectedScore(1700, 1500), 0.76);
    assert.equal(expectedScore(1500, 1900), 0.08);     // D 400: 0.92
    assert.equal(expectedScore(1500, 3500), expectedScore(1500, 1900));
    assert.equal(expectedScore(3000, 100), 0.92);
    for (const [a, b] of [[1500, 1700], [1234, 1987], [2000, 1000], [1500, 1554]]) {
        assert.ok(Math.abs(expectedScore(a, b) + expectedScore(b, a) - 1) < 1e-12);
    }
});

test('elo: rated games, K x (score - PD) rounded half away from zero', () => {
    const r = rated(1500, 40);
    assert.equal(ratingDelta(r, 1500, 1, cfg), 10);
    assert.equal(ratingDelta(r, 1600, 0, cfg), -7);   // PD 0.36: 20 x -0.36 = -7.2
    assert.equal(ratingDelta(r, 1560, 0.5, cfg), 2);  // PD 0.42: 20 x 0.08 = 1.6
    assert.equal(ratingDelta(r, 1100, 1, cfg), 2);    // PD 0.92: 20 x 0.08 = 1.6
    // K 10, D 35 (PD 0.55): 10 x 0.45 = 4.5 -> 5 and 10 x -0.55 = -5.5 -> -6 (in exact hundredths).
    const senior = rated(2435, 200);
    assert.equal(ratingDelta(senior, 2400, 1, cfg), 5);
    assert.equal(ratingDelta(senior, 2400, 0, cfg), -6);
    // Two established players at the same K exchange the same number of points.
    const g = applyGame(rated(1620, 40), rated(1480, 40), 0, cfg);
    assert.equal(g.white.delta, -g.black.delta);
    assert.equal(g.white.delta, -14);                 // D 140: PD 0.69, 20 x -0.69 = -13.8
    assert.equal(g.white.k, 20);
    assert.equal(g.white.expected, 0.69);
});

test('elo: first rating after five games (FIDE 8.2), hand-computed', () => {
    const first = (n, sum, half) => initialRating({ unratedGames: n, unratedOpponents: sum, unratedHalfPoints: half });
    // Five draws against 1500: Ra = (7500 + 2 x 1800) / 7 = 1585.7, p = 3.5 / 7 = 0.50 (dp 0) -> 1586.
    assert.equal(first(5, 7500, 5), 1586);
    // 5 / 5 against 1500: p = 6 / 7 = 0.86 (dp 309): 1585.7 + 309 = 1894.7 -> 1895.
    assert.equal(first(5, 7500, 10), 1895);
    // 0 / 5 against 1500: p = 1 / 7 = 0.14 (dp -309): 1276.7 -> 1277.
    assert.equal(first(5, 7500, 0), 1277);
    // 1.5 / 5 against 1200, 1400, 1600, 1800 and 2000: Ra = 11600 / 7 = 1657.1, p = 2.5 / 7 = 0.36
    // (dp -102): 1555.1 -> 1555; 2.5 / 5 against them: p = 0.50 -> 1657.
    assert.equal(first(5, 8000, 3), 1555);
    assert.equal(first(5, 8000, 5), 1657);
    // Capped at 2200: five wins against Stockfish Max (3500): 3014.3 + 309.
    assert.equal(first(5, 17500, 10), MAX_INITIAL_RATING);
    // The lowest possible: five losses against 100-rated players, 585.7 - 309 -> 277.
    assert.equal(first(5, 500, 0), 277);
});

test('elo: the unrated phase against a rated opponent (Stockfish Novice, 800)', () => {
    const novice = rated(800, 1000);
    // 3 wins, a draw and a loss: Ra = (4000 + 3600) / 7 = 1085.7, p = 4.5 / 7 = 0.64 (dp 102) -> 1188.
    const { record, changes } = play([1, 1, 0.5, 0, 1], novice);
    // RatingChange of an unrated game: before = after (the working rating), except the game that
    // establishes the rating; the rated opponent never moves against an unrated player.
    assert.deepEqual(changes.map((c) => [c.before, c.after, c.k, c.opponentAfter]),
        [[1500, 1500, 0, 800], [1500, 1500, 0, 800], [1500, 1500, 0, 800], [1500, 1500, 0, 800], [1500, 1188, 0, 800]]);
    assert.deepEqual({ ...record }, { rating: 1188, games: 5, wins: 3, draws: 1, losses: 1, peak: 1188, reachedSenior: false, rated: true,
        countedGames: 5, unratedGames: 0, unratedOpponents: 0, unratedHalfPoints: 0 });
    // From then on the K formula applies, K = 40 until 30 counted games (the unrated ones
    // included): D 388, PD 0.91: 40 x 0.09 = 3.6 -> +4.
    const next = applyGame(record, novice, 1, cfg);
    assert.equal(next.white.k, 40);
    assert.equal(next.white.after, 1192);
    assert.equal(next.white.record.countedGames, 6);
    assert.equal(next.white.provisional, true);
    // Before the fifth game the record is unrated and keeps its sums.
    const four = play([1, 1, 0.5, 0], novice).record;
    assert.equal(isUnrated(four), true);
    assert.deepEqual([four.unratedGames, four.unratedOpponents, four.unratedHalfPoints, four.games, four.rating], [4, 3200, 5, 4, 1500]);
    // The zero-score rule (FIDE 8.2.1): the losses before the first half point are left out, so
    // five losses to Stockfish Max leave the player unrated instead of giving a first rating of 2200.
    const lost = play([0, 0, 0, 0, 0], rated(3500, 1000));
    assert.deepEqual([lost.record.rated, lost.record.rating, lost.record.games, lost.record.losses, lost.record.unratedGames,
        lost.record.unratedOpponents, lost.record.countedGames], [false, 1500, 5, 5, 0, 0, 0]);
    assert.ok(lost.changes.every((c) => c.before === 1500 && c.after === 1500 && c.k === 0));
    // Once the player has scored, the losses count: a loss (left out), a draw and four losses
    // against 1500: Ra = 11100 / 7 = 1585.7, p = 1.5 / 7 = 0.21 (dp -230) -> 1356. The peak
    // becomes the first rating, even below the working rating.
    const scored = play([0, 0.5, 0, 0, 0, 0], rated(1500, 100)).record;
    assert.deepEqual([scored.rating, scored.peak, scored.rated, scored.games, scored.losses], [1356, 1356, true, 6, 5]);
});

test('elo: a rated player against an unrated one keeps their rating; the game counts in the games, not the counted games', () => {
    const r = applyGame(rated(1700, 45), defaultRecord(cfg), 0, cfg);
    assert.deepEqual([r.white.before, r.white.after, r.white.k], [1700, 1700, 0]);
    assert.deepEqual([r.white.record.games, r.white.record.losses, r.white.record.countedGames], [46, 1, 45]);
    // The unrated player's game counts at the rated player's rating.
    assert.deepEqual([r.black.record.unratedGames, r.black.record.unratedOpponents, r.black.record.unratedHalfPoints], [1, 1700, 2]);
    assert.equal(r.black.after, 1500);
    // Even the game that establishes the opponent's rating: the ratings before the game count.
    const five = { ...defaultRecord(cfg), games: 4, unratedGames: 4, unratedOpponents: 6000, unratedHalfPoints: 8 };
    const g = applyGame(rated(1800, 80), five, 0, cfg);
    assert.equal(g.white.after, 1800);
    assert.equal(g.black.record.rated, true);
    assert.equal(g.black.after, initialRating({ unratedGames: 5, unratedOpponents: 7800, unratedHalfPoints: 10 }));
});

test('elo: a game between two unrated players counts for both, at the other\'s working rating', () => {
    // A first game lost is left out for both (the zero-score rule, FIDE 8.2.1: the zero score and
    // the opponent's result against it), but it counts in the games and results.
    const first = applyGame(defaultRecord(cfg), defaultRecord(cfg), 0, cfg);
    assert.deepEqual([first.white.record.unratedGames, first.white.record.losses, first.black.record.unratedGames,
        first.black.record.unratedOpponents, first.black.record.wins, first.black.record.games], [0, 1, 0, 0, 1, 1]);
    let w = defaultRecord(cfg), b = defaultRecord(cfg);
    for (const s of [0.5, 1, 1, 0]) {
        const r = applyGame(w, b, s, cfg);
        assert.deepEqual([r.white.before, r.white.after, r.black.before, r.black.after], [1500, 1500, 1500, 1500]);
        w = r.white.record; b = r.black.record;
    }
    assert.deepEqual([w.unratedGames, w.unratedOpponents, w.unratedHalfPoints], [4, 6000, 5]);
    assert.deepEqual([b.unratedGames, b.unratedOpponents, b.unratedHalfPoints], [4, 6000, 3]);
    // The fifth game rates both: White 3.5 / 5 (p = 4.5 / 7 = 0.64, dp 102), Black 1.5 / 5.
    const r = applyGame(w, b, 1, cfg);
    assert.equal(r.white.after, 1586 + 102);
    assert.equal(r.black.after, 1586 - 102);
    assert.deepEqual([r.white.record.rated, r.black.record.rated, r.white.k, r.black.k], [true, true, 0, 0]);
    // A server whose new players start at 1800 (INITIAL_RATING): the working rating counts.
    const c1800 = testConfig({ INITIAL_RATING: '1800' });
    const g = applyGame(defaultRecord(c1800), defaultRecord(c1800), 0.5, c1800);
    assert.equal(g.white.record.unratedOpponents, 1800);
});

test('elo: the zero-score rule closes the unrated booster: an account that only loses gives no one a rating', () => {
    // FIDE 8.2.1 disregards a zero score and the opponents' results against it. Without the
    // second half, five wins against an account that only loses gave a first rating of 1895, the
    // next 25 wins (an unrated opponent: no change) brought the winner to 30 games and onto the
    // leaderboard, and the loser, never counted, stayed unrated to do the same for the next account.
    let booster = defaultRecord(cfg);
    for (let n = 0; n < 3; n++) {
        let fresh = defaultRecord(cfg);
        for (let i = 0; i < 30; i++) {
            const r = applyGame(fresh, booster, 1, cfg);
            assert.deepEqual([r.white.after, r.white.k, r.black.after], [1500, 0, 1500]);
            fresh = r.white.record; booster = r.black.record;
        }
        assert.deepEqual([fresh.rated, fresh.games, fresh.wins, fresh.countedGames, fresh.unratedOpponents], [false, 30, 30, 0, 0]);
        assert.equal(isProvisional(fresh, cfg), true);
    }
    assert.deepEqual([booster.rated, booster.games, booster.losses, booster.countedGames], [false, 90, 90, 0]);
    // A loser that has scored once (a draw) counts, and is rated after five counted games, then
    // loses points under the K formula: a draw and four wins against 1500 give the winner
    // p = 11 / 14 = 0.79 (dp 230): 1816, and the loser 1356.
    let a = defaultRecord(cfg), b = defaultRecord(cfg);
    for (const s of [0.5, 1, 1, 1, 1]) {
        const r = applyGame(a, b, s, cfg);
        a = r.white.record; b = r.black.record;
    }
    assert.deepEqual([a.rated, a.rating, a.countedGames, b.rated, b.rating, b.countedGames], [true, 1816, 5, true, 1356, 5]);
    const next = applyGame(a, b, 1, cfg);
    assert.deepEqual([next.white.k, next.black.k, next.black.after], [40, 40, 1353]);   // D 460 -> 400: 40 x -0.08
    // Scoring against zero scores only is still scoring: that player's losses count, and so do the
    // wins against them.
    const scorer = applyGame(defaultRecord(cfg), defaultRecord(cfg), 1, cfg).white.record;
    assert.deepEqual([scorer.wins, scorer.countedGames], [1, 0]);
    const g = applyGame(defaultRecord(cfg), scorer, 1, cfg);
    assert.deepEqual([g.white.record.countedGames, g.white.record.unratedOpponents, g.black.record.countedGames,
        g.black.record.unratedHalfPoints], [1, 1500, 1, 0]);
});

test('elo: K factor: 40 until 30 counted games (the unrated ones included), 20, 10 once 2400 was reached (for good)', () => {
    const r = rated(1500, 5);
    assert.equal(kFactor(r, cfg), 40);
    assert.equal(kFactor({ ...r, games: 29, countedGames: 29 }, cfg), 40);
    assert.equal(kFactor({ ...r, games: 30, countedGames: 30 }, cfg), 20);
    // The counted games set K and the provisional mark, not the games played.
    assert.equal(kFactor({ ...r, games: 80, countedGames: 29 }, cfg), 40);
    assert.equal(isProvisional({ ...r, games: 80, countedGames: 29 }, cfg), true);
    assert.equal(isProvisional({ ...r, games: 80, countedGames: 30 }, cfg), false);
    // A record without the field (stored before it existed) counts all its games.
    assert.equal(kFactor({ rating: 1500, games: 30, peak: 1500, rated: true }, cfg), 20);
    assert.equal(isProvisional({ games: 30, rated: true }, cfg), false);
    assert.equal(kFactor({ ...r, games: 5, rating: 2400, peak: 2400 }, cfg), 10);
    // Fell back under 2400: still 10.
    assert.equal(kFactor({ ...r, games: 80, rating: 2300, peak: 2410 }, cfg), 10);
    assert.equal(kFactor({ ...r, games: 80, rating: 2300, peak: 2300, reachedSenior: true }, cfg), 10);
    // PROVISIONAL_GAMES is configurable.
    const cfg10 = testConfig({ PROVISIONAL_GAMES: '10' });
    assert.equal(kFactor({ ...r, games: 10, countedGames: 10 }, cfg10), 20);
    assert.equal(isProvisional({ games: 9, rated: true }, cfg10), true);
    assert.equal(isProvisional({ games: 10, rated: true }, cfg10), false);
    // An unrated record is provisional whatever PROVISIONAL_GAMES says.
    const cfg0 = testConfig({ PROVISIONAL_GAMES: '0' });
    assert.equal(isProvisional({ games: 3, rated: false }, cfg0), true);
    assert.equal(isProvisional(defaultRecord(cfg0), cfg0), true);

    // Reaching 2400 by a win sets the senior flag for good.
    const senior = applyGame({ ...rated(2390, 50) }, rated(2500, 50), 1, cfg);
    assert.equal(senior.white.after, 2403);           // D 110: PD 0.35 for the lower-rated, 20 x 0.65 = 13
    assert.equal(senior.white.record.reachedSenior, true);
    const next = applyGame({ ...senior.white.record, rating: 2350 }, rated(1500, 50), 0, cfg);
    assert.equal(next.white.k, 10);
});

test('elo: floor at 100, peak, results and score validation', () => {
    const r = applyGame({ ...rated(101, 50), peak: 1500 }, rated(101, 50), 0, cfg);
    assert.equal(r.white.after, RATING_FLOOR);
    assert.equal(r.white.record.peak, 1500);
    assert.equal(r.black.after, 111);                  // 20 x 0.5
    let w = rated(1500, 40), b = rated(1500, 40);
    for (const s of [1, 0.5, 0]) {
        const g = applyGame(w, b, s, cfg);
        w = g.white.record; b = g.black.record;
    }
    assert.deepEqual([w.wins, w.draws, w.losses, b.wins, b.draws, b.losses, w.games], [1, 1, 1, 1, 1, 1, 43]);
    assert.equal(w.peak, 1510);
    assert.throws(() => applyGame(defaultRecord(cfg), defaultRecord(cfg), 2, cfg), RangeError);
    assert.throws(() => applyGame(defaultRecord(cfg), defaultRecord(cfg), NaN, cfg), RangeError);
});

test('elo: new records start unrated; stored records with games (the previous scheme) stay rated', () => {
    const c1800 = testConfig({ INITIAL_RATING: '1800' });
    assert.deepEqual(defaultRecord(c1800), { rating: 1800, games: 0, wins: 0, draws: 0, losses: 0, peak: 1800, reachedSenior: false,
        rated: false, countedGames: 0, unratedGames: 0, unratedOpponents: 0, unratedHalfPoints: 0 });
    assert.deepEqual(normalizeRecord(null, cfg), defaultRecord(cfg));
    // A record stored before the unrated phase existed: no `rated` field.
    const old = normalizeRecord({ rating: 1650, games: 3, peak: 1700 }, cfg);
    assert.equal(old.rated, true);
    assert.equal(old.countedGames, 3, 'all its games were rated');
    assert.equal(isUnrated(old), false);
    const g = applyGame({ rating: 1650, games: 3, peak: 1700 }, rated(1650, 50), 1, cfg);
    assert.deepEqual([g.white.after, g.white.k], [1670, 40]);
    // Without games and without the field: unrated.
    assert.equal(normalizeRecord({ rating: 1500, games: 0 }, cfg).rated, false);
    assert.equal(isUnrated({ games: 0 }), true);
    assert.equal(isUnrated({ games: 2 }), false);
    // A rated record carries no unrated sums; its counted games are at most its games; an unrated
    // record's are those of its unrated phase.
    assert.equal(normalizeRecord({ rating: 1500, games: 9, rated: true, unratedGames: 3 }, cfg).unratedGames, 0);
    assert.equal(normalizeRecord({ rating: 1500, games: 9, rated: true, countedGames: 12 }, cfg).countedGames, 9);
    assert.equal(normalizeRecord({ rating: 1500, games: 9, rated: true, countedGames: 7 }, cfg).countedGames, 7);
    assert.equal(normalizeRecord({ rating: 1500, games: 9, rated: false, unratedGames: 2, countedGames: 9 }, cfg).countedGames, 2);
    // Partial records: two fresh players.
    const r = applyGame(null, undefined, 1, cfg);
    assert.deepEqual([r.white.after, r.black.after, r.white.provisional], [1500, 1500, true]);
});

test('elo: the vectors shared with the game (elo-vectors.json) are up to date and reproduced', () => {
    assert.equal(fs.readFileSync(VECTORS_PATH, 'utf8'), renderVectors(), 'run node tools/gen-elo-vectors.js');
    const v = JSON.parse(fs.readFileSync(VECTORS_PATH, 'utf8'));
    for (let d = 0; d < v.pd.length; d++) assert.equal(scoringProbability(d), v.pd[d]);
    for (let p = 0; p < v.dp.length; p++) assert.equal(ratingDifference(p), v.dp[p]);
    for (const x of v.initial) assert.equal(initialRating(x), x.rating);
    assert.ok(v.games.length >= 200);
    let unratedSeen = 0, established = 0;
    for (const x of v.games) {
        const r = applyGame(x.white, x.black, x.score, cfg);
        for (const side of ['white', 'black']) {
            const got = r[side], want = x.result[side];
            assert.deepEqual([got.before, got.after, got.k, cppRecord(got.record)], [want.before, want.after, want.k, want.record]);
            if (!x[side].rated) unratedSeen++;
            if (!x[side].rated && want.record.rated) established++;
        }
    }
    assert.ok(unratedSeen > 20 && established > 3, 'the vectors cover the unrated phase');
    assert.equal(UNRATED_GAMES, 5);
});

test('elo: categories', () => {
    assert.equal(categoryOf(180000, 2000, cfg), '3+2');
    assert.equal(categoryOf(60000, 0, cfg), '1+0');
    assert.equal(categoryOf(5400000, 30000, cfg), '90+30');
    assert.equal(categoryOf(180000, 1000, cfg), 'custom');
    assert.equal(categoryOf(15000, 0, cfg), 'custom');
    assert.deepEqual({ ...parseCategory('10+5', cfg) }, { id: '10+5', baseMs: 600000, incMs: 5000 });
    assert.equal(parseCategory('custom', cfg), null);
    assert.equal(parseCategory('4+0', cfg), null);
    assert.equal(parseCategory(undefined, cfg), null);
    assert.equal(isOfficialCategory('3+0', cfg), true);
    const only = testConfig({ RATED_CATEGORIES: '4+4' });
    assert.equal(categoryOf(240000, 4000, only), '4+4');
    assert.equal(categoryOf(180000, 2000, only), 'custom');
});
