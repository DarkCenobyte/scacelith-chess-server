import test from 'node:test';
import assert from 'node:assert/strict';
import { testConfig } from '../../src/config.js';
import { createAnalysisWorker } from '../../src/anticheat/analysis/worker.js';
import { EngineError } from '../../src/anticheat/analysis/engine.js';
import { createFakeStore } from '../../src/anticheat/testing/fake-store.js';

const config = testConfig({ ANALYSIS_ENGINE_PATH: '/fake/engine', ANALYSIS_DEPTH_FAST: '4', ANALYSIS_DEPTH_DEEP: '8', ANALYSIS_POLL_MS: '100' });

// Engine answering every position with three lines; the played move is always the best one.
function fakeEngine(uciOf) {
    return {
        name: 'fake', started: 0, closed: false,
        async start() { this.started++; },
        async newGame() {}, async clearHash() {},
        async analyse(moves, { multiPv }) {
            const best = uciOf(moves.length) || null;
            const lines = [{ multipv: 1, depth: 8, cp: 20, mate: null, bound: null, move: best, pv: [best] }];
            if (multiPv > 1) lines.push({ multipv: 2, depth: 8, cp: -40, mate: null, bound: null, move: 'a1a1', pv: ['a1a1'] }, { multipv: 3, depth: 8, cp: -90, mate: null, bound: null, move: 'b1b1', pv: ['b1b1'] });
            return { lines, bestmove: best };
        },
        async close() { this.closed = true; },
    };
}

function addGame(store, id, plies = 40) {
    const w = store._.addUser(`w${id}`), b = store._.addUser(`b${id}`);
    const moves = Array.from({ length: plies }, (_, i) => (i % 64) | (((i + 9) % 64) << 6));
    store._.addGame({ id, category: '5+0', rated: true, baseMs: 300000, incMs: 0, whiteId: w, blackId: b, whiteRating: 1500, blackRating: 1500,
        endedAt: 1_800_000_000_000 + id, moves: Uint16Array.from(moves), spentMs: Uint32Array.from(moves.map((_, i) => 1000 + (i * 37) % 900)) });
    store._.enqueue(id);
    return { w, b, moves };
}

test('processJob: features stored, both players scored, population updated', async () => {
    const store = createFakeStore();
    const { w, b, moves } = addGame(store, 11);
    const { moveToUci } = await import('../../src/anticheat/analysis/moves.js');
    const engine = fakeEngine((ply) => (ply < moves.length ? moveToUci(moves[ply]) : null));
    const worker = createAnalysisWorker({ config, store, engineFactory: () => engine, workerId: 't' });
    const job = store.analysis.next(1, 't', Date.now())[0];
    const f = await worker.processJob(engine, job);
    assert.equal(f.gameId, 11);
    assert.equal(f.white.n, 12);             // plies 16..39, white's half
    assert.equal(f.white.t1Deep, 1);
    assert.equal(f.white.ratingGames, 0);
    assert.equal(store._.jobs.get(11).status, 'done');
    assert.equal(store.integrity.get(w).level, 'none');
    assert.equal(store.integrity.get(b).evidence.statistics.perGame.length, 1);
    assert.ok(store._.population.get('5+0|1500').metrics.accuracy.n === 2);
    assert.equal(worker.stats.analysed, 1);
});

test('a job whose game is missing, or whose engine crashes, is marked failed', async () => {
    const store = createFakeStore();
    store._.enqueue(5);
    const engine = fakeEngine(() => 'e2e4');
    const worker = createAnalysisWorker({ config, store, engineFactory: () => engine });
    assert.equal(await worker.processJob(engine, { gameId: 5 }), null);
    assert.equal(store._.jobs.get(5).status, 'failed');
    assert.match(store._.jobs.get(5).error, /not found/);
    addGame(store, 6);
    const crashing = { ...fakeEngine(() => 'e2e4'), async analyse() { throw new EngineError('crashed', 'engine exited'); } };
    assert.equal(await worker.processJob(crashing, store.analysis.next(1, 'x', 0)[0]), null);
    assert.match(store._.jobs.get(6).error, /engine crashed/);
    assert.equal(worker.stats.failed, 2);
});

test('an engine that cannot start never claims jobs; stop() ends the loop', async () => {
    const store = createFakeStore();
    addGame(store, 7);
    let starts = 0;
    const broken = { name: 'x', async start() { starts++; throw new EngineError('spawn', 'ENOENT'); }, async close() {} };
    const errors = [];
    const worker = createAnalysisWorker({ config, store, engineFactory: () => broken, log: { info() {}, warn() {}, error: (m) => errors.push(m) } });
    const run = worker.run();
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(store._.jobs.get(7).status, 'queued', 'the job stays in the queue');
    assert.ok(starts >= 1);
    assert.ok(errors.includes('analysis engine unavailable'));
    await worker.stop();
    await run;
});

test('the run loop analyses queued games and idles when the queue is empty', async () => {
    const store = createFakeStore();
    const g1 = addGame(store, 21);
    addGame(store, 22);
    const { moveToUci } = await import('../../src/anticheat/analysis/moves.js');
    const engine = fakeEngine((ply) => (ply < g1.moves.length ? moveToUci(g1.moves[ply]) : null));
    const worker = createAnalysisWorker({ config, store, engineFactory: () => engine });
    const run = worker.run();
    const t0 = Date.now();
    while (worker.stats.analysed < 2 && Date.now() - t0 < 5000) await new Promise((r) => setTimeout(r, 10));
    assert.equal(worker.stats.analysed, 2);
    await worker.stop();
    await run;
    assert.equal(engine.closed, true);
});
