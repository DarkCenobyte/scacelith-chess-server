// Engine analysis backlog policy (store/index.js header, docs/DESIGN.md 6.5): ordinary games are
// sampled and capped by ANALYSIS_QUEUE_MAX, games with a suspicion signal, a report or a
// moderator request are always queued and taken first.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { openStore, migrate, AnalysisPriority, StoreError } from '../../src/store/index.js';
import { startStoreWriter } from '../../src/store/writer.js';
import { testConfig } from '../../src/config.js';
import { metrics } from '../../src/metrics.js';
import { enums } from '../../src/protocol/schema.js';
import { handleReport } from '../../src/anticheat/reports.js';
import { DatabaseSync } from 'node:sqlite';
import { fileURLToPath } from 'node:url';

const { GameStatus, EndReason } = enums;
const DAY = 86400000;

function applyGame(white, black, score) {
    const side = (r, s) => ({ before: r.rating, after: r.rating + (s - 0.5) * 20 });
    return { white: side(white, score), black: side(black, 1 - score) };
}

function setup(overrides = {}, opts = {}) {
    const store = openStore(testConfig({ DB_PATH: ':memory:', ANALYSIS_MIN_PLIES: '20', ...overrides }), { applyGame, ...opts });
    migrate(store);
    const ids = ['Ann', 'Ben', 'Cid', 'Dee', 'Eve', 'Fay'].map((n) => store.users.create({ username: n, email: `${n}@example.org`, createdAt: Date.now() - 400 * DAY }));
    return { store, ids };
}

let nextId = 5_000_000_000_000;
function record(white, black, extra = {}) {
    const plies = extra.plies ?? 30;
    const now = Date.now();
    return {
        id: ++nextId, category: '5+0', rated: true, baseMs: 300000, incMs: 0, whiteId: white, blackId: black, whiteName: 'W',
        blackName: 'B', startedAt: now - 600000, endedAt: now, status: GameStatus.WhiteWins, reason: EndReason.Resignation,
        moves: new Uint16Array(plies), spentMs: new Uint32Array(plies), clockMs: new Uint32Array(plies), ...extra,
    };
}

const skippedMetric = (reason) => metrics.metrics.get('scacelith_anticheat_analysis_skipped_total')?.children.get(reason)?.value ?? 0;
const claimAll = (store) => store.analysis.next(100, 'w', Date.now()).map((j) => [j.gameId, j.priority]);

test('ANALYSIS_QUEUE_MAX caps the ordinary games waiting; a claimed job makes room', () => {
    const { store, ids: [a, b] } = setup({ ANALYSIS_QUEUE_MAX: '3' });
    const before = skippedMetric('backlog');
    const games = Array.from({ length: 5 }, () => record(a, b));
    const res = store.games.finishBatch(games);
    assert.deepEqual(res.map((r) => r.analysisSkipped ?? null), [null, null, null, 'backlog', 'backlog']);
    assert.ok(res.every((r) => r.ratings), 'the skipped games are rated and stored all the same');
    assert.equal(skippedMetric('backlog') - before, 2);
    assert.deepEqual(store.analysis.backlog(), { ordinary: 3, priority: 0 });
    // The next batch (its own transaction) counts again: still full.
    assert.equal(store.games.finishBatch([record(a, b)])[0].analysisSkipped, 'backlog');
    // A job taken by the engine leaves the queue: one more game fits, then it is full again.
    assert.equal(store.analysis.next(1, 'w', Date.now())[0].gameId, games[0].id);
    const more = store.games.finishBatch([record(a, b), record(a, b)]);
    assert.deepEqual(more.map((r) => r.analysisSkipped ?? null), [null, 'backlog']);
    assert.deepEqual(store.analysis.stats(), { queued: 3, running: 1, done: 0, failed: 0 });
    // Games the automatic analysis never takes are not "skipped".
    const casual = store.games.finishBatch([record(a, b, { rated: false }), record(a, b, { plies: 19 })]);
    assert.deepEqual(casual.map((r) => r.analysisSkipped ?? null), [null, null]);
    store.close();
});

test('ANALYSIS_QUEUE_MAX=0 queues only prioritized games', () => {
    const { store, ids: [a, b, c] } = setup({ ANALYSIS_QUEUE_MAX: '0' });
    store.integrity.set(c, { level: 'suspected', score: 3 });
    const res = store.games.finishBatch([record(a, b), record(c, a)]);
    assert.deepEqual(res.map((r) => r.analysisSkipped ?? null), ['backlog', null]);
    assert.deepEqual(store.analysis.backlog(), { ordinary: 0, priority: 1 });
    store.close();
});

test('priority: moderator request, then reported games, then suspicion signals, then ordinary games (oldest first)', () => {
    const { store, ids: [a, b, c] } = setup({ ANALYSIS_QUEUE_MAX: '2' });
    const [o1, o2, skipped, analysed] = [record(a, b), record(b, a), record(a, b), record(b, a)];
    store.games.finishBatch([analysed]);
    store.analysis.complete(store.analysis.next(1, 'w', Date.now())[0].gameId, { gameId: analysed.id });
    const res = store.games.finishBatch([o1, o2, skipped]);
    assert.equal(res[2].analysisSkipped, 'backlog');
    // A suspected player: their new games are queued whatever the backlog, ahead of the ordinary ones.
    store.integrity.set(c, { level: 'suspected', score: 3 });
    const flagged = record(c, a);
    assert.equal(store.games.finishBatch([flagged])[0].analysisSkipped, undefined);
    // A report on the skipped game queues it ahead of the flagged one; a moderator request on the
    // analysed game re-analyses it before everything else.
    assert.equal(store.analysis.request(skipped.id, 'report'), true);
    store.analysis.enqueue(analysed.id);
    assert.deepEqual(store.analysis.backlog(), { ordinary: 2, priority: 3 });
    const P = AnalysisPriority;
    assert.deepEqual(claimAll(store), [
        [analysed.id, P.manual], [skipped.id, P.report], [flagged.id, P.signal], [o1.id, P.ordinary], [o2.id, P.ordinary],
    ]);
    store.close();
});

test('the claim order holds across several claims and the job carries its priority', () => {
    const { store, ids: [a, b, c] } = setup();
    const games = [record(a, b), record(a, b), record(a, b)];
    store.games.finishBatch(games);
    store.integrity.set(c, { level: 'high_confidence', score: 5 });
    const late = record(b, c);
    store.games.finishBatch([late]);
    const t = Date.now();
    assert.deepEqual(store.analysis.next(1, 'w', t).map((j) => [j.gameId, j.priority]), [[late.id, AnalysisPriority.signal]]);
    assert.deepEqual(store.analysis.next(2, 'w', t).map((j) => j.gameId), [games[0].id, games[1].id]);
    store.close();
});

test('suspicion signals at the end of a game: integrity level, open reports, anomalies of the game', () => {
    const { store, ids: [a, b, c, d, e, f] } = setup({ ANALYSIS_QUEUE_MAX: '0' });
    const now = Date.now();
    const queued = (g) => store.games.finishBatch([g])[0].analysisSkipped === undefined;
    // An earlier game between the reporter and the reported player (reports reference games).
    const earlier = record(a, b);
    store.games.finishBatch([earlier]);
    // Open cheating report with weight, within 30 days: the reported player's next games are flagged.
    store.reports.create({ reporterId: a, reportedId: b, gameId: earlier.id, category: 'cheating', weight: 0.8, at: now - 2 * DAY });
    assert.equal(queued(record(b, c)), true);
    // Not a signal: an abuse report, reports of low credibility (new accounts, brigading cap), a
    // dismissed one, an old one.
    store.reports.create({ reporterId: a, reportedId: c, gameId: earlier.id, category: 'abuse', weight: 1, at: now - DAY });
    store.reports.create({ reporterId: b, reportedId: c, gameId: earlier.id, category: 'cheating', weight: 0, at: now - DAY });
    store.reports.create({ reporterId: f, reportedId: c, gameId: earlier.id, category: 'cheating', weight: 0.49, at: now - DAY });
    const dismissed = store.reports.create({ reporterId: d, reportedId: c, gameId: earlier.id, category: 'other', weight: 1, at: now - DAY });
    store.reports.resolve(dismissed, 'dismissed', 'mod');
    store.reports.create({ reporterId: e, reportedId: c, gameId: earlier.id, category: 'cheating', weight: 1, at: now - 31 * DAY });
    assert.equal(queued(record(c, d)), false);
    // A suspicious anomaly recorded in this game flags it; an info one or one of another game does not.
    const g1 = record(d, e);
    store.anomalies.insertBatch([
        { userId: d, gameId: g1.id, kind: 'desync', severity: 'info', at: now - 1000 },
        { userId: e, gameId: 12345, kind: 'clock_implausible', severity: 'suspicious', at: now - 1000 },
    ]);
    assert.equal(queued(g1), false);
    const g2 = record(e, f);
    store.anomalies.insertBatch([{ userId: f, gameId: g2.id, kind: 'clock_implausible', severity: 'suspicious', at: now - 1000 }]);
    assert.equal(queued(g2), true);
    // Any integrity level above 'none' (here confirmed, after a certain cheat and the ban).
    store.integrity.set(a, { level: 'confirmed' });
    assert.equal(queued(record(d, a)), true);
    store.close();
});

test('ANALYSIS_SAMPLE_RATE draws ordinary games only; flagged games are never sampled out', () => {
    const draws = [0.1, 0.7, 0.49, 0.5];
    let calls = 0;
    const { store, ids: [a, b, c] } = setup({ ANALYSIS_SAMPLE_RATE: '0.5' }, { random: () => draws[calls++ % draws.length] });
    const before = skippedMetric('sample');
    const res = store.games.finishBatch([record(a, b), record(a, b), record(a, b), record(a, b)]);
    assert.deepEqual(res.map((r) => r.analysisSkipped ?? null), [null, 'sample', null, 'sample']);
    assert.equal(skippedMetric('sample') - before, 2);
    store.integrity.set(c, { level: 'suspected' });
    assert.equal(store.games.finishBatch([record(c, a)])[0].analysisSkipped, undefined);
    assert.equal(calls, 4, 'no draw for a flagged game');
    store.close();

    const none = setup({ ANALYSIS_SAMPLE_RATE: '0' }, { random: () => 0 });
    assert.equal(none.store.games.finishBatch([record(none.ids[0], none.ids[1])])[0].analysisSkipped, 'sample');
    none.store.close();

    let used = 0;
    const all = setup({}, { random: () => { used++; return 0.99; } });
    assert.equal(all.store.games.finishBatch([record(all.ids[0], all.ids[1])])[0].analysisSkipped, undefined);
    assert.equal(used, 0, 'the default rate 1 draws nothing');
    all.store.close();
});

test('the sample rate and the cap are validated by the configuration', () => {
    assert.equal(testConfig().analysisSampleRate, 1);
    assert.equal(testConfig().analysisQueueMax, 5000);
    assert.equal(testConfig({ ANALYSIS_SAMPLE_RATE: '.25' }).analysisSampleRate, 0.25);
    assert.throws(() => testConfig({ ANALYSIS_SAMPLE_RATE: '1.5' }), /ANALYSIS_SAMPLE_RATE: at most 1/);
    assert.throws(() => testConfig({ ANALYSIS_SAMPLE_RATE: '-0.1' }), /ANALYSIS_SAMPLE_RATE: at least 0/);
    assert.throws(() => testConfig({ ANALYSIS_SAMPLE_RATE: 'half' }), /ANALYSIS_SAMPLE_RATE: number expected/);
    assert.throws(() => testConfig({ ANALYSIS_QUEUE_MAX: '-1' }), /ANALYSIS_QUEUE_MAX: at least 0/);
    // Every commit of finished games counts the waiting ordinary jobs up to this cap, under the
    // write lock: a huge cap would make each commit scan the whole queue.
    assert.equal(testConfig({ ANALYSIS_QUEUE_MAX: '100000' }).analysisQueueMax, 100000);
    assert.throws(() => testConfig({ ANALYSIS_QUEUE_MAX: '100001' }), /ANALYSIS_QUEUE_MAX: at most 100000/);
});

test('analysis.request: eligible games only; raises a waiting job, re-queues a failed one, leaves running and done jobs', () => {
    const { store, ids: [a, b] } = setup({ ANALYSIS_QUEUE_MAX: '1' });
    const [waiting, skipped, running, done, failed] = Array.from({ length: 5 }, () => record(a, b));
    const casual = record(a, b, { rated: false });
    const custom = record(a, b, { category: 'custom' });
    const aborted = record(a, b, { status: GameStatus.Aborted, reason: EndReason.NoShow });
    const short = record(a, b, { plies: 19 });
    const res = store.games.finishBatch([waiting, skipped, running, done, failed, casual, custom, aborted, short]);
    assert.deepEqual(res.map((r) => r.analysisSkipped ?? null), [null, 'backlog', 'backlog', 'backlog', 'backlog', null, null, null, null]);
    for (const g of [casual, custom, aborted, short]) assert.equal(store.analysis.request(g.id, 'report'), false);
    assert.equal(store.analysis.request(424242, 'report'), false, 'unknown game: no job, no foreign key error');
    assert.equal(store.analysis.backlog().priority, 0);
    assert.equal(store.analysis.request(skipped.id, 'report'), true);
    assert.equal(store.analysis.request(skipped.id, 'signal'), false, 'already waiting with a higher priority');
    assert.equal(store.analysis.request(waiting.id, 'signal'), true, 'priority raised');
    assert.deepEqual(store.analysis.backlog(), { ordinary: 0, priority: 2 });

    // Running, done and failed jobs.
    store.analysis.enqueue(running.id);
    store.analysis.enqueue(done.id);
    store.analysis.enqueue(failed.id);
    const t = Date.now();
    const claimed = new Map(store.analysis.next(3, 'w', t).map((j) => [j.gameId, j]));
    assert.ok(claimed.has(running.id) && claimed.has(done.id) && claimed.has(failed.id));
    store.analysis.complete(done.id, { gameId: done.id }, t);
    for (let i = 0; i < 3; i++) {
        assert.equal(store.analysis.fail(failed.id, 'engine crashed', t), i < 2 ? 'queued' : 'failed');
        if (i < 2) assert.equal(store.analysis.next(1, 'w', t)[0].gameId, failed.id);
    }
    assert.equal(store.analysis.request(running.id, 'report'), false);
    assert.equal(store.analysis.request(done.id, 'report'), false);
    assert.equal(store.analysis.request(failed.id, 'report'), true);
    const again = store.analysis.next(1, 'w', t)[0];
    assert.deepEqual([again.gameId, again.attempts], [failed.id, 1], 'a failed job gets its attempts back');
    assert.throws(() => store.analysis.request(waiting.id, 'urgent'), (e) => e instanceof StoreError && e.code === 'invalid');
    assert.throws(() => store.analysis.request(waiting.id, 'ordinary'), (e) => e.code === 'invalid', 'would bypass ANALYSIS_QUEUE_MAX');
    store.close();
});

test('a report on a game asks for its analysis ahead of the ordinary games (abuse reports do not)', () => {
    const { store, ids: [a, b, c] } = setup({ ANALYSIS_QUEUE_MAX: '0' });
    const [g1, g2] = [record(a, b), record(a, c)];
    store.games.finishBatch([g1, g2]);
    assert.deepEqual(store.analysis.backlog(), { ordinary: 0, priority: 0 });
    const config = testConfig();
    const user = (id) => ({ id, createdAt: Date.now() - 400 * DAY });
    const now = Date.now() + 60000;
    // Ben has played 60 rated games (a credible reporter); Cid only this one (low credibility).
    const credible = { ...store, ratings: { ...store.ratings, forUser: (id) => (id === b ? [{ category: '5+0', games: 60 }] : store.ratings.forUser(id)) } };
    const report = (reporter, body) => handleReport({ user: user(reporter), body, store: credible, config }, { now: () => now });
    assert.equal(report(a, { gameId: g1.id, reported: 'Ben', category: 'abuse' }).status, 202);
    assert.deepEqual(store.analysis.backlog(), { ordinary: 0, priority: 0 });
    assert.equal(report(b, { gameId: g1.id, reported: 'Ann', category: 'cheating' }).status, 202);
    assert.equal(report(c, { gameId: g2.id, reported: 'Ann', category: 'other' }).status, 202);
    assert.deepEqual(store.reports.forReported(a).map((r) => r.weight >= 0.5).sort(), [false, true]);
    // The credible report at 'report' priority; the low-credibility one only at 'signal' priority,
    // beside the statistical suspicion signals instead of ahead of them.
    assert.deepEqual(claimAll(store), [[g1.id, AnalysisPriority.report], [g2.id, AnalysisPriority.signal]]);
    store.close();
});

test('reports from fresh accounts cannot push the games of a statistically suspected player back', () => {
    const { store, ids: [a, b, c, d] } = setup({ ANALYSIS_QUEUE_MAX: '0' });
    // Two brand-new accounts play each other and report every game (stored weight at most 0.1).
    const theirs = Array.from({ length: 5 }, () => record(c, d, { whiteName: 'Cid', blackName: 'Dee' }));
    store.games.finishBatch(theirs);
    store.integrity.set(a, { level: 'suspected', score: 3 });
    const suspect = record(a, b);
    store.games.finishBatch([suspect]);
    const config = testConfig();
    const now = Date.now() + 60000;
    for (const g of theirs) {
        const body = { gameId: g.id, reported: 'Dee', category: 'cheating' };
        assert.equal(handleReport({ user: { id: c, createdAt: now - 3600000 }, body, store, config }, { now: () => now }).status, 202);
    }
    assert.ok(store.reports.forReported(d).every((r) => r.weight < 0.5));
    const claimed = claimAll(store);
    assert.equal(claimed.length, 6);
    assert.deepEqual(claimed[0], [suspect.id, AnalysisPriority.signal], "the suspect's game first (queued first, in the same tier)");
    assert.ok(claimed.every(([, p]) => p === AnalysisPriority.signal));
    store.close();
});

test('every fourth claim takes the oldest ordinary game first: prioritized games never starve the ordinary ones', () => {
    const { store, ids: [a, b, c, d] } = setup();
    const ordinary = [record(a, b), record(b, a), record(a, b)];
    store.games.finishBatch(ordinary);
    store.integrity.set(c, { level: 'suspected' });
    store.integrity.set(d, { level: 'high_confidence' });
    store.games.finishBatch(Array.from({ length: 10 }, (_, i) => (i % 2 ? record(c, a) : record(d, b))));
    const P = AnalysisPriority;
    const claimed = Array.from({ length: 14 }, () => store.analysis.next(1, 'w', Date.now())[0] ?? null);
    assert.deepEqual(claimed.map((j) => (j ? j.priority : null)), [1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, null]);
    assert.deepEqual(claimed.filter((j) => j && j.priority === P.ordinary).map((j) => j.gameId), ordinary.map((g) => g.id), 'oldest first');
    store.close();

    // Without ordinary jobs the reserved claim takes the next prioritized job; several claims at
    // once get their share too.
    const two = setup();
    const [x, y, z] = two.ids;
    two.store.integrity.set(z, { level: 'suspected' });
    two.store.games.finishBatch(Array.from({ length: 5 }, () => record(z, x)));
    assert.equal(Array.from({ length: 5 }, () => two.store.analysis.next(1, 'w', Date.now()).length).join(''), '11111');
    two.store.games.finishBatch([...Array.from({ length: 8 }, () => record(z, y)), record(x, y), record(y, x)]);
    const batch = two.store.analysis.next(8, 'w', Date.now());
    assert.equal(batch.length, 8);
    assert.equal(batch.filter((j) => j.priority === P.ordinary).length, 2, 'claims 6 to 13: two reserved turns (the 8th and the 12th)');
    two.store.close();
});

test('at most 20 flagged games of one player wait; more are skipped and counted until the engine takes some', () => {
    const { store, ids: [a, b, c, d, e] } = setup();
    store.integrity.set(c, { level: 'suspected', score: 3 });
    const before = skippedMetric('player');
    const games = Array.from({ length: 23 }, (_, i) => (i % 2 ? record(c, a) : record(b, c)));
    const res = store.games.finishBatch(games);
    assert.deepEqual(res.map((r) => r.analysisSkipped ?? null), [...Array(20).fill(null), 'player', 'player', 'player']);
    assert.ok(res.every((r) => r.ratings), 'the skipped games are rated and stored all the same');
    assert.equal(skippedMetric('player') - before, 3);
    assert.deepEqual(store.analysis.backlog(), { ordinary: 0, priority: 20 });
    // Never demoted to the ordinary sample (which feeds the population statistics).
    assert.equal(store.analysis.stats().queued, 20);
    // The cap is per player: another flagged player's games and ordinary games are still queued.
    store.integrity.set(e, { level: 'suspected' });
    assert.equal(store.games.finishBatch([record(e, d)])[0].analysisSkipped, undefined);
    assert.equal(store.games.finishBatch([record(a, d)])[0].analysisSkipped, undefined);
    // A game of c taken by the engine makes room for one more.
    const job = store.analysis.next(1, 'w', Date.now())[0];
    assert.equal(job.gameId, games[0].id);
    assert.equal(store.games.finishBatch([record(c, d)])[0].analysisSkipped, undefined);
    assert.equal(store.games.finishBatch([record(c, d)])[0].analysisSkipped, 'player');
    // A low-credibility report ('signal') does not pass the cap; a credible one ('report') does.
    assert.equal(store.analysis.request(games[21].id, 'signal'), false);
    assert.equal(store.analysis.request(games[21].id, 'report'), true);
    store.close();
});

test('the backlog gauge counts at most 100000 jobs per tier', (t) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-backlog-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const file = path.join(dir, 'db.sqlite');
    const store = openStore(testConfig({ DB_PATH: file }), { applyGame });
    migrate(store);
    const raw = new DatabaseSync(file, { enableForeignKeyConstraints: false });     // jobs without games rows
    raw.exec(`WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200010)
        INSERT INTO analysis_jobs (game_id, queued_at, priority) SELECT i, i, i % 2 FROM n`);
    raw.close();
    assert.deepEqual(store.analysis.backlog(), { ordinary: 100000, priority: 100000 });
    store.close();
});

test('migration 003 records the players of the jobs queued by an earlier build', (t) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-mig003-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const migDir = path.join(dir, 'migrations');
    fs.mkdirSync(migDir);
    const real = fileURLToPath(new URL('../../src/store/migrations/', import.meta.url));
    for (const f of ['001_initial.sql', '002_analysis_priority.sql']) fs.copyFileSync(path.join(real, f), path.join(migDir, f));
    const config = testConfig({ DB_PATH: path.join(dir, 'db.sqlite'), ANALYSIS_MIN_PLIES: '20' });
    let store = openStore(config, { applyGame });
    assert.deepEqual(migrate(store, { dir: migDir }).applied, [1, 2]);
    const [a, b, c] = ['Ann', 'Ben', 'Cid'].map((n) => store.users.create({ username: n, email: `${n}@example.org` }));
    // Games stored (unrated: this build inserts no job for them) and jobs as build 002 wrote them.
    const games = Array.from({ length: 20 }, (_, i) => record(i % 2 ? c : a, i % 2 ? b : c, { rated: false }));
    store.games.finishBatch(games);
    store.close();
    const raw = new DatabaseSync(config.dbPath);
    const put = raw.prepare('INSERT INTO analysis_jobs (game_id, queued_at, priority) VALUES (?, ?, 1)');
    for (const g of games) put.run(g.id, Date.now());
    raw.close();

    store = openStore(config, { applyGame });
    assert.deepEqual(migrate(store).applied, [3]);
    const check = new DatabaseSync(config.dbPath, { readOnly: true });
    const rows = check.prepare(`SELECT j.white_id AS jw, j.black_id AS jb, g.white_id AS gw, g.black_id AS gb FROM analysis_jobs j
        JOIN games g ON g.id = j.game_id`).all();
    check.close();
    assert.equal(rows.length, 20);
    assert.ok(rows.every((r) => r.jw === r.gw && r.jb === r.gb));
    // Cid has 20 signal jobs waiting from the earlier build: the cap already counts them.
    store.integrity.set(c, { level: 'suspected' });
    assert.equal(store.games.finishBatch([record(c, a)])[0].analysisSkipped, 'player');
    store.close();
});

test('writer thread: the skipped games are counted in the shard registry from the thread answers', async (t) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-queue-writer-'));
    const config = testConfig({ DB_PATH: path.join(dir, 'db.sqlite'), DATA_DIR: dir, ANALYSIS_QUEUE_MAX: '1', ANALYSIS_MIN_PLIES: '20' });
    const store = openStore(config, { applyGame });
    migrate(store);
    const writer = startStoreWriter({ config, logging: false });
    t.after(async () => {
        await writer.close();
        store.close();
        fs.rmSync(dir, { recursive: true, force: true });
    });
    const a = store.users.create({ username: 'Wa', email: 'wa@example.org' });
    const b = store.users.create({ username: 'Wb', email: 'wb@example.org' });
    const before = skippedMetric('backlog');
    const res = await writer.finishBatch([record(a, b), record(b, a), record(a, b)]);
    assert.deepEqual(res.map((r) => r.analysisSkipped ?? null), [null, 'backlog', 'backlog']);
    assert.equal(skippedMetric('backlog') - before, 2);
    assert.deepEqual(store.analysis.backlog(), { ordinary: 1, priority: 0 });
});
