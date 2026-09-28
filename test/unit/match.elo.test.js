import { test } from 'node:test';
import assert from 'node:assert/strict';
import { testConfig } from '../../src/config.js';
import {
    expectedScore, kFactor, applyGame, defaultRecord, normalizeRecord, isProvisional, ratingDelta,
    categoryOf, parseCategory, isOfficialCategory, SENIOR_RATING, RATING_FLOOR,
} from '../../src/match/elo.js';

const cfg = testConfig();

// Produced by compiling the game's src/game/elo.cpp with a small harness: for each game, White
// and Black are rated with elo::applyResult against the other's rating before the game.
//  White: rating, games, peak | Black: rating, games, peak | score (White) |
//  -> K White, White after, White peak, K Black, Black after, Black peak, expected score of White
const CPP_TABLE = [
    [1500, 0, 1500, 1500, 0, 1500, 1, /* -> */ 40, 1520, 1520, 40, 1480, 1500, 0.5000000000],
    [1500, 0, 1500, 1500, 0, 1500, 0.5, /* -> */ 40, 1500, 1500, 40, 1500, 1500, 0.5000000000],
    [1500, 0, 1500, 1500, 0, 1500, 0, /* -> */ 40, 1480, 1500, 40, 1520, 1520, 0.5000000000],
    [1500, 29, 1520, 1700, 30, 1750, 1, /* -> */ 40, 1530, 1530, 20, 1685, 1750, 0.2402530734],
    [1500, 30, 1520, 1700, 30, 1750, 0.5, /* -> */ 20, 1505, 1520, 20, 1695, 1750, 0.2402530734],
    [1700, 45, 1710, 1500, 12, 1500, 0, /* -> */ 20, 1685, 1710, 40, 1530, 1530, 0.7597469266],
    [1800, 100, 1850, 1200, 100, 1300, 1, /* -> */ 20, 1802, 1850, 20, 1198, 1300, 0.9090909091],
    [1800, 100, 1850, 1200, 100, 1300, 0, /* -> */ 20, 1782, 1850, 20, 1218, 1300, 0.9090909091],
    [1800, 100, 1850, 1200, 100, 1300, 0.5, /* -> */ 20, 1792, 1850, 20, 1208, 1300, 0.9090909091],
    [2390, 50, 2390, 2300, 60, 2300, 1, /* -> */ 20, 2397, 2397, 20, 2293, 2300, 0.6266990817],
    [2350, 80, 2410, 2380, 90, 2390, 0, /* -> */ 10, 2345, 2410, 20, 2389, 2390, 0.4569335080],
    [2400, 5, 2400, 2000, 5, 2000, 0.5, /* -> */ 10, 2396, 2400, 40, 2016, 2016, 0.9090909091],
    [110, 40, 1500, 1600, 40, 1600, 0, /* -> */ 20, 108, 1500, 20, 1602, 1602, 0.0909090909],
    [120, 3, 1500, 900, 3, 1500, 0, /* -> */ 40, 116, 1500, 40, 904, 1500, 0.0909090909],
    [1234, 31, 1300, 1287, 17, 1287, 0.5, /* -> */ 20, 1236, 1300, 40, 1284, 1287, 0.4243130476],
    [1523, 64, 1600, 1511, 64, 1560, 0.5, /* -> */ 20, 1523, 1600, 20, 1511, 1560, 0.5172625244],
    [1999, 29, 1999, 2011, 29, 2011, 1, /* -> */ 40, 2020, 2020, 40, 1990, 2011, 0.4827374756],
    [2600, 200, 2700, 2650, 300, 2700, 0.5, /* -> */ 10, 2601, 2700, 10, 2649, 2700, 0.4285368826],
    [2000, 30, 2000, 1000, 30, 1100, 1, /* -> */ 20, 2002, 2002, 20, 998, 1100, 0.9090909091],
    [1000, 30, 1100, 2000, 30, 2000, 1, /* -> */ 20, 1018, 1100, 20, 1982, 2000, 0.0909090909],
    [1503, 10, 1510, 1497, 10, 1500, 0.5, /* -> */ 40, 1503, 1510, 40, 1497, 1500, 0.5086338358],
    [1350, 29, 1400, 1362, 30, 1400, 0, /* -> */ 40, 1331, 1400, 20, 1372, 1400, 0.4827374756],
    [105, 5, 1500, 110, 5, 1500, 0, /* -> */ 40, 100, 1500, 40, 130, 1500, 0.4928049183],
    [100, 50, 1500, 480, 50, 1500, 1, /* -> */ 20, 118, 1500, 20, 462, 1500, 0.1008826284],
    [130, 50, 1500, 480, 50, 1500, 0, /* -> */ 20, 128, 1500, 20, 482, 1500, 0.1176617030],
    [2050, 60, 2450, 2399, 60, 2399, 0.5, /* -> */ 10, 2054, 2450, 20, 2391, 2399, 0.1182606407],
    [1850, 33, 1900, 2420, 3, 2420, 1, /* -> */ 20, 1868, 1900, 10, 2411, 2420, 0.0909090909],
    [1612, 29, 1640, 1587, 29, 1600, 0.5, /* -> */ 40, 1611, 1640, 40, 1588, 1600, 0.5359159269],
    [1777, 29, 1777, 1433, 31, 1500, 0, /* -> */ 40, 1742, 1777, 20, 1451, 1500, 0.8787049512],
];

function rec(rating, games, peak) {
    return { rating, games, wins: 0, draws: 0, losses: 0, peak, reachedSenior: peak >= SENIOR_RATING };
}

test('elo: applyGame reproduces the game\'s elo.cpp (K, 400 cap, senior, floor, draws, rounding)', () => {
    for (const row of CPP_TABLE) {
        const [wr, wg, wp, br, bg, bp, s, kw, wAfter, wPeak, kb, bAfter, bPeak, e] = row;
        const white = rec(wr, wg, wp), black = rec(br, bg, bp);
        const label = JSON.stringify(row);
        assert.equal(kFactor(white, cfg), kw, 'K white ' + label);
        assert.equal(kFactor(black, cfg), kb, 'K black ' + label);
        assert.ok(Math.abs(expectedScore(wr, br) - e) < 1e-9, 'E ' + label);
        const r = applyGame(white, black, s, cfg);
        assert.equal(r.white.before, wr, label);
        assert.equal(r.white.after, wAfter, 'white after ' + label);
        assert.equal(r.white.record.peak, wPeak, 'white peak ' + label);
        assert.equal(r.black.before, br, label);
        assert.equal(r.black.after, bAfter, 'black after ' + label);
        assert.equal(r.black.record.peak, bPeak, 'black peak ' + label);
        assert.equal(r.white.k, kw);
        assert.equal(r.black.k, kb);
        assert.equal(r.white.delta, wAfter - wr);
        assert.equal(r.white.games, wg + 1);
        assert.equal(r.black.games, bg + 1);
        // Inputs are not modified.
        assert.equal(white.rating, wr);
        assert.equal(white.games, wg);
        assert.equal(black.rating, br);
    }
});

test('elo: expected score is capped at a 400-point difference and symmetric', () => {
    assert.equal(expectedScore(1500, 1500), 0.5);
    assert.equal(expectedScore(1500, 3500), expectedScore(1500, 1900));
    assert.equal(expectedScore(3000, 100), expectedScore(1500, 1100));
    assert.ok(Math.abs(expectedScore(2400, 800) - 10 / 11) < 1e-12);
    for (const [a, b] of [[1500, 1700], [1234, 1987], [2000, 1000]]) {
        assert.ok(Math.abs(expectedScore(a, b) + expectedScore(b, a) - 1) < 1e-12);
    }
});

test('elo: K factor: 40 provisional, 20 established, 10 once 2400 was reached (for good)', () => {
    const r = defaultRecord(cfg);
    assert.equal(kFactor(r, cfg), 40);
    assert.equal(kFactor({ ...r, games: 29 }, cfg), 40);
    assert.equal(kFactor({ ...r, games: 30 }, cfg), 20);
    assert.equal(kFactor({ ...r, games: 5, rating: 2400, peak: 2400 }, cfg), 10);
    // Fell back under 2400: still 10.
    assert.equal(kFactor({ ...r, games: 80, rating: 2300, peak: 2410 }, cfg), 10);
    assert.equal(kFactor({ ...r, games: 80, rating: 2300, peak: 2300, reachedSenior: true }, cfg), 10);
    // PROVISIONAL_GAMES is configurable.
    const cfg10 = testConfig({ PROVISIONAL_GAMES: '10' });
    assert.equal(kFactor({ ...r, games: 10 }, cfg10), 20);
    assert.equal(isProvisional({ games: 9 }, cfg10), true);
    assert.equal(isProvisional({ games: 10 }, cfg10), false);
});

test('elo: records track games, results, peak and the senior flag', () => {
    let w = defaultRecord(cfg), b = defaultRecord(cfg);
    assert.deepEqual(w, { rating: 1500, games: 0, wins: 0, draws: 0, losses: 0, peak: 1500, reachedSenior: false });
    let r = applyGame(w, b, 1, cfg);
    w = r.white.record; b = r.black.record;
    assert.deepEqual([w.wins, w.draws, w.losses, b.wins, b.draws, b.losses], [1, 0, 0, 0, 0, 1]);
    r = applyGame(w, b, 0.5, cfg);
    w = r.white.record; b = r.black.record;
    assert.deepEqual([w.wins, w.draws, w.losses, b.wins, b.draws, b.losses], [1, 1, 0, 0, 1, 1]);
    assert.equal(w.games, 2);
    assert.equal(w.peak, 1520);
    assert.equal(r.white.provisional, true);

    const senior = applyGame({ ...defaultRecord(cfg), rating: 2390, peak: 2390, games: 50 }, { ...defaultRecord(cfg), rating: 2500, peak: 2500, games: 50 }, 1, cfg);
    assert.equal(senior.white.after, 2403);  // 20 * (1 - 0.3468) = 13.06
    assert.equal(senior.white.record.reachedSenior, true);
    assert.equal(senior.white.provisional, false);
    // Next game: K = 10 even after dropping below 2400.
    const next = applyGame({ ...senior.white.record, rating: 2350 }, defaultRecord(cfg), 0, cfg);
    assert.equal(next.white.k, 10);
});

test('elo: floor at 100, rounding and score validation', () => {
    const r = applyGame({ ...defaultRecord(cfg), rating: 101, peak: 1500 }, { ...defaultRecord(cfg), rating: 101 }, 0, cfg);
    assert.equal(r.white.after, RATING_FLOOR);
    assert.equal(r.black.after, 121);
    // ratingDelta rounds like std::lround; it is the change before the floor.
    assert.equal(ratingDelta({ rating: 1500, games: 40, peak: 1500 }, 1500, 1, cfg), 10);
    assert.equal(ratingDelta({ rating: 1500, games: 40, peak: 1500 }, 1600, 0, cfg), -7);  // 20 * -0.3599 = -7.2
    assert.equal(ratingDelta({ rating: 1500, games: 40, peak: 1500 }, 1560, 0, cfg), -8);  // 20 * -0.4144 = -8.29
    assert.throws(() => applyGame(defaultRecord(cfg), defaultRecord(cfg), 2, cfg), RangeError);
    assert.throws(() => applyGame(defaultRecord(cfg), defaultRecord(cfg), NaN, cfg), RangeError);
});

test('elo: defaults and partial records', () => {
    const cfg1800 = testConfig({ INITIAL_RATING: '1800' });
    assert.equal(defaultRecord(cfg1800).rating, 1800);
    assert.equal(defaultRecord(cfg1800).peak, 1800);
    assert.equal(defaultRecord().rating, 1500);
    const n = normalizeRecord({ rating: 1650, games: 3 }, cfg);
    assert.deepEqual(n, { rating: 1650, games: 3, wins: 0, draws: 0, losses: 0, peak: 1650, reachedSenior: false });
    assert.deepEqual(normalizeRecord(null, cfg), defaultRecord(cfg));
    const r = applyGame(null, undefined, 1, cfg);
    assert.equal(r.white.after, 1520);
    assert.equal(r.black.after, 1480);
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
