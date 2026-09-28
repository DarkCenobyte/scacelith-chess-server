import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { Worker } from 'node:worker_threads';
import { openStore, migrate, StoreError } from '../../src/store/index.js';
import { testConfig } from '../../src/config.js';
import { enums } from '../../src/protocol/schema.js';

const { GameStatus, EndReason } = enums;
const STORE_URL = new URL('../../src/store/index.js', import.meta.url).href;
const CONFIG_URL = new URL('../../src/config.js', import.meta.url).href;
const ID_BASE = 1_000_000_000_000;

// Minimal Elo with the documented signature of match/elo.js applyGame (K 40 provisional, else 20).
function applyGame(white, black, score, cfg) {
    const e = 1 / (1 + 10 ** ((black.rating - white.rating) / 400));
    const k = (r) => (r.games < cfg.provisionalGames ? 40 : 20);
    const side = (r, s, exp) => {
        const after = Math.max(100, r.rating + Math.round(k(r) * (s - exp)));
        return {
            before: r.rating, after,
            record: { rating: after, games: r.games + 1, wins: r.wins + (s === 1 ? 1 : 0), draws: r.draws + (s === 0.5 ? 1 : 0),
                losses: r.losses + (s === 0 ? 1 : 0), peak: Math.max(r.peak, after), reachedSenior: r.reachedSenior || after >= 2400 },
        };
    };
    return { white: side(white, score, e), black: side(black, 1 - score, 1 - e) };
}

function setup(overrides = {}) {
    const store = openStore(testConfig({ DB_PATH: ':memory:', ...overrides }), { applyGame });
    migrate(store);
    const ids = ['Ann', 'Ben', 'Cid', 'Dee'].map((n) => store.users.create({ username: n, email: `${n}@example.org` }));
    return { store, ids };
}

let nextId = ID_BASE;
function record(white, black, extra = {}) {
    const plies = extra.plies ?? 40;
    const moves = new Uint16Array(plies);
    const spentMs = new Uint32Array(plies);
    const clockMs = new Uint32Array(plies);
    for (let i = 0; i < plies; i++) { moves[i] = (i * 7) & 0x7fff; spentMs[i] = 1000 + i; clockMs[i] = 180000 - i * 10; }
    return {
        id: ++nextId, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: white, blackId: black,
        whiteName: 'W', blackName: 'B', whiteRating: 1500, blackRating: 1500, startedAt: 1_800_000_000_000,
        endedAt: 1_800_000_600_000, status: GameStatus.WhiteWins, reason: EndReason.Checkmate, moves, spentMs, clockMs,
        rematchOf: 0, flags: 0, ...extra,
    };
}

test('finishBatch: game rows, ratings read and written in the transaction, RatingChange objects', () => {
    const { store, ids: [a, b, c] } = setup();
    const g1 = record(a, b);
    const g2 = record(c, a, { status: GameStatus.Draw, reason: EndReason.Agreement });
    const res = store.games.finishBatch([g1, g2]);
    assert.equal(res.length, 2);
    assert.deepEqual(res[0], { gameId: g1.id, ratings: {
        white: { before: 1500, after: 1520, games: 1, provisional: true },
        black: { before: 1500, after: 1480, games: 1, provisional: true } } });
    // Second game uses a's rating as updated by the first one, inside the same transaction.
    const r2 = res[1].ratings;
    assert.equal(r2.black.before, 1520);
    assert.equal(r2.white.before, 1500);
    assert.equal(r2.black.games, 2);
    assert.equal(r2.white.after - 1500, 1520 - r2.black.after, 'zero-sum with equal K');

    const ra = store.ratings.get(a, '3+2');
    assert.deepEqual({ ...ra }, { rating: r2.black.after, games: 2, wins: 1, draws: 1, losses: 0, peak: 1520, reachedSenior: false });
    assert.deepEqual(store.ratings.get(b, '3+2'), { rating: 1480, games: 1, wins: 0, draws: 0, losses: 1, peak: 1500, reachedSenior: false });
    assert.deepEqual(store.ratings.get(a, '5+0'), { rating: 1500, games: 0, wins: 0, draws: 0, losses: 0, peak: 1500, reachedSenior: false });
    const fu = store.ratings.forUser(a);
    assert.equal(fu.length, 1);
    assert.equal(fu[0].category, '3+2');
    assert.equal(fu[0].provisional, true);

    const g = store.games.byId(g1.id);
    assert.equal(g.whiteId, a);
    assert.equal(g.status, GameStatus.WhiteWins);
    assert.equal(g.reason, EndReason.Checkmate);
    assert.equal(g.plyCount, 40);
    assert.equal(g.rated, true);
    assert.equal(g.rematchOf, null);
    assert.deepEqual(g.ratingChanges, { white: { before: 1500, after: 1520 }, black: { before: 1500, after: 1480 } });
    assert.ok(g.moves instanceof Uint16Array);
    assert.deepEqual([...g.moves], [...g1.moves]);
    assert.deepEqual([...g.spentMs], [...g1.spentMs]);
    assert.deepEqual([...g.clockMs], [...g1.clockMs]);
    assert.equal(store.games.byId(12345), null);
    store.close();
});

test('finishBatch: unrated, aborted and custom games change no rating; analysis queue rules', () => {
    const { store, ids: [a, b] } = setup({ ANALYSIS_MIN_PLIES: '20' });
    const casual = record(a, b, { rated: false });
    const aborted = record(a, b, { status: GameStatus.Aborted, reason: EndReason.NoShow, plies: 1 });
    const custom = record(a, b, { category: 'custom', rated: true, baseMs: 420000 });
    const short = record(a, b, { plies: 19 });
    const long = record(b, a, { plies: 20, status: GameStatus.BlackWins, reason: EndReason.Resignation });
    const res = store.games.finishBatch([casual, aborted, custom, short, long]);
    assert.deepEqual(res.slice(0, 3).map((r) => r.ratings), [null, null, null]);
    assert.ok(res[3].ratings && res[4].ratings);
    assert.equal(store.ratings.get(a, '3+2').games, 2);
    assert.equal(store.ratings.get(a, '3+2').wins, 2);
    assert.equal(store.ratings.get(b, '3+2').losses, 2);
    assert.equal(store.ratings.get(a, 'custom').games, 0);
    assert.equal(store.games.byId(aborted.id).plyCount, 1);
    assert.equal(store.games.byId(custom.id).ratingChanges, null);
    // Only the rated, played game with >= ANALYSIS_MIN_PLIES plies is queued.
    const jobs = store.analysis.next(10, 'w', Date.now());
    assert.deepEqual(jobs.map((j) => j.gameId), [long.id]);
    store.close();
});

test('finishBatch is idempotent: a re-committed game id changes nothing and reports the stored change', () => {
    const { store, ids: [a, b] } = setup();
    const g = record(a, b);
    const first = store.games.finishBatch([g]);
    const again = store.games.finishBatch([g, record(b, a)]);
    assert.equal(again[0].duplicate, true);
    // The duplicate is evaluated before the batch's new game: the player's current count is 1.
    assert.deepEqual(again[0].ratings.white, { before: 1500, after: 1520, games: 1, provisional: true });
    assert.equal(again[1].duplicate, undefined);
    assert.equal(first[0].ratings.white.after, 1520);
    assert.equal(store.ratings.get(a, '3+2').games, 2, 'only the new game was rated');
    assert.equal(store.games.recentForUser(a, 10).length, 2);
    assert.equal(store.analysis.stats().queued, 2);
    store.close();
});

test('finishBatch is atomic: a failing record rolls the whole batch back', () => {
    const { store, ids: [a, b] } = setup();
    const ok1 = record(a, b);
    const bad = record(a, 999999);            // unknown user: foreign key violation
    const invalid = record(a, b, { status: GameStatus.Ongoing });
    assert.throws(() => store.games.finishBatch([ok1, bad]), (e) => e instanceof StoreError && e.code === 'foreign_key');
    assert.equal(store.games.byId(ok1.id), null);
    assert.equal(store.ratings.get(a, '3+2').games, 0);
    assert.equal(store.analysis.stats().queued, 0);
    assert.throws(() => store.games.finishBatch([ok1, invalid]), (e) => e.code === 'invalid_record' && e.gameId === invalid.id);
    assert.equal(store.games.byId(ok1.id), null);
    // A throwing applyGame also rolls back.
    const s2 = openStore(testConfig({ DB_PATH: ':memory:' }), { applyGame: () => { throw new Error('elo exploded'); } });
    migrate(s2);
    const x = s2.users.create({ username: 'X', email: 'x@e.org' });
    const y = s2.users.create({ username: 'Y', email: 'y@e.org' });
    const casual = record(x, y, { rated: false });
    assert.throws(() => s2.games.finishBatch([casual, record(x, y)]), /elo exploded/);
    assert.equal(s2.games.byId(casual.id), null);
    s2.close();
    // Without applyGame, rated games cannot be committed (unrated ones can).
    const s3 = openStore(testConfig({ DB_PATH: ':memory:' }));
    migrate(s3);
    const p = s3.users.create({ username: 'P', email: 'p@e.org' });
    const q = s3.users.create({ username: 'Q', email: 'q@e.org' });
    assert.throws(() => s3.games.finishBatch([record(p, q)]), (e) => e.code === 'no_rating_function');
    assert.equal(s3.games.finishBatch([record(p, q, { rated: false })])[0].ratings, null);
    s3.close();
    // The store is still usable after a rollback.
    assert.equal(store.games.finishBatch([ok1])[0].gameId, ok1.id);
    assert.deepEqual(store.games.finishBatch([]), []);
    store.close();
});

test('ratings over many games: provisional flag, peak, leaderboard filters', () => {
    const { store, ids: [a, b, c, d] } = setup({ PROVISIONAL_GAMES: '3' });
    const batch = [];
    for (let i = 0; i < 4; i++) batch.push(record(a, b));
    for (let i = 0; i < 3; i++) batch.push(record(c, b, { status: GameStatus.Draw }));
    batch.push(record(d, c, { status: GameStatus.BlackWins }));
    const res = store.games.finishBatch(batch);
    assert.equal(res[2].ratings.white.provisional, false, '3 games played: established');
    assert.equal(res[1].ratings.white.provisional, true);
    const ra = store.ratings.get(a, '3+2');
    assert.equal(ra.games, 4);
    assert.equal(ra.peak, ra.rating);
    const rb = store.ratings.get(b, '3+2');
    assert.equal(rb.peak, 1500);
    assert.equal(rb.games, 7);

    let board = store.ratings.leaderboard('3+2', 100, 3);
    assert.deepEqual(board.map((r) => r.userId), [a, c, b], 'd has 1 game only; sorted by rating');
    assert.equal(board[0].username, 'Ann');
    assert.deepEqual(store.ratings.leaderboard('3+2', 1, 3).map((r) => r.userId), [a]);
    assert.deepEqual(store.ratings.leaderboard('5+0', 100, 0), []);
    store.integrity.set(a, { level: 'confirmed', score: 9 });
    store.users.anonymize(c);
    board = store.ratings.leaderboard('3+2', 100, 3);
    assert.deepEqual(board.map((r) => r.userId), [b], 'confirmed cheaters and deleted accounts are hidden');
    store.close();
});

test('recentForUser pagination, countBetween, countForUser', () => {
    const { store, ids: [a, b, c] } = setup();
    const batch = [];
    for (let i = 0; i < 25; i++) batch.push(i % 2 ? record(a, b, { rated: false, endedAt: 1000 + i }) : record(c, a, { rated: false, endedAt: 1000 + i }));
    store.games.finishBatch(batch);
    const page1 = store.games.recentForUser(a, 10);
    assert.equal(page1.length, 10);
    assert.deepEqual(page1.map((g) => g.id), batch.slice(-10).reverse().map((g) => g.id));
    assert.equal(page1[0].moves, undefined, 'summaries carry no move arrays');
    const page3 = store.games.recentForUser(a, 10, store.games.recentForUser(a, 10, page1[9].id)[9].id);
    assert.equal(page3.length, 5);
    assert.equal(page3[4].id, batch[0].id);
    assert.equal(store.games.recentForUser(b, 50).length, 12);
    assert.equal(store.games.countBetween(a, b, 0), 12);
    assert.equal(store.games.countBetween(b, a, 0), 12);
    assert.equal(store.games.countBetween(a, c, 1020), 3);
    assert.equal(store.games.countBetween(a, c, 0, { rated: true }), 0);
    assert.equal(store.games.countForUser(a), 25);
    assert.equal(store.games.countForUser(99), 0);
    store.close();
});

test('analysis queue: claim, complete, fail with attempt cap, stale re-queue, forUser, enqueue', () => {
    const { store, ids: [a, b] } = setup();
    const gs = [record(a, b), record(b, a), record(a, b)];
    store.games.finishBatch(gs);
    const t = Date.now();
    const j1 = store.analysis.next(2, 'w1', t);
    assert.deepEqual(j1.map((j) => j.gameId), [gs[0].id, gs[1].id]);
    assert.equal(j1[0].attempts, 1);
    assert.equal(j1[0].worker, 'w1');
    assert.deepEqual(store.analysis.next(5, 'w2', t).map((j) => j.gameId), [gs[2].id]);
    assert.deepEqual(store.analysis.next(5, 'w2', t), []);
    assert.equal(store.analysis.complete(gs[0].id, { acpl: 23.5, moves: 20 }, t), true);
    assert.equal(store.analysis.fail(gs[1].id, 'engine crashed', t), 'queued');
    assert.deepEqual(store.analysis.stats(), { queued: 1, running: 1, done: 1, failed: 0 });
    // Retried until the cap.
    assert.equal(store.analysis.next(1, 'w1', t)[0].attempts, 2);
    assert.equal(store.analysis.fail(gs[1].id, 'again', t), 'queued');
    assert.equal(store.analysis.next(1, 'w1', t)[0].attempts, 3);
    assert.equal(store.analysis.fail(gs[1].id, 'third', t), 'failed');
    assert.deepEqual(store.analysis.next(5, 'w1', t), []);
    assert.equal(store.analysis.fail(424242, 'x', t), null);
    // gs[2] is running for w2 since t: 10 minutes later it goes back to the queue.
    assert.deepEqual(store.analysis.next(5, 'w3', t + 5 * 60000), []);
    const again = store.analysis.next(5, 'w3', t + 11 * 60000);
    assert.deepEqual(again.map((j) => [j.gameId, j.attempts, j.worker]), [[gs[2].id, 2, 'w3']]);
    const mine = store.analysis.forUser(a, 10);
    assert.equal(mine.length, 3);
    assert.equal(mine[0].gameId, gs[2].id);
    const done = mine.find((j) => j.gameId === gs[0].id);
    assert.deepEqual(done.features, { acpl: 23.5, moves: 20 });
    assert.equal(done.color, 'white');
    assert.equal(mine.find((j) => j.gameId === gs[1].id).color, 'black');
    store.analysis.enqueue(gs[1].id, t);
    assert.equal(store.analysis.next(5, 'w9', t + 11 * 60000)[0].attempts, 1);
    assert.throws(() => store.analysis.enqueue(55555, t), (e) => e.code === 'foreign_key');
    store.close();
});

function runWorker(code, workerData) {
    return new Promise((resolve, reject) => {
        const w = new Worker(`
            const { workerData, parentPort } = require('node:worker_threads');
            (async () => { ${code} })().then((r) => parentPort.postMessage({ ok: true, r }),
                (e) => parentPort.postMessage({ ok: false, e: String((e && e.stack) || e) }));
        `, { eval: true, workerData });
        w.once('message', (m) => (m.ok ? resolve(m.r) : reject(new Error(m.e))));
        w.once('error', reject);
    });
}

test('analysis.next: two workers with their own connections never claim the same job', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-jobs-'));
    const file = path.join(dir, 'jobs.db');
    const store = openStore(testConfig({ DB_PATH: file }), { applyGame });
    migrate(store);
    const a = store.users.create({ username: 'Ja', email: 'ja@e.org' });
    const b = store.users.create({ username: 'Jb', email: 'jb@e.org' });
    const batch = [];
    for (let i = 0; i < 200; i++) batch.push(record(a, b, { status: i % 3 ? GameStatus.WhiteWins : GameStatus.Draw }));
    store.games.finishBatch(batch);
    const sab = new SharedArrayBuffer(8);
    const ready = new Int32Array(sab, 0, 1);
    const go = new Int32Array(sab, 4, 1);
    const code = `
        const { openStore } = await import(${JSON.stringify(STORE_URL)});
        const { testConfig } = await import(${JSON.stringify(CONFIG_URL)});
        const store = openStore(testConfig({ DB_PATH: workerData.file }));
        const ready = new Int32Array(workerData.sab, 0, 1), go = new Int32Array(workerData.sab, 4, 1);
        Atomics.add(ready, 0, 1);
        Atomics.wait(go, 0, 0);
        const got = [];
        for (;;) {
            const jobs = store.analysis.next(3, workerData.name, Date.now());
            if (!jobs.length) break;
            for (const j of jobs) { got.push(j.gameId); store.analysis.complete(j.gameId, { by: workerData.name }); }
        }
        store.close();
        return got;`;
    const runs = [runWorker(code, { file, sab, name: 'w1' }), runWorker(code, { file, sab, name: 'w2' })];
    while (Atomics.load(ready, 0) < 2) await sleep(5);
    Atomics.store(go, 0, 1);
    Atomics.notify(go, 0);
    const [g1, g2] = await Promise.all(runs);
    const all = [...g1, ...g2];
    assert.equal(all.length, 200);
    assert.equal(new Set(all).size, 200, 'no job claimed twice');
    assert.deepEqual(store.analysis.stats(), { queued: 0, running: 0, done: 200, failed: 0 });
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});
