#!/usr/bin/env node
// Elo vectors: test/fixtures/elo-vectors.json, computed with src/match/elo.js and read by the JS
// tests (test/unit/match.elo.test.js: elo.js still gives these results) and by the game's C++
// tests (tests/elo_tests.cpp: src/game/elo.cpp gives the same), so that a player's online and
// offline ratings follow the same rules to the last point.
//
//   node tools/gen-elo-vectors.js           writes the file
//   node tools/gen-elo-vectors.js --check   exits 1 when the committed file is stale
//
// File format (deterministic):
//   pd[d]        PD of FIDE table 8.1.2 in hundredths for a rating difference d = 0..800 (no cap)
//   dp[p]        dp of FIDE table 8.1.1 for a percentage score p = 0..100
//   initial[]    { unratedGames, unratedOpponents, unratedHalfPoints, rating }: first ratings
//   games[]      { white, black, score, result: { white, black } }: one rated game between two
//                records (C++ elo::applyPair, JS applyGame), score from White's side; each side of
//                the result is { before, after, k, record }. Records hold the fields of the C++
//                elo::Record: rating, games, wins, draws, losses, peak, rated, countedGames,
//                unratedGames, unratedOpponents, unratedHalfPoints (reachedSenior is rated &&
//                peak >= 2400).

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
    applyGame, initialRating, ratingDifference, scoringProbability, DEFAULT_INITIAL_RATING, SENIOR_RATING, UNRATED_GAMES,
} from '../src/match/elo.js';

const here = path.dirname(fileURLToPath(import.meta.url));
export const VECTORS_PATH = path.resolve(here, '../test/fixtures/elo-vectors.json');

// The game's constants (elo.h): the C++ records use them, whatever the server's configuration.
const CFG = { initialRating: DEFAULT_INITIAL_RATING, provisionalGames: 30 };
const FIELDS = ['rating', 'games', 'wins', 'draws', 'losses', 'peak', 'rated', 'countedGames', 'unratedGames', 'unratedOpponents',
    'unratedHalfPoints'];

function mulberry32(seed) {
    let a = seed >>> 0;
    return () => {
        a = (a + 0x6d2b79f5) >>> 0;
        let t = a;
        t = Math.imul(t ^ (t >>> 15), t | 1);
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
}

/** A record with exactly the C++ fields. */
export function cppRecord(r) {
    const out = {};
    for (const f of FIELDS) out[f] = r[f];
    return out;
}

function rated(rating, games, peak = rating, counted = games) {
    return { rating, games, wins: 0, draws: 0, losses: 0, peak: Math.max(peak, rating), rated: true, countedGames: counted, unratedGames: 0,
        unratedOpponents: 0, unratedHalfPoints: 0 };
}

// An unrated record whose `games` counted games scored `halfPoints` (at least one when it has
// games: a counted game comes after the first half point), as wins, then a draw.
function unrated(games, opponents, halfPoints, rating = DEFAULT_INITIAL_RATING) {
    const wins = Math.floor(halfPoints / 2), draws = halfPoints % 2;
    return { rating, games, wins, draws, losses: games - wins - draws, peak: rating, rated: false, countedGames: games, unratedGames: games,
        unratedOpponents: opponents, unratedHalfPoints: halfPoints };
}

// The same record after `lost` zero scores (losses before the first half point) and `beaten` wins
// against zero scores: the zero-score rule left them all out of the unrated phase.
function afterLosses(record, lost, beaten = 0) {
    return { ...record, games: record.games + lost + beaten, losses: record.losses + lost, wins: record.wins + beaten };
}

// Hand-picked games: every branch, the boundaries of K (counted games), the 400-point rule, the
// floor, the establishing game, the cap, the zero-score rule (for both players) and the unrated /
// rated combinations.
const EDGE_GAMES = [
    [unrated(0, 0, 0), unrated(0, 0, 0), 1],
    [unrated(0, 0, 0), unrated(0, 0, 0), 0.5],
    [afterLosses(unrated(0, 0, 0), 0, 1), unrated(0, 0, 0), 0],
    [unrated(4, 6000, 4), unrated(4, 6000, 4), 0.5],
    [unrated(4, 6000, 8), unrated(0, 0, 0), 1],
    [unrated(4, 6000, 8), afterLosses(unrated(0, 0, 0), 12), 1],
    [unrated(4, 6000, 8), afterLosses(unrated(0, 0, 0), 0, 1), 1],
    [unrated(4, 6000, 8), afterLosses(unrated(0, 0, 0), 3, 2), 0],
    [rated(1700, 45), unrated(0, 0, 0), 1],
    [rated(1700, 45), afterLosses(unrated(0, 0, 0), 2, 1), 1],
    [unrated(4, 14000, 8), rated(3500, 400), 1],
    [unrated(4, 400, 1), rated(100, 60), 0],
    [unrated(0, 0, 0), rated(3500, 400), 0],
    [afterLosses(unrated(0, 0, 0), 4), rated(3500, 400), 0],
    [afterLosses(unrated(0, 0, 0), 4), rated(3500, 400), 0.5],
    [afterLosses(unrated(3, 9000, 1), 2), rated(3500, 400), 0],
    [afterLosses(unrated(4, 6800, 3), 27), rated(1500, 100), 1],
    [rated(1700, 45), unrated(2, 3000, 2), 0],
    [rated(2450, 300), unrated(4, 9000, 7), 1],
    [rated(1500, 29, 1520), rated(1700, 30, 1750), 1],
    [rated(1500, 30, 1520), rated(1700, 30, 1750), 0.5],
    [rated(1500, 60, 1520, 29), rated(1700, 60, 1750, 30), 1],
    [rated(1500, 45, 1500, 5), rated(1480, 45, 1500, 45), 0],
    [rated(1800, 100, 1850), rated(1200, 100, 1300), 1],
    [rated(1800, 100, 1850), rated(1200, 100, 1300), 0],
    [rated(1800, 100, 1850), rated(1200, 100, 1300), 0.5],
    [rated(2390, 50), rated(2300, 60), 1],
    [rated(2350, 80, 2410), rated(2380, 90, 2390), 0],
    [rated(2400, 5), rated(2000, 5), 0.5],
    [rated(110, 40, 1500), rated(1600, 40), 0],
    [rated(101, 50, 1500), rated(101, 50), 0],
    [rated(2600, 200, 2700), rated(2650, 300, 2700), 0.5],
    [rated(1999, 29), rated(2011, 29), 1],
    [rated(1503, 10, 1510), rated(1497, 10), 0.5],
    [rated(2050, 60, 2450), rated(2399, 60), 0.5],
    [rated(1850, 33, 1900), rated(2420, 3), 1],
    [rated(1500, 1), rated(1100, 1), 1],
    [rated(1500, 1), rated(1099, 1), 1],
    [rated(1500, 40), rated(1504, 40), 0.5],
    [rated(1500, 40), rated(1503, 40), 0.5],
    [rated(1500, 12), rated(1554, 12), 0.5],
    [rated(1500, 12), rated(1553, 12), 0.5],
    [rated(1500, 12), rated(1892, 12), 1],
    [rated(1500, 12), rated(1891, 12), 1],
];

function randomRecord(rnd) {
    const between = (lo, hi) => lo + Math.floor(rnd() * (hi - lo + 1));
    if (rnd() < 0.3) {
        const n = between(0, UNRATED_GAMES - 1);
        let sum = 0;
        for (let i = 0; i < n; i++) sum += between(100, 3500);
        // The first game counted scored (the zero-score rule), after 0 to 3 losses and 0 to 2 wins
        // left out (zero scores, the player's own or their opponents').
        return afterLosses(unrated(n, sum, n ? between(1, 2 * n) : 0), between(0, 3), between(0, 2));
    }
    const rating = between(100, 2900);
    const peak = rnd() < 0.5 ? rating : Math.min(3000, rating + between(0, 400));
    const games = between(1, 120);
    // Half of them count all their games (records stored before the counted games existed).
    const r = rated(rating, games, peak, rnd() < 0.5 ? games : between(Math.min(games, UNRATED_GAMES), games));
    r.wins = between(0, r.games);
    r.losses = between(0, r.games - r.wins);
    r.draws = r.games - r.wins - r.losses;
    return r;
}

function game(white, black, score) {
    const res = applyGame(white, black, score, CFG);
    const side = (s) => ({ before: s.before, after: s.after, k: s.k, record: cppRecord(s.record) });
    return { white: cppRecord(white), black: cppRecord(black), score, result: { white: side(res.white), black: side(res.black) } };
}

/** Builds the vectors. */
export function buildVectors() {
    const pd = [];
    for (let d = 0; d <= 800; d++) pd.push(scoringProbability(d));
    const dp = [];
    for (let p = 0; p <= 100; p++) dp.push(ratingDifference(p));
    const initial = [];
    const rnd = mulberry32(0x5ca1e17);
    for (const [n, sum, half] of [[5, 7500, 5], [5, 7500, 10], [5, 7500, 0], [5, 4000, 10], [5, 4000, 0], [5, 17500, 10],
        [5, 500, 0], [5, 12000, 7], [5, 9321, 3], [6, 9000, 6], [3, 4500, 3]]) {
        initial.push({ unratedGames: n, unratedOpponents: sum, unratedHalfPoints: half, rating: initialRating({ unratedGames: n, unratedOpponents: sum, unratedHalfPoints: half }) });
    }
    for (let i = 0; i < 40; i++) {
        let sum = 0;
        for (let g = 0; g < UNRATED_GAMES; g++) sum += 100 + Math.floor(rnd() * 3401);
        const half = Math.floor(rnd() * (2 * UNRATED_GAMES + 1));
        initial.push({ unratedGames: UNRATED_GAMES, unratedOpponents: sum, unratedHalfPoints: half,
            rating: initialRating({ unratedGames: UNRATED_GAMES, unratedOpponents: sum, unratedHalfPoints: half }) });
    }
    const games = EDGE_GAMES.map(([w, b, s]) => game(w, b, s));
    for (let i = 0; i < 200; i++) games.push(game(randomRecord(rnd), randomRecord(rnd), [0, 0.5, 1][Math.floor(rnd() * 3)]));
    return {
        generator: 'dedicated-server/tools/gen-elo-vectors.js (src/match/elo.js); checked by tests/elo_tests.cpp',
        seniorRating: SENIOR_RATING,
        pd, dp, initial, games,
    };
}

/** Serialises the vectors: tables on one line each, one vector per line (stable, diff-friendly). */
export function renderVectors(v = buildVectors()) {
    const head = Object.entries(v).filter(([k]) => k !== 'initial' && k !== 'games')
        .map(([k, x]) => `  ${JSON.stringify(k)}: ${JSON.stringify(x)}`);
    const list = (a) => a.map((x) => '    ' + JSON.stringify(x)).join(',\n');
    return `{\n${head.join(',\n')},\n  "initial": [\n${list(v.initial)}\n  ],\n  "games": [\n${list(v.games)}\n  ]\n}\n`;
}

function main(argv) {
    const text = renderVectors();
    const current = fs.existsSync(VECTORS_PATH) ? fs.readFileSync(VECTORS_PATH, 'utf8') : null;
    const rel = path.relative(process.cwd(), VECTORS_PATH);
    if (argv.includes('--check')) {
        if (current !== text) { console.error(`${rel} is stale: run node tools/gen-elo-vectors.js`); return 1; }
        return 0;
    }
    if (current === text) { console.log(`${rel} up to date`); return 0; }
    fs.mkdirSync(path.dirname(VECTORS_PATH), { recursive: true });
    fs.writeFileSync(VECTORS_PATH, text);
    console.log(`wrote ${rel}`);
    return 0;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
    process.exitCode = main(process.argv.slice(2));
}
