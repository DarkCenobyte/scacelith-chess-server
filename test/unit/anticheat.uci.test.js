import test from 'node:test';
import assert from 'node:assert/strict';
import { parseInfoLine, mergeInfo, finalLines, UciEngine, EngineError } from '../../src/anticheat/analysis/engine.js';
import { moveToUci, uciToMove, numberList } from '../../src/anticheat/analysis/moves.js';
import { spearman, ranks, winPercent, moveAccuracy, coefficientOfVariation, welfordAdd, welfordVariance, normalTail } from '../../src/anticheat/analysis/stats.js';

test('parses multipv info lines with cp scores and pv', () => {
    const l = parseInfoLine('info depth 18 seldepth 25 multipv 2 score cp -35 nodes 1234567 nps 900000 hashfull 120 tbhits 0 time 1371 pv e7e5 g1f3 b8c6');
    assert.deepEqual(l, { depth: 18, seldepth: 25, multipv: 2, cp: -35, mate: null, bound: null, wdl: null, pv: ['e7e5', 'g1f3', 'b8c6'] });
});

test('parses mate scores, bounds and wdl', () => {
    const m = parseInfoLine('info depth 12 seldepth 14 multipv 1 score mate -3 nodes 100 pv h2h3 d8h4');
    assert.equal(m.mate, -3);
    assert.equal(m.cp, null);
    const lb = parseInfoLine('info depth 20 seldepth 30 multipv 1 score cp 57 lowerbound nodes 999 nps 1 time 3001 pv d2d4');
    assert.equal(lb.bound, 'lowerbound');
    assert.equal(lb.cp, 57);
    const ub = parseInfoLine('info depth 20 multipv 1 score cp 12 upperbound wdl 100 800 100 pv d2d4 d7d5');
    assert.equal(ub.bound, 'upperbound');
    assert.deepEqual(ub.wdl, [100, 800, 100]);
    assert.deepEqual(ub.pv, ['d2d4', 'd7d5']);
    const mate0 = parseInfoLine('info depth 0 score mate 0');
    assert.equal(mate0.mate, 0);
    assert.deepEqual(mate0.pv, []);
});

test('ignores lines without a score and non-info lines', () => {
    assert.equal(parseInfoLine('info depth 5 currmove e2e4 currmovenumber 1'), null);
    assert.equal(parseInfoLine('info string NNUE evaluation using nn-5af11540bbfe.nnue enabled'), null);
    assert.equal(parseInfoLine('bestmove e2e4 ponder e7e5'), null);
    assert.equal(parseInfoLine('info depth 3 score xx 5'), null);
    assert.equal(parseInfoLine(''), null);
});

test('merges iterations: deepest line per multipv, exact beats bound at equal depth', () => {
    const acc = new Map();
    for (const s of [
        'info depth 9 multipv 1 score cp 20 pv e2e4',
        'info depth 9 multipv 2 score cp 10 pv d2d4',
        'info depth 9 multipv 3 score cp 5 pv c2c4',
        'info depth 10 multipv 1 score cp 40 lowerbound pv e2e4',
        'info depth 10 multipv 1 score cp 25 pv e2e4 e7e5',
        'info depth 10 multipv 1 score cp 99 upperbound pv g1f3',
        'info depth 10 multipv 2 score cp 15 pv d2d4 d7d5',
        'info depth 10 multipv 3 score cp 8 pv g1f3',
    ]) mergeInfo(acc, parseInfoLine(s));
    const lines = finalLines(acc);
    assert.deepEqual(lines.map((l) => [l.multipv, l.depth, l.cp, l.move, l.bound]), [[1, 10, 25, 'e2e4', null], [2, 10, 15, 'd2d4', null], [3, 10, 8, 'g1f3', null]]);
});

test('u16 moves to UCI and back', () => {
    const e2 = 12, e4 = 28, e1 = 4, g1 = 6, a7 = 48, a8 = 56;
    assert.equal(moveToUci(e2 | (e4 << 6)), 'e2e4');
    assert.equal(moveToUci(e1 | (g1 << 6)), 'e1g1');
    assert.equal(moveToUci(a7 | (a8 << 6) | (5 << 12)), 'a7a8q');
    assert.equal(moveToUci(a7 | (a8 << 6) | (2 << 12)), 'a7a8n');
    assert.equal(uciToMove('a7a8q'), a7 | (a8 << 6) | (5 << 12));
    assert.equal(uciToMove('h8h9'), -1);
    for (const m of ['e2e4', 'g8f6', 'b7b8r', 'h2h1b']) assert.equal(moveToUci(uciToMove(m)), m);
});

test('game record columns in any storage shape', () => {
    assert.deepEqual(numberList(new Uint16Array([1, 2, 65535]), 2), [1, 2, 65535]);
    assert.deepEqual(numberList(Buffer.from(new Uint16Array([7, 300]).buffer), 2), [7, 300]);
    assert.deepEqual(numberList(new Uint8Array(new Uint32Array([70000, 5]).buffer), 4), [70000, 5]);
    assert.deepEqual(numberList('[4,5]', 4), [4, 5]);
    assert.deepEqual(numberList(null, 2), []);
});

test('statistics helpers', () => {
    assert.deepEqual(ranks([10, 20, 20, 5]), [2, 3.5, 3.5, 1]);
    assert.equal(spearman([1, 2, 3, 4], [10, 20, 30, 40]), 1);
    assert.ok(Math.abs(spearman([1, 2, 3, 4], [4, 3, 2, 1]) + 1) < 1e-12);
    assert.equal(spearman([1, 1, 1], [1, 2, 3]), null);
    assert.equal(winPercent(0), 50);
    assert.ok(winPercent(1000) > 97 && winPercent(5000) === winPercent(1000));
    assert.equal(moveAccuracy(60, 60), 100);
    assert.ok(moveAccuracy(80, 30) < 15);
    assert.ok(Math.abs(coefficientOfVariation([1, 1, 1, 1])) < 1e-12);
    let s = null;
    for (const x of [2, 4, 4, 4, 5, 5, 7, 9]) s = welfordAdd(s, x);
    assert.equal(s.mean, 5);
    assert.ok(Math.abs(welfordVariance(s) - 32 / 7) < 1e-12);
    assert.ok(Math.abs(normalTail(1.96) - 0.025) < 1e-3);
    assert.ok(Math.abs(normalTail(-1) - 0.8413) < 1e-3);
});

test('an engine that cannot start reports EngineError(spawn)', async () => {
    const e = new UciEngine({ path: '/nonexistent/engine-binary' });
    await assert.rejects(e.start(), (err) => err instanceof EngineError && err.code === 'spawn');
});

test('an engine that hangs is killed and the error says timeout', async () => {
    // `cat` echoes the commands but never says uciok.
    const e = new UciEngine({ path: '/bin/cat', timeoutMs: 200, handshakeTimeoutMs: 200 });
    await assert.rejects(e.start(), (err) => err instanceof EngineError && err.code === 'timeout');
    assert.equal(e.alive, false);
    await e.close();
});
