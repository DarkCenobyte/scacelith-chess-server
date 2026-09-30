import test from 'node:test';
import assert from 'node:assert/strict';
import { testConfig } from '../../src/config.js';
import { createAnalysisWorker } from '../../src/anticheat/analysis/worker.js';
import { EngineError } from '../../src/anticheat/analysis/engine.js';
import { createFakeStore } from '../../src/anticheat/testing/fake-store.js';
import { AnalysisPriority } from '../../src/store/index.js';
import { MODEL } from '../../src/anticheat/scoring.js';
import { metrics } from '../../src/metrics.js';

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

function addGame(store, id, plies = 40, priority = AnalysisPriority.ordinary, players = {}) {
    const w = players.w ?? store._.addUser(`w${id}`), b = players.b ?? store._.addUser(`b${id}`);
    const moves = Array.from({ length: plies }, (_, i) => (i % 64) | (((i + 9) % 64) << 6));
    store._.addGame({ id, category: '5+0', rated: true, baseMs: 300000, incMs: 0, whiteId: w, blackId: b, whiteRating: 1500, blackRating: 1500,
        endedAt: 1_800_000_000_000 + id, moves: Uint16Array.from(moves), spentMs: Uint32Array.from(moves.map((_, i) => 1000 + (i * 37) % 900)) });
    store._.enqueue(id, priority);
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
    assert.equal(store.integrity.get(b).evidence.statistics.games, 1);
    assert.equal(f.profile, 'fake; depth 4/8; analysis 2');
    assert.equal(store.integrity.get(b).evidence.statistics.profile, f.profile);
    assert.equal(store._.population.get(`${f.profile}|5+0|1500|accuracy`).n, 2);
    assert.equal(worker.stats.analysed, 1);
});

test('the rating evidence of a side is its counted games once rated, 0 while unrated, whatever the games played', async () => {
    const store = createFakeStore();
    const { moveToUci } = await import('../../src/anticheat/analysis/moves.js');
    const cases = [
        [{ games: 60, rated: false, countedGames: 3, unratedGames: 3 }, 0],      // many games, never rated
        [{ games: 50, rated: true, countedGames: 12 }, 12],                      // the unrated phase's losses did not count
        [{ games: 45, rated: true, countedGames: 45 }, 45],
        [{ games: 40 }, 40],                                                    // stored before `rated` existed: rated, all counted
        [{ games: 0 }, 0],
    ];
    const worker = createAnalysisWorker({ config, store, engineFactory: () => null, workerId: 't' });
    for (const [i, [rating, expected]] of cases.entries()) {
        const { w, b, moves } = addGame(store, 71 + i);
        store._.setRating(w, '5+0', rating);
        store._.setRating(b, '5+0', { games: 60, rated: true, countedGames: 60 });
        const engine = fakeEngine((ply) => (ply < moves.length ? moveToUci(moves[ply]) : null));
        const f = await worker.processJob(engine, store.analysis.next(1, 't', Date.now())[0]);
        assert.deepEqual([f.white.ratingGames, f.black.ratingGames], [expected, 60], JSON.stringify(rating));
    }
});

test('only games claimed at ordinary priority (the random sample) feed the population statistics', async () => {
    const store = createFakeStore();
    const P = AnalysisPriority;
    // Flagged, reported and moderator-requested games are analysed first and in full: counted in
    // the population, they would shift the baseline towards the suspects it judges.
    const flagged = [addGame(store, 41, 40, P.signal), addGame(store, 42, 40, P.report), addGame(store, 43, 40, P.manual)];
    const { moveToUci } = await import('../../src/anticheat/analysis/moves.js');
    const engine = fakeEngine((ply) => (ply < flagged[0].moves.length ? moveToUci(flagged[0].moves[ply]) : null));
    const worker = createAnalysisWorker({ config, store, engineFactory: () => engine, workerId: 't' });
    for (let i = 0; i < 3; i++) {
        const job = store.analysis.next(1, 't', Date.now())[0];
        assert.notEqual(job.priority, P.ordinary);
        assert.ok(await worker.processJob(engine, job));
    }
    assert.equal(store._.population.size, 0, 'no population update from prioritized games');
    for (const g of flagged) assert.equal(store.integrity.get(g.b).evidence.statistics.games, 1, 'the players are scored all the same');
    // A job without a priority (another caller) is not a sample either.
    const other = addGame(store, 44);
    store._.jobs.get(44).status = 'running';
    assert.ok(await worker.processJob(engine, { gameId: 44 }));
    assert.equal(store._.population.size, 0);
    assert.equal(store.integrity.get(other.w).evidence.statistics.games, 1);
    // An ordinary job does.
    addGame(store, 45);
    const job = store.analysis.next(1, 't', Date.now())[0];
    assert.deepEqual([job.gameId, job.priority], [45, P.ordinary]);
    const f = await worker.processJob(engine, job);
    assert.equal(store._.population.get(`${f.profile}|5+0|1500|accuracy`).n, 2);
});

test('another engine restarts the statistics: its own population, players scored on its games, levels kept until judged', async () => {
    const store = createFakeStore();
    const first = addGame(store, 61);
    const players = { w: first.w, b: first.b };
    const { moveToUci } = await import('../../src/anticheat/analysis/moves.js');
    const best = (ply) => (ply < first.moves.length ? moveToUci(first.moves[ply]) : null);
    const before = fakeEngine(best);
    const after = { ...fakeEngine(best), name: 'fake 2' };
    const worker = createAnalysisWorker({ config, store, engineFactory: () => before, workerId: 't' });
    const old = await worker.processJob(before, store.analysis.next(1, 't', Date.now())[0]);
    // A level reached with the earlier engine (the model would not flag these few games).
    store.integrity.set(first.w, { level: 'suspected', score: 3.6, evidence: {} });
    for (let i = 1; i <= MODEL.suspected.minGames; i++) {
        addGame(store, 61 + i, 40, AnalysisPriority.ordinary, players);
        const f = await worker.processJob(after, store.analysis.next(1, 't', Date.now())[0]);
        assert.notEqual(f.profile, old.profile);
        const st = store.integrity.get(first.w).evidence.statistics;
        assert.deepEqual([st.profile, st.games], [f.profile, i], 'scored on the games of the new profile only');
        assert.equal(store.integrity.get(first.w).level, i < MODEL.suspected.minGames ? 'suspected' : 'none', `after ${i} games of the new profile`);
        assert.equal(store._.population.get(`${f.profile}|5+0|1500|accuracy`).n, 2 * i);
    }
    assert.equal(store._.population.get(`${old.profile}|5+0|1500|accuracy`).n, 2, 'the earlier population is left as it was');
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

test('every engine start is logged with where its network lives, and the gauges count the engines that share it', async () => {
    const gauge = (name) => metrics.snapshot().find((m) => m.name === name).children[0].v;
    const run = async (workers, reports) => {
        const store = createFakeStore();
        const logs = [];
        const log = { info: (msg, f) => logs.push(['info', msg, f]), warn: (msg, f) => logs.push(['warn', msg, f]), error() {} };
        // Engines as UciEngine leaves them after a start (engine.js): alive, with their report.
        const engines = reports.map(([netMemory, netMemoryError]) => ({ ...fakeEngine(() => null), name: 'Stockfish 19', net: 'nn-1a298aa575a0.nnue',
            alive: true, starts: 1, netMemory, netMemoryError }));
        const worker = createAnalysisWorker({ config: { ...config, analysisWorkers: workers }, store, log, engineFactory: (i) => engines[i], pollMs: 10 });
        const running = worker.run();
        const t0 = Date.now();
        while (logs.filter((l) => l[1] !== 'analysis worker started').length < workers && Date.now() - t0 < 5000) await new Promise((r) => setTimeout(r, 5));
        const counts = [gauge('scacelith_anticheat_analysis_engines'), gauge('scacelith_anticheat_analysis_engines_shared')];
        engines[0].starts = 2;                                   // restarted: reported once more
        while (logs.filter((l) => l[1] !== 'analysis worker started').length < workers + 1 && Date.now() - t0 < 5000) await new Promise((r) => setTimeout(r, 5));
        await worker.stop();
        await running;
        return { logs: logs.filter((l) => l[1] !== 'analysis worker started'), counts };
    };

    const shared = await run(2, [['shared', null], ['shared', null]]);
    assert.deepEqual(shared.counts, [2, 2]);
    assert.deepEqual(shared.logs.map((l) => [l[0], l[1], l[2].network]), Array(3).fill(['info', 'analysis engine started', 'shared memory']));
    assert.deepEqual(shared.logs.map((l) => l[2].engine).sort(), [0, 0, 1], 'once per start of each engine');

    // Stockfish 19 without sharing while two engines run: a warning with the engine's reason.
    const local = await run(2, [['local', 'Shared memory is not serving to other processes'], ['shared', null]]);
    assert.deepEqual(local.counts, [2, 1]);
    const warn = local.logs.find((l) => l[0] === 'warn');
    assert.equal(warn[1], 'analysis engine started with its own copy of the network');
    assert.deepEqual([warn[2].network, warn[2].why, warn[2].engines], ['local memory', 'Shared memory is not serving to other processes', 2]);

    // Alone, a copy of its own costs nothing more; an engine that says nothing (Stockfish 16).
    const alone = await run(1, [['local', 'why']]);
    assert.deepEqual(alone.logs.map((l) => [l[0], l[2].network]), [['info', 'local memory'], ['info', 'local memory']]);
    const sf16 = await run(1, [[null, null]]);
    assert.deepEqual(sf16.counts, [1, 0]);
    assert.deepEqual(sf16.logs.map((l) => [l[0], l[2].network]), [['info', 'not reported'], ['info', 'not reported']]);
    assert.deepEqual([gauge('scacelith_anticheat_analysis_engines'), gauge('scacelith_anticheat_analysis_engines_shared')], [0, 0], 'stopped workers count no engine');
});
