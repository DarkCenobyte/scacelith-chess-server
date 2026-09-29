// Retention purge against the real schema: a temporary database file migrated with the real
// migrations, old and recent rows of every kind the purge touches. Old rows go, recent rows stay,
// IP columns are erased (rows kept) where that is the design, and the chunked, sliced runner
// (runAsync, used by the primary) does exactly what the one-shot run() does.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { openStore, migrate } from '../../src/store/index.js';
import { testConfig } from '../../src/config.js';
import { enums } from '../../src/protocol/schema.js';

const { GameStatus, EndReason } = enums;
const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite');
const DAY = 86400000;
const NOW = Date.now() + 400 * DAY;     // the queue times (real clock) are then "long ago"

function applyGame(white, black, score) {
    const side = (r, s) => ({ before: r.rating, after: r.rating + (s - 0.5) * 20 });
    return { white: side(white, score), black: side(black, 1 - score) };
}

let nextGameId = 7_000_000_000_000;
function record(white, black) {
    const plies = 40;
    return {
        id: ++nextGameId, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: white, blackId: black,
        whiteName: 'W', blackName: 'B', startedAt: Date.now() - 600000, endedAt: Date.now(), status: GameStatus.WhiteWins,
        reason: EndReason.Checkmate, moves: new Uint16Array(plies), spentMs: new Uint32Array(plies), clockMs: new Uint32Array(plies),
    };
}

/** A migrated store on a temporary file, and a second raw connection to inspect and prepare rows. */
function tempStore(t, overrides = {}) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-retention-'));
    const file = path.join(dir, 'retention.db');
    const store = openStore(testConfig({ DB_PATH: file, RETENTION_SECURITY_DAYS: '90', RETENTION_IP_DAYS: '30', ...overrides }), { applyGame });
    migrate(store);
    const raw = new DatabaseSync(file);
    raw.exec('PRAGMA busy_timeout = 5000');
    t.after(() => {
        raw.close();
        store.close();
        fs.rmSync(dir, { recursive: true, force: true });
    });
    return { store, raw };
}

// Old and recent rows of every kind; returns the game ids of the analysis jobs by state.
function seed(store, raw) {
    const a = store.users.create({ username: 'Ann', email: 'ann@example.org' });
    const b = store.users.create({ username: 'Ben', email: 'ben@example.org' });
    const sess = (tokenHash, f) => store.sessions.create({ userId: a, tokenHash, expiresAt: NOW + 50 * DAY, idleExpiresAt: NOW + DAY, ...f });
    sess('s-expired', { createdAt: NOW - 100 * DAY, expiresAt: NOW - 1, ip: '198.51.100.1' });
    sess('s-idle', { createdAt: NOW - 10 * DAY, idleExpiresAt: NOW - 1, ip: '198.51.100.2' });
    store.sessions.revoke(sess('s-revoked-old', { createdAt: NOW - 10 * DAY }), a, NOW - 2 * DAY);
    store.sessions.revoke(sess('s-revoked-new', { createdAt: NOW - 10 * DAY }), a, NOW - 1000);
    sess('s-live-old-ip', { createdAt: NOW - 40 * DAY, ip: '198.51.100.3' });
    sess('s-live-new-ip', { createdAt: NOW - DAY, ip: '198.51.100.4' });
    store.tokens.create({ kind: 'password_reset', tokenHash: 't-expired', userId: a, data: { email: 'ann@example.org' }, expiresAt: NOW - 1 });
    store.tokens.create({ kind: 'password_reset', tokenHash: 't-live', userId: a, expiresAt: NOW + 3600000 });
    store.tokens.create({ kind: 'email_verify', tokenHash: 't-consumed-live', userId: a, expiresAt: NOW + 3600000 });
    store.tokens.consume('email_verify', 't-consumed-live', NOW - 5000);
    store.security.insertBatch([
        { kind: 'sec-old', userId: a, ip: '203.0.113.1', at: NOW - 91 * DAY },
        { kind: 'sec-mid', userId: a, ip: '203.0.113.2', at: NOW - 31 * DAY },
        { kind: 'sec-mid-noip', userId: a, ip: null, at: NOW - 31 * DAY },
        { kind: 'sec-new', userId: a, ip: '203.0.113.3', at: NOW - DAY },
    ]);
    store.anomalies.insertBatch([
        { userId: a, kind: 'desync', severity: 'info', at: NOW - 91 * DAY },
        { userId: a, kind: 'bad_seq', severity: 'suspicious', at: NOW - 91 * DAY },
        { userId: a, kind: 'illegal_move', severity: 'certain', at: NOW - 91 * DAY },
        { userId: a, kind: 'flood', severity: 'suspicious', at: NOW - DAY },
    ]);
    store.conduct.record(a, 'abandon', NOW - 31 * DAY);
    store.conduct.record(a, 'noshow', NOW - DAY);
    // Analysis jobs: failed long ago / recently, done long ago, queued long ago, running.
    const games = Array.from({ length: 5 }, () => record(a, b));
    store.games.finishBatch(games);
    const [failedOld, failedNew, doneOld, queuedOld, running] = games.map((g) => g.id);
    const set = raw.prepare('UPDATE analysis_jobs SET status = ?, finished_at = ?, features = ?, attempts = ? WHERE game_id = ?');
    set.run('failed', NOW - 31 * DAY, null, 3, failedOld);
    set.run('failed', NOW - DAY, null, 3, failedNew);
    set.run('done', NOW - 100 * DAY, JSON.stringify({ gameId: doneOld, white: { userId: a }, black: { userId: b } }), 1, doneOld);
    raw.prepare("UPDATE analysis_jobs SET status = 'running', started_at = ?, attempts = 1 WHERE game_id = ?").run(NOW - 60000, running);
    return { a, b, jobs: { failedOld, failedNew, doneOld, queuedOld, running } };
}

// Everything the purge may touch, as plain rows (to compare two databases).
function dump(raw) {
    const q = (sql) => raw.prepare(sql).all().map((r) => ({ ...r }));
    return {
        sessions: q('SELECT CAST(token_hash AS TEXT) AS token, ip FROM sessions ORDER BY id'),
        tokens: q('SELECT CAST(token_hash AS TEXT) AS token FROM tokens ORDER BY id'),
        security: q('SELECT kind, ip FROM security_events ORDER BY id'),
        anomalies: q('SELECT kind, severity FROM anomalies ORDER BY id'),
        conduct: q('SELECT kind FROM conduct_events ORDER BY id'),
        jobs: q('SELECT game_id AS gameId, status FROM analysis_jobs ORDER BY game_id'),
    };
}

// ipErased: the IPs of the 40-day-old live session and of the 31-day-old event, and those of the
// expired session and of the 91-day-old event, erased before their rows are deleted.
const EXPECTED_COUNTS = { sessions: 3, tokens: 1, securityEvents: 1, anomalies: 2, conductEvents: 1, analysisJobs: 1, ipErased: 4 };

test('retention on a real database: old rows go, recent rows stay, IPs are erased in place', async (t) => {
    const { store, raw } = tempStore(t);
    const { a, jobs } = seed(store, raw);
    const counts = await store.retention.runAsync(NOW);
    assert.deepEqual(counts, EXPECTED_COUNTS);
    const d = dump(raw);
    // Expired, idle-expired and revoked (for more than a day) sessions are deleted; the live
    // session older than RETENTION_IP_DAYS keeps its row but loses its IP.
    assert.deepEqual(d.sessions, [
        { token: 's-revoked-new', ip: null },
        { token: 's-live-old-ip', ip: null },
        { token: 's-live-new-ip', ip: '198.51.100.4' },
    ]);
    // Expired tokens (with the e-mail address in their data) are deleted, live ones stay even consumed.
    assert.deepEqual(d.tokens.map((r) => r.token), ['t-live', 't-consumed-live']);
    // Security events: deleted after RETENTION_SECURITY_DAYS, IP erased after RETENTION_IP_DAYS.
    assert.deepEqual(d.security, [
        { kind: 'sec-mid', ip: null },
        { kind: 'sec-mid-noip', ip: null },
        { kind: 'sec-new', ip: '203.0.113.3' },
    ]);
    // Anomalies: certain ones (evidence of a sanction) are kept, the others go with the security retention.
    assert.deepEqual(d.anomalies, [{ kind: 'illegal_move', severity: 'certain' }, { kind: 'flood', severity: 'suspicious' }]);
    assert.deepEqual(d.conduct, [{ kind: 'noshow' }]);
    // Analysis jobs: only failed ones older than 30 days go; done jobs (the players' analysed
    // history), waiting and running jobs stay whatever their age.
    const status = Object.fromEntries(d.jobs.map((r) => [r.gameId, r.status]));
    assert.equal(status[jobs.failedOld], undefined);
    assert.equal(status[jobs.failedNew], 'failed');
    assert.equal(status[jobs.doneOld], 'done');
    assert.equal(status[jobs.queuedOld], 'queued');
    assert.equal(status[jobs.running], 'running');
    assert.equal(store.analysis.forUser(a, 10).find((j) => j.gameId === jobs.doneOld).features.gameId, jobs.doneOld);
    // Nothing is left to do.
    assert.deepEqual(await store.retention.runAsync(NOW), Object.fromEntries(Object.keys(EXPECTED_COUNTS).map((k) => [k, 0])));
});

test('retention: run() and runAsync() execute the same statements with the same result', async (t) => {
    const one = tempStore(t);
    const two = tempStore(t);
    seed(one.store, one.raw);
    seed(two.store, two.raw);
    assert.deepEqual(one.store.retention.run(NOW), EXPECTED_COUNTS);
    assert.deepEqual(await two.store.retention.runAsync(NOW, undefined, { sliceMs: 0 }), EXPECTED_COUNTS);
    const strip = (d) => ({ ...d, jobs: d.jobs.map((j) => j.status) });   // game ids differ between the two databases
    assert.deepEqual(strip(dump(one.raw)), strip(dump(two.raw)));
});

test('retention.runAsync with a fixed chunk of 1000 rows: pauses between slices, stops when aborted', async (t) => {
    const { store, raw } = tempStore(t);
    const a = store.users.create({ username: 'Cid', email: 'cid@example.org' });
    const events = [];
    for (let i = 0; i < 2500; i++) events.push({ kind: 'login_failed', userId: a, ip: null, at: NOW - 100 * DAY });
    store.security.insertBatch(events);
    for (let i = 0; i < 1200; i++) store.conduct.record(a, 'abort', NOW - 40 * DAY);
    const left = () => raw.prepare('SELECT (SELECT count(*) FROM security_events) AS sec, (SELECT count(*) FROM conduct_events) AS conduct').get();

    // Aborted at the 6th pause: 5 statements with nothing to do, then the first chunk of events.
    const ctl = new AbortController();
    let pauses = 0;
    const first = await store.retention.runAsync(NOW, undefined, {
        sliceMs: 0, chunk: 1000, signal: ctl.signal, pause: async () => { if (++pauses === 6) ctl.abort(); },
    });
    assert.equal(pauses, 6);
    assert.equal(first.securityEvents, 1000);
    assert.deepEqual({ ...left() }, { sec: 1500, conduct: 1200 });

    // The next run finishes: one pause per statement with sliceMs 0 (3 chunks of events, 2 of
    // conduct events, one statement for each other step).
    pauses = 0;
    const done = await store.retention.runAsync(NOW, undefined, { sliceMs: 0, chunk: 1000, pause: async () => { pauses++; } });
    assert.equal(done.securityEvents, 1500);
    assert.equal(done.conductEvents, 1200);
    assert.deepEqual({ ...left() }, { sec: 0, conduct: 0 });
    assert.equal(pauses, 5 + 2 + 1 + 2 + 1, 'session IPs, event IPs, sessions x2, tokens, events x2, anomalies, conduct x2, jobs');

    // A large slice runs everything without pausing.
    pauses = 0;
    await store.retention.runAsync(NOW, undefined, { sliceMs: 60000, pause: async () => { pauses++; } });
    assert.equal(pauses, 0);
});

test('retention.runAsync stops once the store is closed (never touches a closed database)', async (t) => {
    const { store } = tempStore(t);
    let pauses = 0;
    const counts = await store.retention.runAsync(NOW, undefined, { sliceMs: 0, pause: async () => { pauses++; store.close(); } });
    assert.equal(pauses, 1);
    assert.deepEqual(counts, Object.fromEntries(Object.keys(EXPECTED_COUNTS).map((k) => [k, 0])));
});

// A clock for runAsync that charges `perRowMs` for every security event deleted since it was last
// read (counted on the raw connection), so statement times are deterministic; `chunks` receives
// the rows each statement deleted.
function rowClock(raw, perRowMs, chunks) {
    const left = () => raw.prepare('SELECT count(*) AS n FROM security_events').get().n;
    let t = 0, last = left();
    return () => {
        const n = left();
        if (n !== last) chunks.push(last - n);
        t += (last - n) * perRowMs;
        last = n;
        return t;
    };
}

test('retention.runAsync adapts its chunk so that one statement takes about half a slice, at most 1000 rows', async (t) => {
    const { store, raw } = tempStore(t);
    const a = store.users.create({ username: 'Dan', email: 'dan@example.org' });
    const seed = () => store.security.insertBatch(Array.from({ length: 3000 }, () => ({ kind: 'login_failed', userId: a, ip: null, at: NOW - 100 * DAY })));
    const noPause = async () => {};

    // Slow statements (0.1 ms per row, 10 ms slices): the first chunk of 200 rows takes 20 ms,
    // four times the 5 ms aimed at, so the next ones take 50 rows (5 ms).
    seed();
    let chunks = [];
    let counts = await store.retention.runAsync(NOW, undefined, { sliceMs: 10, pause: noPause, clock: rowClock(raw, 0.1, chunks) });
    assert.equal(counts.securityEvents, 3000);
    assert.equal(chunks[0], 200);
    assert.ok(chunks.slice(1).every((n) => n === 50), `chunks ${chunks}`);

    // Fast statements: the chunk doubles up to 1000 rows, never more.
    seed();
    chunks = [];
    counts = await store.retention.runAsync(NOW, undefined, { sliceMs: 10, pause: noPause, clock: rowClock(raw, 0.001, chunks) });
    assert.equal(counts.securityEvents, 3000);
    assert.deepEqual(chunks, [200, 400, 800, 1000, 600]);

    // Very slow statements: never below 50 rows.
    seed();
    chunks = [];
    await store.retention.runAsync(NOW, undefined, { sliceMs: 10, pause: noPause, clock: rowClock(raw, 5, chunks) });
    assert.deepEqual(chunks.slice(0, 3), [200, 50, 50]);
    assert.equal(chunks.reduce((x, y) => x + y, 0), 3000);
});

test('retention.runAsync pauses for sliceMs between slices, so that other writers get the lock', async (t) => {
    const { store } = tempStore(t);
    // A clock on which every statement takes a whole slice: the run pauses after each of its 9
    // statements (nothing to purge), for sliceMs each time (a timer, not just a turn of the loop).
    let fake = 0;
    const t0 = performance.now();
    await store.retention.runAsync(NOW, undefined, { sliceMs: 25, clock: () => (fake += 12.5) });
    const took = performance.now() - t0;
    assert.ok(took >= 9 * 25 - 9, `9 pauses of 25 ms took ${took.toFixed(1)} ms`);
});

test('erased IPs, deleted sessions and anonymized e-mail addresses do not stay readable in the database file', async (t) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-secdel-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const file = path.join(dir, 'secdel.db');
    const store = openStore(testConfig({ DB_PATH: file, RETENTION_SECURITY_DAYS: '90', RETENTION_IP_DAYS: '30' }), { applyGame });
    migrate(store);
    const raw = new DatabaseSync(file);
    const ip = (k, i) => `198.18.${k}.${i}`;     // marker addresses (a range reserved for benchmarks)
    const a = store.users.create({ username: 'Eva', email: 'eva@example.org' });
    for (let i = 0; i < 250; i++) {
        // Live sessions older than RETENTION_IP_DAYS (IP erased in place) and expired ones (rows
        // deleted, whole pages freed).
        store.sessions.create({ userId: a, tokenHash: `live-${i}`, createdAt: NOW - 40 * DAY, expiresAt: NOW + DAY, idleExpiresAt: NOW + DAY, ip: ip(1, i) });
        store.sessions.create({ userId: a, tokenHash: `dead-${i}`, createdAt: NOW - 40 * DAY, expiresAt: NOW - 1, idleExpiresAt: NOW - 1, ip: ip(2, i) });
    }
    // Sessions that expired before their IP was due for erasure: deleted with it (with
    // secure_delete FAST, the freed pages would keep them).
    for (let i = 0; i < 250; i++) {
        store.sessions.create({ userId: a, tokenHash: `short-${i}`, createdAt: NOW - 10 * DAY, expiresAt: NOW - 1, idleExpiresAt: NOW - 1, ip: ip(5, i) });
    }
    store.security.insertBatch(Array.from({ length: 250 }, (_, i) => ({ kind: 'login', userId: a, ip: ip(3, i), at: NOW - 100 * DAY })));
    store.security.insertBatch(Array.from({ length: 250 }, (_, i) => ({ kind: 'login', userId: a, ip: ip(4, i), at: NOW - 40 * DAY })));
    const gone = store.users.create({ username: 'Gus', email: 'gus.secret-marker@example.org' });
    raw.exec('PRAGMA wal_checkpoint(TRUNCATE)');    // the rows are in the main file now
    const counts = await store.retention.runAsync(NOW, undefined, { sliceMs: 1000 });
    assert.deepEqual([counts.sessions, counts.securityEvents, counts.ipErased], [500, 250, 1000]);
    store.users.anonymize(gone, NOW);
    raw.close();
    store.close();                                  // the last connection: checkpointed, WAL removed
    let bytes = fs.readFileSync(file).toString('latin1');
    if (fs.existsSync(file + '-wal')) bytes += fs.readFileSync(file + '-wal').toString('latin1');
    assert.equal((bytes.match(/198\.18\.\d+\.\d+/g) || []).length, 0, 'no erased or deleted IP address left in the file');
    assert.equal(bytes.includes('secret-marker'), false, 'no anonymized e-mail address left in the file');
});
