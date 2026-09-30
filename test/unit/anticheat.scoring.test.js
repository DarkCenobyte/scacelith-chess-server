import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Population, scorePlayer, updatePlayerIntegrity, updatePopulationFromGame, bucketOfRating, MODEL, gameZ } from '../../src/anticheat/scoring.js';
import { priorFor, timeClass, PRIOR_GAMES } from '../../src/anticheat/priors.js';
import { rng, syntheticHistory, syntheticSide, learnPopulation, gauss } from '../../src/anticheat/testing/synthetic.js';
import { createFakeStore } from '../../src/anticheat/testing/fake-store.js';
import { openStore, migrate } from '../../src/store/index.js';
import { testConfig } from '../../src/config.js';

const quiet = { debug() {}, info() {}, warn() {}, error() {}, security() {} };

// One learned population shared by the tests (the server's own data after a while).
const learned = learnPopulation(new Population(null), { r: rng(99), count: 30000, categories: ['5+0', '15+10'] });

function firstLevel(hist, pop, level) {
    for (let k = 1; k <= hist.length; k++) if (scorePlayer(hist.slice(0, k), pop).level === level) return k;
    return null;
}

test('priors: sensible ordering by rating and time control', () => {
    assert.ok(priorFor('accuracy', 2400).mean > priorFor('accuracy', 1200).mean);
    assert.ok(priorFor('acpl', 2400).mean < priorFor('acpl', 1200).mean);
    assert.ok(priorFor('accuracy', 1500, 'bullet').mean < priorFor('accuracy', 1500, 'classical').mean);
    assert.equal(timeClass(60000, 0), 'bullet');
    assert.equal(timeClass(180000, 2000), 'blitz');
    assert.equal(timeClass(900000, 10000), 'rapid');
    assert.equal(timeClass(1800000, 20000), 'classical');
    assert.equal(bucketOfRating(1549), 1500);
    assert.equal(bucketOfRating(99), 500);
    assert.equal(bucketOfRating(3400), 2900);
});

test('priors: the accuracy row follows the measured accuracy / ACPL relation from the ACPL row', () => {
    // The relation of priors.js and docs/ANTICHEAT.md section 4: accuracy ~ 100 - 0.28 ACPL up to
    // an ACPL of 90, and the measured values above it at the ACPL of the two lowest ratings. Both
    // quality metrics must describe the same player, or honest players look more or less
    // engine-like on one of them than on the other.
    const measuredAbove90 = new Map([[110, 71], [150, 65]]);
    for (const rating of [600, 1000, 1500, 2000, 2500, 2900]) {
        const acpl = priorFor('acpl', rating).mean, accuracy = priorFor('accuracy', rating).mean;
        const expected = acpl <= 90 ? 100 - 0.28 * acpl : measuredAbove90.get(acpl);
        assert.ok(expected !== undefined, `rating ${rating}: no measured accuracy for an ACPL of ${acpl}`);
        // The table holds whole points.
        assert.ok(Math.abs(accuracy - expected) <= 0.5, `rating ${rating}: accuracy ${accuracy} for an ACPL of ${acpl}, the relation gives ${expected.toFixed(1)}`);
    }
});

test('population: priors blend with Welford data, winsorised, text/array formats accepted', () => {
    const store = createFakeStore({ textColumns: true });
    const pop = new Population(store);
    const p = pop.effective('accuracy', '5+0', 1500);
    assert.equal(p.n, 0);
    assert.ok(Math.abs(p.mean - priorFor('accuracy', 1550, 'blitz').mean) < 1e-9);
    for (let i = 0; i < 200; i++) pop.update('5+0', 1500, { accuracy: 90, acpl: 30, nComplex: 0, t1Complex: 1 });
    const q = new Population(store).effective('accuracy', '5+0', 1500);   // re-read from the store
    assert.equal(q.n, 200);
    assert.ok(Math.abs(q.mean - (PRIOR_GAMES * p.mean + 200 * 90) / (PRIOR_GAMES + 200)) < 1e-6);
    assert.equal(store._.population.get('5+0|1500|accuracy').n, 200, 'one running statistic per metric');
    assert.equal(new Population(store).raw('5+0', 1500).t1Complex, undefined, 't1Complex needs enough complex positions');
    // An absurd value is clipped at 4 sd.
    const pop2 = new Population(null);
    pop2.update('5+0', 1500, { acpl: 100000 });
    const eff = pop2.effective('acpl', '5+0', 1500);
    assert.ok(eff.mean < 200, `winsorised mean ${eff.mean}`);
    // Array rows as another store might return them.
    const pop3 = new Population({ integrity: { populationStats: () => [{ metric: 'accuracy', n: 1000, mean: 70, m2: 999 * 25 }] } });
    const e3 = pop3.effective('accuracy', '5+0', 1500);
    assert.equal(e3.n, 1000);
    assert.ok(e3.mean < 71 && e3.mean > 70);
});

test('population statistics reach the real store and survive a restart', () => {
    const store = openStore(testConfig({ DB_PATH: ':memory:' }));
    migrate(store);
    try {
        const pop = new Population(store);
        const values = [80, 84, 76, 90, 70];
        for (const accuracy of values) pop.update('3+2', 1500, { accuracy, acpl: 40, nComplex: 0 });
        const saved = store.integrity.populationStats('3+2', 1500);
        assert.deepEqual(Object.keys(saved).sort(), ['accuracy', 'acpl']);
        assert.equal(saved.accuracy.n, values.length);
        const m = values.reduce((a, x) => a + x, 0) / values.length;
        assert.ok(Math.abs(saved.accuracy.mean - m) < 1e-9);
        assert.ok(Math.abs(saved.accuracy.m2 - values.reduce((a, x) => a + (x - m) ** 2, 0)) < 1e-9);
        // A new process (or the periodic cache reload) reads the same statistics back.
        const again = new Population(store).raw('3+2', 1500);
        assert.equal(again.accuracy.n, values.length);
        assert.ok(Math.abs(again.accuracy.mean - pop.raw('3+2', 1500).accuracy.mean) < 1e-9);
    } finally {
        store.close();
    }
});

test('population statistics are kept per analysis profile; a player is scored on the games of one profile', () => {
    const store = createFakeStore();
    const [a, b] = ['Stockfish 16; nn-5af11540bbfe.nnue; depth 10/18; hash 32; analysis 1', 'Stockfish 19; nn-1a298aa575a0.nnue; depth 9/16; hash 32; analysis 1'];
    const popA = new Population(store, { profile: a }), popB = new Population(store, { profile: b });
    const f = features(1, { userId: 1, rating: 1500, n: 30, accuracy: 80, acpl: 50 }, { userId: 2, rating: 1500, n: 30, accuracy: 85, acpl: 40 }, { profile: a });
    assert.equal(updatePopulationFromGame(popB, f), 0, 'a game of another profile is refused');
    assert.equal(updatePopulationFromGame(popA, f), 2);
    assert.equal(store._.population.get(`${a}|5+0|1500|accuracy`).n, 2);
    assert.equal(new Population(store, { profile: a }).raw('5+0', 1500).accuracy.n, 2, 're-read from the store');
    assert.deepEqual(popB.raw('5+0', 1500), {}, 'the other profile starts from the priors');
    const hist = syntheticHistory({ r: rng(8), games: 12, userId: 1, rating: 1500, category: '5+0', engineFrom: 0, engine: 1 })
        .map((g, i) => ({ ...g, profile: i < 8 ? a : b }));
    assert.equal(scorePlayer(hist, popA).games, 8);
    assert.equal(scorePlayer(hist, popB).games, 4);
    assert.equal(scorePlayer(hist, new Population(null)).games, 0, 'records of a profile are never scored without it');
});

test('the analysis-profiles migration drops the population statistics written before it', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-profiles-'));
    const migrations = fileURLToPath(new URL('../../src/store/migrations/', import.meta.url));
    const profiles = fs.readdirSync(migrations).find((n) => n.endsWith('_analysis_profiles.sql'));
    const before = path.join(dir, 'migrations');
    fs.mkdirSync(before);
    for (const name of fs.readdirSync(migrations).filter((n) => n < profiles)) fs.copyFileSync(path.join(migrations, name), path.join(before, name));
    const cfg = testConfig({ DB_PATH: path.join(dir, 'x.db') });
    const store = openStore(cfg);
    try {
        migrate(store, { dir: before });
        store.integrity.updatePopulation([{ key: '5+0|1500|accuracy', value: 80 }, { key: '5+0|1500|acpl', value: 50 }]);
        assert.equal(migrate(store).applied[0], Number.parseInt(profiles, 10));
        assert.deepEqual(store.integrity.populationStats('5+0', 1500), {});
        const pop = new Population(store, { profile: 'Stockfish 19; depth 9/16; analysis 1' });
        pop.update('5+0', 1500, { accuracy: 90 });
        assert.equal(new Population(store, { profile: pop.profile }).raw('5+0', 1500).accuracy.n, 1);
    } finally {
        store.close();
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('no flag on small samples, however extreme', () => {
    const perfect = Array.from({ length: 4 }, (_, i) => ({ gameId: i, category: '5+0', rating: 1200, ratingGames: 100, n: 40, nComplex: 15, nTimed: 40,
        accuracy: 100, acpl: 0, t1Deep: 1, t1Fast: 1, t1Complex: 1, timeCorr: -0.5, timeCv: 0.05, endedAt: i }));
    for (const pop of [new Population(null), learned]) {
        const r = scorePlayer(perfect, pop);
        assert.equal(r.level, 'none');
        assert.ok(r.groups.Q > 0);
    }
    // Games with too few scored moves are ignored entirely.
    const tiny = perfect.map((g) => ({ ...g, n: 5 })).concat(perfect.map((g) => ({ ...g, n: 5, gameId: g.gameId + 10 })));
    assert.equal(scorePlayer(tiny, learned).games, 0);
});

test('no flag for strong but consistent honest players (rating-relative)', () => {
    const r = rng(7);
    for (let i = 0; i < 60; i++) {
        const rating = [1100, 1600, 2100, 2500][i % 4];
        // 2.5 between-player sd above peers of the same rating, every game, for 30 games.
        const hist = syntheticHistory({ r, games: 30, userId: i + 1, rating, category: i % 2 ? '5+0' : '15+10', theta: 1.0, timeStyle: 0.5 * gauss(r) });
        for (const pop of [learned, new Population(null)]) {
            const res = scorePlayer(hist, pop);
            assert.equal(res.level, 'none', `rating ${rating}: ${JSON.stringify(res.groups)}`);
        }
    }
});

test('a provisional rating gets the benefit of the doubt (smurf / returning player)', () => {
    // A 1900-strength player whose rating still says 1500.
    const g = syntheticSide({ r: rng(3), userId: 1, rating: 1900, category: '5+0', theta: 0 });
    const est = gameZ({ ...g, rating: 1500, ratingGames: 100 }, learned);
    const prov = gameZ({ ...g, rating: 1500, ratingGames: 5 }, learned);
    // Judged against peers up to 400 points stronger instead of 100.
    assert.ok(prov.zQ < est.zQ - 0.15, `provisional ${prov.zQ} vs established ${est.zQ}`);
    assert.ok(prov.z.accuracy < est.z.accuracy);
});

test('assisted player: flagged only after enough games, then high confidence with relay timing', () => {
    const r = rng(11);
    let flagged = 0;
    for (let i = 0; i < 10; i++) {
        const hist = syntheticHistory({ r, games: 30, userId: 1000 + i, rating: 1500, category: '5+0', engineFrom: 0, engine: 1 });
        const s = firstLevel(hist, learned, 'suspected') ?? firstLevel(hist, learned, 'high_confidence');
        const h = firstLevel(hist, learned, 'high_confidence');
        assert.ok(s === null || s >= MODEL.suspected.minGames);
        assert.ok(h !== null && h >= MODEL.high.minGames, `high at ${h}`);
        const final = scorePlayer(hist, learned);
        assert.equal(final.level, 'high_confidence');
        assert.ok(final.reasons.some((x) => x.startsWith('Move quality')));
        flagged++;
    }
    assert.equal(flagged, 10);
});

test('high_confidence needs independent agreement: engine moves with human timing stay suspected', () => {
    const r = rng(12);
    for (let i = 0; i < 10; i++) {
        const hist = syntheticHistory({ r, games: 30, userId: 2000 + i, rating: 1500, category: '5+0', engineFrom: 0, engine: 1, engineTiming: false });
        for (let k = 1; k <= 30; k++) assert.notEqual(scorePlayer(hist.slice(0, k), learned).level, 'high_confidence');
        assert.equal(scorePlayer(hist, learned).level, 'suspected');
    }
    // Timing alone (a very regular, flat thinker) never flags anyone.
    for (let i = 0; i < 10; i++) {
        const hist = syntheticHistory({ r, games: 30, userId: 3000 + i, rating: 1500, category: '5+0', timeStyle: 3 });
        const res = scorePlayer(hist, learned);
        assert.equal(res.level, 'none');
        assert.ok(res.groups.T > 2, `timing signal ${res.groups.T}`);
    }
});

test('sudden lasting jump is flagged; a single outstanding game or a plausible improvement is not', () => {
    const r = rng(21);
    let flagged = 0;
    for (let i = 0; i < 10; i++) {
        // 20 honest games, then 10 engine games with human-looking timing.
        const hist = syntheticHistory({ r, games: 30, userId: 4000 + i, rating: 1300, category: '5+0', theta: 0.2, engineFrom: 20, engine: 1, engineTiming: false });
        const res = scorePlayer(hist, learned);
        assert.ok(res.jump.lasting);
        assert.ok(res.jump.score > 0);
        // The 30-game window alone is diluted by the honest games...
        assert.ok(res.windows.all.Q.score < MODEL.suspected.accuracyType);
        if (res.level !== 'none') flagged++;
    }
    assert.ok(flagged >= 8, `jump flagged in ${flagged}/10`);
    for (let i = 0; i < 20; i++) {
        const hist = syntheticHistory({ r, games: 30, userId: 5000 + i, rating: 1600, category: '5+0' });
        hist[25] = syntheticSide({ r, userId: 5000 + i, rating: 1600, category: '5+0', engine: 1, gameId: hist[25].gameId, endedAt: hist[25].endedAt });
        const res = scorePlayer(hist, learned);
        assert.equal(res.jump.score, 0);
        assert.equal(res.level, 'none');
        // Honest improvement of 0.75 per-game sd (rating lagging behind).
        const better = syntheticHistory({ r, games: 30, userId: 6000 + i, rating: 1600, category: '5+0', engineFrom: 20, engine: 0 })
            .map((g, k) => (k >= 20 ? syntheticSide({ r, userId: 6000 + i, rating: 1600, category: '5+0', theta: 0.75, gameId: g.gameId, endedAt: g.endedAt }) : g));
        assert.equal(scorePlayer(better, learned).level, 'none');
    }
});

test('honest population: no flags at any sample size (false-positive check)', () => {
    const r = rng(5);
    let flags = 0;
    for (let i = 0; i < 400; i++) {
        const rating = 800 + Math.floor(r() * 1800);
        const hist = syntheticHistory({ r, games: 30, userId: 7000 + i, rating, category: i % 2 ? '5+0' : '15+10', theta: 0.4 * gauss(r), timeStyle: 0.5 * gauss(r) });
        for (const k of [5, 10, 20, 30]) if (scorePlayer(hist.slice(0, k), learned).level !== 'none') flags++;
    }
    assert.equal(flags, 0);
});

function features(gameId, white, black, extra = {}) {
    return { v: 1, gameId, category: '5+0', baseMs: 300000, incMs: 0, endedAt: gameId, analysedAt: gameId, white, black, ...extra };
}

test('updatePlayerIntegrity: stores level, score and evidence with numbers; never touches confirmed', () => {
    const store = createFakeStore();
    const r = rng(33);
    const cheat = store._.addUser('cheater');
    const honest = store._.addUser('honest');
    const other = store._.addUser('confirmedone');
    const hist = syntheticHistory({ r, games: 25, userId: cheat, rating: 1500, category: '5+0', engineFrom: 0, engine: 1 });
    const opp = syntheticHistory({ r, games: 25, userId: honest, rating: 1500, category: '5+0' });
    let last = null;
    for (let i = 0; i < hist.length; i++) {
        const id = 1000 + i;
        store._.addGame({ id, whiteId: cheat, blackId: honest, endedAt: 1_800_000_000_000 + i });
        store.analysis.complete(id, features(id, hist[i], opp[i], { endedAt: 1_800_000_000_000 + i }));
        last = updatePlayerIntegrity({ store, userId: cheat, population: learned, now: 1_900_000_000_000 + i, log: quiet });
        updatePlayerIntegrity({ store, userId: honest, population: learned, now: 1_900_000_000_000 + i, log: quiet });
    }
    assert.equal(last.level, 'high_confidence');
    const rec = store.integrity.get(cheat);
    assert.equal(rec.level, 'high_confidence');
    assert.ok(rec.score >= 3);
    assert.equal(rec.evidence.statistics.games, 25);
    assert.ok(rec.evidence.statistics.trigger.includes('agree'));
    assert.equal(rec.evidence.peak.level, 'high_confidence');
    assert.equal(store.integrity.get(honest).evidence.statistics.reasons, undefined, 'compact evidence for unremarkable players');
    assert.ok(rec.evidence.statistics.reasons.join(' ').match(/accuracy \d+\.\d vs \d+\.\d expected/));
    assert.equal(store.integrity.get(honest).level, 'none');

    store.integrity.set(other, { level: 'confirmed', score: 0, evidence: { certain: [{ kind: 'illegal_move' }] } });
    const res = updatePlayerIntegrity({ store, userId: other, population: learned, log: quiet });
    assert.equal(res.level, 'confirmed');
    assert.equal(store.integrity.get(other).evidence.certain.length, 1);
});

test('high_confidence falls back to suspected, not none, without a moderator; a cleared player needs new evidence', () => {
    const store = createFakeStore();
    const u = store._.addUser('p');
    store.integrity.set(u, { level: 'high_confidence', score: 5, evidence: {} });
    assert.equal(updatePlayerIntegrity({ store, userId: u, population: learned, log: quiet }).level, 'suspected');

    const v = store._.addUser('q');
    const r = rng(44);
    const hist = syntheticHistory({ r, games: 20, userId: v, rating: 1200, category: '5+0', engineFrom: 0, engine: 1, engineTiming: false, startAt: 1000 });
    const opp = syntheticHistory({ r, games: 20, userId: 999, rating: 1200, category: '5+0', startAt: 1000 });
    hist.forEach((g, i) => { store._.addGame({ id: 50 + i, whiteId: v, blackId: 999, endedAt: g.endedAt }); store.analysis.complete(50 + i, features(50 + i, g, opp[i], { endedAt: g.endedAt, analysedAt: g.endedAt })); });
    const score = scorePlayer(hist, learned).score;
    store.integrity.set(v, { level: 'none', score, evidence: { review: { clearedAt: Date.now(), clearedScore: score, by: 'mod' } } });
    const res = updatePlayerIntegrity({ store, userId: v, population: learned, log: quiet });
    assert.equal(res.result.level === 'none' ? 'none' : 'flagged', 'flagged', 'the model alone would flag');
    assert.equal(res.level, 'none', 'but the moderator cleared this evidence');
});

test('suspected has hysteresis: a score just under the threshold keeps the flag', () => {
    const r = rng(45);
    const hist = syntheticHistory({ r, games: 30, userId: 1, rating: 1500, category: '5+0', engineFrom: 0, engine: 0.8, engineTiming: false });
    let k = 5;
    let res = null;
    for (; k <= 30; k++) {
        res = scorePlayer(hist.slice(0, k), learned);
        if (res.level === 'none' && res.groups.accuracyType >= MODEL.suspected.accuracyType - MODEL.suspected.hysteresis) break;
    }
    assert.ok(k <= 30, 'found a history just under the threshold');
    const store = createFakeStore();
    const u = store._.addUser('h');
    const opp = syntheticHistory({ r, games: k, userId: 999, rating: 1500, category: '5+0' });
    hist.slice(0, k).forEach((g, i) => { store._.addGame({ id: 70 + i, whiteId: u, blackId: 999, endedAt: g.endedAt }); store.analysis.complete(70 + i, features(70 + i, { ...g, userId: u }, opp[i], { endedAt: g.endedAt })); });
    store.integrity.set(u, { level: 'suspected', score: 3.6, evidence: {} });
    assert.equal(updatePlayerIntegrity({ store, userId: u, population: learned, log: quiet }).level, 'suspected');
    store.integrity.set(u, { level: 'none', score: 0, evidence: {} });
    assert.equal(updatePlayerIntegrity({ store, userId: u, population: learned, log: quiet }).level, 'none', 'no flag from below the threshold');
});

test('population updates skip flagged players and short games', () => {
    const pop = new Population(null);
    const f = features(1, { userId: 1, rating: 1500, n: 30, accuracy: 80, acpl: 50 }, { userId: 2, rating: 1500, n: 30, accuracy: 99, acpl: 5 });
    assert.equal(updatePopulationFromGame(pop, f, (uid) => (uid === 2 ? 'high_confidence' : 'none')), 1);
    assert.equal(pop.raw('5+0', 1500).accuracy.n, 1);
    assert.equal(pop.raw('5+0', 1500).accuracy.mean, 80);
    const short = features(2, { userId: 1, rating: 1500, n: 4, accuracy: 10 }, { userId: 3, rating: 1500, n: 4, accuracy: 10 });
    assert.equal(updatePopulationFromGame(pop, short), 0);
});
