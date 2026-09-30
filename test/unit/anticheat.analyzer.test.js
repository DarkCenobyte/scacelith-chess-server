import test from 'node:test';
import assert from 'node:assert/strict';
import { analyseGame, analysePositions, computeFeatures, lineCp, ANALYSIS } from '../../src/anticheat/analysis/analyzer.js';
import { moveToUci } from '../../src/anticheat/analysis/moves.js';

// Distinct dummy moves (the analyser never checks legality: the engine does).
const mv = (i) => (i % 64) | (((i * 7 + 3) % 64) << 6);
const line = (multipv, move, cp, mate = null) => ({ multipv, depth: 10, cp: mate === null ? cp : null, mate, bound: null, move, pv: move ? [move] : [] });

test('lineCp converts mate scores', () => {
    assert.equal(lineCp({ cp: 35, mate: null }), 35);
    assert.equal(lineCp({ cp: null, mate: 3 }), ANALYSIS.mateCp - 30);
    assert.equal(lineCp({ cp: null, mate: -2 }), -ANALYSIS.mateCp + 20);
    assert.equal(lineCp({ cp: null, mate: 0 }), -ANALYSIS.mateCp);
});

test('skips opening, forced and decided moves; scores the others', async () => {
    const moves = Array.from({ length: 24 }, (_, i) => mv(i));
    const uci = moves.map(moveToUci);
    const other = (k) => `h${k}h${k}`;     // never a played move
    // Deep analyses by ply (position before move `ply`), from the side to move's view.
    const deep = {
        16: [line(1, uci[16], 30), line(2, other(1), 20), line(3, other(2), -100)],   // white: T1, complex (2 within 50)
        17: [line(1, uci[17], -25)],                                                    // black: forced
        18: [line(1, other(1), 700), line(2, other(2), 650), line(3, uci[18], 600)],    // white: decided
        19: [line(1, other(1), 10), line(2, other(2), -150), line(3, other(3), -200)],  // black: not top 3
        20: [line(1, other(1), 200), line(2, uci[20], 150), line(3, other(3), 100)],    // white (after black's move: black lost 210)
        21: [line(1, uci[21], -150), line(2, other(2), -300), line(3, other(3), -400)], // black: T1, gap 150
        22: [line(1, other(1), 160), line(2, uci[22], 140), line(3, other(3), 0)],       // white: top 2, complex
        23: [line(1, other(1), -130), line(2, uci[23], -500), line(3, other(3), -600)], // black: blunder 370
        24: [line(1, other(1), 480)],                                                   // final position (white to move)
    };
    const fastBest = { 16: uci[16], 19: other(1), 20: other(1), 21: other(4), 22: uci[22], 23: other(1) };
    let clears = 0, newGames = 0;
    const calls = [];
    const engine = {
        name: 'fake-engine',
        async newGame() { newGames++; },
        async clearHash() { clears++; },
        async analyse(ms, { depth, multiPv }) {
            calls.push([ms.length, depth, multiPv]);
            assert.deepEqual(ms, uci.slice(0, ms.length));
            if (depth === 12) return { lines: deep[ms.length], bestmove: deep[ms.length][0].move };
            return { lines: [line(1, fastBest[ms.length], 0)], bestmove: fastBest[ms.length] };
        },
    };
    const spentMs = moves.map((_, i) => 1000 + i * 10);
    const f = await analyseGame(engine, { id: 42, category: '5+0', whiteId: 1, blackId: 2, whiteRating: 1600, blackRating: 1550, moves: new Uint16Array(moves), spentMs: new Uint32Array(spentMs), endedAt: 5 },
        { depthFast: 4, depthDeep: 12, extra: { white: { ratingGames: 40 } } });
    assert.equal(newGames, 1);
    // Deep pass: plies 16..24; shallow pass only where a move is scored (not forced / decided).
    assert.deepEqual(calls.filter((c) => c[1] === 12).map((c) => c[0]), [16, 17, 18, 19, 20, 21, 22, 23, 24]);
    assert.deepEqual(calls.filter((c) => c[1] === 4).map((c) => c[0]), [16, 19, 20, 21, 22, 23]);
    assert.ok(calls.every((c) => c[2] === (c[1] === 12 ? 3 : 1)));
    assert.equal(clears, 6, 'hash cleared before every shallow search');

    assert.equal(f.gameId, 42);
    assert.equal(f.engine, 'fake-engine');
    const w = f.white, b = f.black;
    assert.equal(w.userId, 1);
    assert.equal(w.ratingGames, 40);
    assert.deepEqual(w.skipped, { opening: 8, forced: 0, decided: 1, missing: 0 });
    assert.deepEqual(b.skipped, { opening: 8, forced: 1, decided: 0, missing: 0 });
    // White scored plies 16, 20, 22: losses 0, 50, 20.
    assert.equal(w.n, 3);
    assert.equal(w.acpl, Math.round((0 + 50 + 20) / 3 * 10) / 10);
    assert.equal(w.t1Deep, Math.round(1 / 3 * 1e4) / 1e4);
    assert.equal(w.t1Fast, Math.round(2 / 3 * 1e4) / 1e4);   // 16 and 22 match the shallow choice
    assert.equal(w.top3, 1);
    assert.equal(w.nComplex, 3);                               // 16 (30/20), 20 (200/150) and 22 (160/140)
    assert.equal(w.t1Complex, 0.3333);
    // Black scored plies 19, 21, 23: losses 10-(-200)=210 (from the next position), 0, 370.
    assert.equal(b.n, 3);
    assert.equal(b.acpl, Math.round((210 + 0 + 370) / 3 * 10) / 10);
    assert.equal(b.top3, Math.round(2 / 3 * 1e4) / 1e4);
    assert.ok(w.accuracy > b.accuracy);
    assert.equal(w.timeCorr, null, 'too few timed moves for a correlation');
    assert.deepEqual(w.moves[0], [16, 0, 1 | 2 | 4 | 8, 2, spentMs[16]]);
});

test('time features: think time following complexity gives a high rank correlation', () => {
    const n = 60;
    const moves = Array.from({ length: n }, (_, i) => mv(i));
    const uci = moves.map(moveToUci);
    const deep = new Map(), fast = new Map();
    const spent = new Array(n).fill(0);
    for (let p = 16; p <= n; p++) {
        const k = p % 3;       // 0: one clear best move, 1: two good moves, 2: three good moves
        const cps = k === 0 ? [50, -150, -300] : k === 1 ? [40, 20, -200] : [30, 25, 10];
        deep.set(p, { lines: [line(1, p < n ? uci[p] : 'a1a1', cps[0]), line(2, 'b1b1', cps[1]), line(3, 'c1c1', cps[2])], bestmove: p < n ? uci[p] : null });
        if (p < n) {
            fast.set(p, { lines: [], bestmove: uci[p] });
            spent[p] = 2000 + 4000 * k + (p % 5) * 100;
        }
    }
    const f = computeFeatures({ moves, spentMs: spent }, { deep, fast });
    assert.ok(f.white.timeCorr > 0.8, `white ${f.white.timeCorr}`);
    assert.ok(f.black.timeCorr > 0.8);
    assert.ok(f.white.timeCv > 0.3);
    assert.equal(f.white.t1Deep, 1);
    assert.equal(f.white.acpl, 0);
    assert.equal(f.white.accuracy, 100);
    // Constant think time: no correlation, tiny variation.
    const flat = computeFeatures({ moves, spentMs: spent.map(() => 3000) }, { deep, fast });
    assert.equal(flat.white.timeCorr, null);
    assert.equal(flat.white.timeCv, 0);
});

test('short games give empty features', async () => {
    const engine = { name: 'x', async newGame() { throw new Error('not called'); }, async clearHash() {}, async analyse() { throw new Error('not called'); } };
    const f = await analyseGame(engine, { id: 1, moves: [mv(1), mv(2)], spentMs: [0, 0], whiteId: 1, blackId: 2 }, { depthFast: 2, depthDeep: 4 });
    assert.equal(f.white.n, 0);
    assert.equal(f.black.accuracy, null);
});

test('a deep search stopped by the node limit is repeated from an empty hash, without the limit', async () => {
    const uci = Array.from({ length: 20 }, (_, i) => moveToUci(mv(i)));
    const calls = [];
    const engine = {
        name: 'fake-engine',
        async newGame() { calls.push('newGame'); },
        async clearHash() { calls.push('clearHash'); },
        async analyse(ms, { depth, nodes }) {
            calls.push([ms.length, depth, nodes ?? null]);
            // The search of ply 18 blows up on the hash: the limit stops it, with an unfinished answer.
            const nodeLimited = depth === 12 && ms.length === 18 && nodes !== undefined;
            const cp = nodeLimited ? 900 : 10;
            return { lines: [line(1, uci[ms.length] ?? null, cp), line(2, 'h1h1', cp - 20), line(3, 'h2h2', cp - 40)], bestmove: uci[ms.length] ?? null, nodeLimited };
        },
    };
    const { deep } = await analysePositions(engine, uci, { depthFast: 4, depthDeep: 12 });
    const deepCalls = calls.filter((c) => c === 'clearHash' || c[1] === 12);
    assert.deepEqual(deepCalls, [
        [16, 12, ANALYSIS.deepNodeLimit], [17, 12, ANALYSIS.deepNodeLimit],
        [18, 12, ANALYSIS.deepNodeLimit], 'clearHash', [18, 12, null],
        [19, 12, ANALYSIS.deepNodeLimit], [20, 12, ANALYSIS.deepNodeLimit],
        // The shallow pass clears the hash before each of its searches.
        'clearHash', 'clearHash', 'clearHash', 'clearHash',
    ]);
    assert.equal(deep.get(18).lines[0].cp, 10, 'the answer of the repeated search');
    assert.ok(ANALYSIS.deepNodeLimit >= 10_000_000, 'far above a normal deep search');
});
