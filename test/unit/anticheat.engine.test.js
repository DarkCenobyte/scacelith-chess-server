// End-to-end validation of the engine analysis and the scoring model with a real UCI engine
// (Stockfish). Skipped when no engine is installed. Set SCACELITH_TEST_ENGINE to use another
// binary. Small depths keep it under a minute on 4 cores.
//
//   (a) an assisted player: plays the engine's best move at depth 12 every time, relayed with a
//       think time unrelated to the position, against a decent human stand-in;
//   (b) weak human stand-ins: uniform picks among the top-4 moves at depth 5 (within 200 cp),
//       thinking longer when there are several good moves.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { UciEngine } from '../../src/anticheat/analysis/engine.js';
import { analyseGame } from '../../src/anticheat/analysis/analyzer.js';
import { Population, scorePlayer, sideOf, updatePopulationFromGame, MODEL } from '../../src/anticheat/scoring.js';
import { createAnalysisWorker } from '../../src/anticheat/analysis/worker.js';
import { createFakeStore } from '../../src/anticheat/testing/fake-store.js';
import { playGame } from '../../src/anticheat/testing/engine-games.js';
import { rng } from '../../src/anticheat/testing/synthetic.js';
import { mean } from '../../src/anticheat/analysis/stats.js';
import { testConfig } from '../../src/config.js';

const ENGINE = process.env.SCACELITH_TEST_ENGINE || ['/usr/games/stockfish', '/usr/bin/stockfish', '/usr/local/bin/stockfish'].find((p) => fs.existsSync(p));
const skip = ENGINE ? false : 'no UCI engine installed (set SCACELITH_TEST_ENGINE)';

const DEPTH_FAST = 6, DEPTH_DEEP = 10, RATING = 1200, CATEGORY = '15+10';
const ASSISTED = { kind: 'assisted', assistDepth: 12 };
const DECENT = { kind: 'plausible', depth: 8, topN: 3, plausibleCp: 60 };
const WEAK = { kind: 'plausible', depth: 5, topN: 4, plausibleCp: 200 };
const CHEATER = 100;
const HONEST = [201, 202, 203, 204];
const ASSISTED_GAMES = 18, HUMAN_GAMES = 20, PIPELINES = 3;

async function runPool(jobs, fn) {
    const pipes = Array.from({ length: PIPELINES }, () => ({
        gen: new UciEngine({ path: ENGINE, hashMb: 16 }),
        ana: new UciEngine({ path: ENGINE, hashMb: 16 }),
    }));
    const out = new Array(jobs.length);
    let next = 0;
    try {
        await Promise.all(pipes.map(async (p) => {
            while (next < jobs.length) {
                const i = next++;
                out[i] = await fn(p, jobs[i]);
            }
        }));
    } finally {
        await Promise.all(pipes.flatMap((p) => [p.gen.close(), p.ana.close()]));
    }
    return out;
}

test('real engine: features separate an assisted player from human stand-ins, scoring flags only the former', { skip, timeout: 170000 }, async (t) => {
    const jobs = [];
    for (let i = 0; i < ASSISTED_GAMES; i++) {
        const cheaterWhite = i % 2 === 0;
        jobs.push({ id: 1 + i, seed: 1000 + i, white: cheaterWhite ? ASSISTED : DECENT, black: cheaterWhite ? DECENT : ASSISTED,
            whiteId: cheaterWhite ? CHEATER : 300 + i, blackId: cheaterWhite ? 300 + i : CHEATER });
    }
    for (let i = 0; i < HUMAN_GAMES; i++) {
        jobs.push({ id: 100 + i, seed: 2000 + i, white: WEAK, black: WEAK, whiteId: HONEST[i % 4], blackId: HONEST[(i + 1) % 4] });
    }
    const started = Date.now();
    const results = await runPool(jobs, async (p, job) => {
        const g = await playGame(p.gen, { r: rng(job.seed), white: job.white, black: job.black, maxPlies: 90 });
        const record = { id: job.id, category: CATEGORY, rated: true, baseMs: 900000, incMs: 10000, whiteId: job.whiteId, blackId: job.blackId,
            whiteRating: RATING, blackRating: RATING, endedAt: 1_800_000_000_000 + job.id * 60000,
            moves: Uint16Array.from(g.moves), spentMs: Uint32Array.from(g.spentMs) };
        return { record, features: await analyseGame(p.ana, record, { depthFast: DEPTH_FAST, depthDeep: DEPTH_DEEP, extra: { white: { ratingGames: 100 }, black: { ratingGames: 100 } } }) };
    });
    t.diagnostic(`generated and analysed ${jobs.length} games in ${Date.now() - started} ms`);
    // One engine, one set of depths: one analysis profile, the one the populations below hold.
    const profile = results[0].features.profile;
    assert.ok(results.every((r) => r.features.profile === profile), profile);
    t.diagnostic(`profile: ${profile}`);

    const cheat = results.filter((r) => r.record.whiteId === CHEATER || r.record.blackId === CHEATER).map((r) => sideOf(r.features, CHEATER));
    const human = [];
    for (const r of results.filter((x) => x.record.id >= 100)) for (const uid of [r.record.whiteId, r.record.blackId]) human.push(sideOf(r.features, uid));
    const agg = (xs, k) => mean(xs.filter((x) => x[k] !== null && x.n > 0).map((x) => x[k]));
    const summary = (xs) => ({ games: xs.length, moves: xs.reduce((a, x) => a + x.n, 0), accuracy: agg(xs, 'accuracy').toFixed(1), acpl: agg(xs, 'acpl').toFixed(1),
        t1Deep: agg(xs, 't1Deep').toFixed(2), t1Fast: agg(xs, 't1Fast').toFixed(2), t1Complex: agg(xs, 't1Complex').toFixed(2),
        timeCorr: agg(xs, 'timeCorr').toFixed(2), timeCv: agg(xs, 'timeCv').toFixed(2) });
    t.diagnostic(`assisted: ${JSON.stringify(summary(cheat))}`);
    t.diagnostic(`humans:   ${JSON.stringify(summary(human))}`);

    // 1. The per-game features separate the two groups.
    assert.ok(cheat.every((s) => s.n >= MODEL.minMovesPerGame), 'every assisted game has scored moves');
    assert.ok(agg(cheat, 'accuracy') > agg(human, 'accuracy') + 8);
    assert.ok(agg(cheat, 'acpl') < 0.4 * agg(human, 'acpl'));
    assert.ok(agg(cheat, 't1Deep') > agg(human, 't1Deep') + 0.2);
    assert.ok(agg(cheat, 'timeCv') < agg(human, 'timeCv') - 0.3);
    assert.ok(agg(cheat, 'timeCorr') < agg(human, 'timeCorr'));
    const minCheatAcc = Math.min(...cheat.map((s) => s.accuracy));
    const medianHumanAcc = [...human.map((s) => s.accuracy)].sort((a, b) => a - b)[human.length >> 1];
    assert.ok(minCheatAcc > medianHumanAcc, `worst assisted game ${minCheatAcc} vs median human ${medianHumanAcc}`);

    // 2. Scoring on a fresh server (priors only): the humans are never flagged.
    const fresh = new Population(null, { profile });
    for (const uid of HONEST) {
        const hist = human.filter((s) => s.userId === uid);
        for (let k = 1; k <= hist.length; k++) assert.equal(scorePlayer(hist.slice(0, k), fresh).level, 'none', `human ${uid} after ${k} games`);
    }
    const dayOne = scorePlayer(cheat, fresh);
    t.diagnostic(`assisted player on a fresh server after ${cheat.length} games: ${dayOne.level} ${JSON.stringify(dayOne.groups)}`);

    // 3. With the server's own population (the human games at this rating), the assisted player
    //    is flagged, but only once enough games exist; the humans still are not.
    // (Games are only played at one rating here; they stand for the neighbouring buckets too,
    // since z-scores take the most favourable bucket of the rating band.)
    const pop = new Population(null, { profile });
    for (const r of results.filter((x) => x.record.id >= 100)) {
        for (const d of [-MODEL.ratingBand.established, 0, MODEL.ratingBand.established]) {
            const f = r.features;
            updatePopulationFromGame(pop, { ...f, white: { ...f.white, rating: RATING + d }, black: { ...f.black, rating: RATING + d } });
        }
    }
    for (const uid of HONEST) {
        const hist = human.filter((s) => s.userId === uid);
        for (let k = 1; k <= hist.length; k++) assert.equal(scorePlayer(hist.slice(0, k), pop).level, 'none', `human ${uid} after ${k} games (learned)`);
    }
    const chrono = [...cheat].sort((a, b) => a.endedAt - b.endedAt);
    const levels = chrono.map((_, k) => scorePlayer(chrono.slice(0, k + 1), pop));
    t.diagnostic(`assisted player, learned population: ${levels.map((l) => `${l.level[0]}${l.score.toFixed(1)}`).join(' ')}`);
    for (let k = 0; k < MODEL.suspected.minGames - 1; k++) assert.equal(levels[k].level, 'none', `not flagged after ${k + 1} games`);
    const final = levels[levels.length - 1];
    assert.notEqual(final.level, 'none', `flagged after ${chrono.length} games: ${JSON.stringify(final.groups)}`);
    assert.ok(final.reasons.length >= 3);
});

test('real engine: the worker analyses a queued game end to end', { skip, timeout: 60000 }, async () => {
    const store = createFakeStore();
    const config = testConfig({ ANALYSIS_ENGINE_PATH: ENGINE, ANALYSIS_DEPTH_FAST: '4', ANALYSIS_DEPTH_DEEP: '8', ANALYSIS_POLL_MS: '100' });
    const gen = new UciEngine({ path: ENGINE });
    let g;
    // 80 plies: the weak stand-ins' games are often decided early, and the moves of a decided
    // position are not scored; at 40 plies (at most 12 scored moves per side) a lopsided game
    // could leave both sides under the population's 10 scored moves.
    try { g = await playGame(gen, { r: rng(5), white: WEAK, black: WEAK, maxPlies: 80 }); } finally { await gen.close(); }
    const w = store._.addUser('white'), b = store._.addUser('black');
    store._.addGame({ id: 9001, category: '5+0', rated: true, baseMs: 300000, incMs: 0, whiteId: w, blackId: b, whiteRating: 1500, blackRating: 1500,
        endedAt: Date.now(), moves: Buffer.from(Uint16Array.from(g.moves).buffer), spentMs: Buffer.from(Uint32Array.from(g.spentMs).buffer) });
    store._.enqueue(9001);
    store._.enqueue(9002);   // a job whose game vanished
    const worker = createAnalysisWorker({ config, store, log: null, workerId: 'test' });
    const run = worker.run();
    const t0 = Date.now();
    while (worker.stats.analysed + worker.stats.failed < 2 && Date.now() - t0 < 50000) await new Promise((r) => setTimeout(r, 50));
    await worker.stop();
    await run;
    const job = store._.jobs.get(9001);
    assert.equal(job.status, 'done');
    assert.equal(job.features.plies, g.moves.length);
    assert.ok(job.features.white.n > 0 && job.features.engine.startsWith('Stockfish'));
    assert.match(job.features.profile, /; nn-[0-9a-f]{12}\.nnue; depth 4\/8; hash 32; analysis 2$/, 'the profile names the network');
    assert.equal(store._.jobs.get(9002).status, 'failed');
    assert.ok(store.integrity.get(w), 'integrity computed for both players');
    assert.equal(store.integrity.get(w).level, 'none');
    assert.ok(store._.population.size >= 1, 'population statistics updated');
});
