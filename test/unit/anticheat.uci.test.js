import test from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { parseInfoLine, mergeInfo, finalLines, parseNetworkReplica, networkMemory, UciEngine, EngineError } from '../../src/anticheat/analysis/engine.js';
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

test('parses where Stockfish keeps a replica of its network, and nothing else', () => {
    assert.deepEqual(parseNetworkReplica('info string Network replica 1: Shared memory.'), { replica: 1, memory: 'shared', error: null });
    assert.deepEqual(parseNetworkReplica('info string Network replica 2: Local memory. Shared memory not supported by the OS. Local allocation fallback.'),
        { replica: 2, memory: 'local', error: 'Shared memory not supported by the OS. Local allocation fallback.' });
    assert.deepEqual(parseNetworkReplica('info string Network replica 2: No allocation.'), { replica: 2, memory: 'none', error: null });
    // A status of a later version: kept as the explanation.
    assert.deepEqual(parseNetworkReplica('info string Network replica 1: Unknown status.'), { replica: 1, memory: 'unknown', error: 'Unknown status.' });
    for (const other of ['info string NNUE evaluation using nn-1a298aa575a0.nnue (109MiB, (86896, 1024, 32, 32, 1))', 'info string Using 1 thread',
        'info string Available processors: 0-3', 'info depth 1 seldepth 1 multipv 1 score cp 20 pv e2e4', 'bestmove e2e4', '']) {
        assert.equal(parseNetworkReplica(other), null, other);
    }
});

test('a network is shared when every allocated replica is', () => {
    const r = (memory, error = null) => ({ memory, error });
    assert.deepEqual(networkMemory([]), { memory: null, error: null }, 'not reported (Stockfish 16, another engine)');
    assert.deepEqual(networkMemory([r('shared')]), { memory: 'shared', error: null });
    assert.deepEqual(networkMemory([r('shared'), r('none')]), { memory: 'shared', error: null }, 'a NUMA node without threads needs no replica');
    assert.deepEqual(networkMemory([r('shared'), r('local', 'why')]), { memory: 'local', error: 'why' });
    assert.deepEqual(networkMemory([r('unknown', 'Unknown status.')]), { memory: 'local', error: 'Unknown status.' });
    assert.equal(networkMemory([r('none')]).memory, 'local');
});

const FAKE = fileURLToPath(new URL('./helpers/fake-uci-engine.js', import.meta.url));
const fakeEngine = (...strings) => new UciEngine({ path: process.execPath, args: [FAKE, ...strings], lowPriority: false });

test('the engine learns at start where its network lives, among its other info strings', async (t) => {
    const sf19 = fakeEngine('Available processors: 0-3', 'Using 1 thread', 'NNUE evaluation using nn-1a298aa575a0.nnue (109MiB, (86896, 1024, 32, 32, 1))',
        'Network replica 1: Shared memory.');
    t.after(() => sf19.close());
    await sf19.start();
    assert.deepEqual([sf19.name, sf19.net, sf19.netMemory, sf19.netMemoryError, sf19.starts], ['Fake Engine 1', 'nn-1a298aa575a0.nnue', 'shared', null, 1]);
    // A search still reads its score line.
    assert.equal((await sf19.analyse(['e2e4'], { depth: 5 })).lines[0].cp, 20);
    assert.equal(sf19.netMemory, 'shared');

    const fallback = fakeEngine('NNUE evaluation using nn-1a298aa575a0.nnue (109MiB, (86896, 1024, 32, 32, 1))',
        'Network replica 1: Local memory. Shared memory is not serving to other processes');
    t.after(() => fallback.close());
    await fallback.start();
    assert.deepEqual([fallback.netMemory, fallback.netMemoryError], ['local', 'Shared memory is not serving to other processes']);

    const sf16 = fakeEngine('NNUE evaluation using nn-5af11540bbfe.nnue enabled');
    t.after(() => sf16.close());
    await sf16.start();
    assert.deepEqual([sf16.net, sf16.netMemory, sf16.netMemoryError], ['nn-5af11540bbfe.nnue', null, null], 'Stockfish 16 says nothing: not reported');
});

test('a restarted engine reports where its network lives anew', async (t) => {
    const e = fakeEngine('Network replica 1: Shared memory.');
    t.after(() => e.close());
    await e.start();
    e.args = [FAKE];                     // the restarted process says nothing (another build)
    e.proc.kill('SIGKILL');              // a crash
    await e.start();
    assert.deepEqual([e.starts, e.netMemory], [2, null]);
});

test('a search within a node limit says whether the limit stopped it', async (t) => {
    const e = fakeEngine();
    t.after(() => e.close());
    await e.start();
    const sent = [];
    const send = e._send.bind(e);
    e._send = (cmd) => { sent.push(cmd); send(cmd); };
    // The fake engine completes depth 1 in 20 nodes, and never goes deeper.
    assert.equal((await e.analyse(['e2e4'], { depth: 1 })).nodeLimited, false, 'no limit');
    assert.equal((await e.analyse(['e2e4'], { depth: 1, nodes: 1000 })).nodeLimited, false, 'the depth completed within the limit');
    assert.equal((await e.analyse(['e2e4'], { depth: 1, nodes: 20 })).nodeLimited, true, 'the limit reached');
    assert.equal((await e.analyse(['e2e4'], { depth: 5, nodes: 1000 })).nodeLimited, true, 'stopped before the depth was complete');
    assert.deepEqual(sent.filter((c) => c.startsWith('go')), ['go depth 1', 'go depth 1 nodes 1000', 'go depth 1 nodes 20', 'go depth 5 nodes 1000']);
});
