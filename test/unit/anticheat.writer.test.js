// The anti-cheat's writes in a shard go through the store writer thread (src/store/writer.js):
// nothing on the event loop's connection, and in order with the finished-game commits (the
// anomalies flushed right before a commit are written before it, DESIGN 6.5).

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { createAnticheat } from '../../src/anticheat/index.js';
import { testConfig } from '../../src/config.js';
import { metrics } from '../../src/metrics.js';
import { enums } from '../../src/protocol/schema.js';
import { openStore, migrate, AnalysisPriority } from '../../src/store/index.js';
import { startStoreWriter } from '../../src/store/writer.js';

const { GameStatus, EndReason } = enums;
const T = Date.UTC(2026, 8, 1, 12);
const quiet = { debug() {}, info() {}, warn() {}, error() {}, security() {}, child() { return this; } };

let nextId = 8_000_000_000_000;
function record(white, black, extra = {}) {
    const plies = 40;
    return {
        id: ++nextId, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: white, blackId: black, whiteName: 'W',
        blackName: 'B', startedAt: T - 600000, endedAt: T, status: GameStatus.WhiteWins, reason: EndReason.Resignation,
        moves: new Uint16Array(plies), spentMs: new Uint32Array(plies), clockMs: new Uint32Array(plies), ...extra,
    };
}

// The shard's own store must not be written by the anti-cheat when it has a writer.
function readOnlyView(store) {
    const refuse = () => { throw new Error('written on the event loop\'s connection'); };
    return { ...store, anomalies: { insertBatch: refuse }, sanctions: { create: refuse, activeBan: refuse, active: refuse }, integrity: { get: refuse, set: refuse } };
}

function setup(t) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-acwriter-'));
    const config = testConfig({ DB_PATH: path.join(dir, 'db.sqlite'), DATA_DIR: dir });
    const store = openStore(config);
    migrate(store);
    const writer = startStoreWriter({ config, logging: false });
    t.after(async () => {
        await writer.close();
        store.close();
        fs.rmSync(dir, { recursive: true, force: true });
    });
    const [a, b] = ['Ann', 'Ben'].map((n) => store.users.create({ username: n, email: `${n}@example.org` }));
    return { config, store, writer, a, b };
}

test('anomalies flushed before a commit are written by the thread before it: the analysis queue sees them', async (t) => {
    const { config, store, writer, a, b } = setup(t);
    const ac = createAnticheat({ config, store: readOnlyView(store), writer, log: quiet, now: () => T });
    const flagged = record(a, b), plain = record(b, a);
    ac.recordAnomaly({ userId: a, gameId: flagged.id, kind: 'clock_implausible', detail: { thinkMs: 1 } });
    assert.equal(ac.pendingSignalCount, 1);
    // What GameHost does: flush (handed to the thread), then the commit, without waiting in between.
    assert.equal(ac.flush(), 1);
    const res = await writer.finishBatch([flagged, plain]);
    assert.equal(res.length, 2);
    const jobs = new Map(store.analysis.next(10, 'w', T).map((j) => [j.gameId, j.priority]));
    assert.equal(jobs.get(flagged.id), AnalysisPriority.signal, 'the anomaly was in the database when the game was committed');
    assert.equal(jobs.get(plain.id), AnalysisPriority.ordinary);
    assert.equal(store.anomalies.forUser(a, 10)[0].kind, 'clock_implausible');
    ac.close();
});

test('a certain cheat: anomaly, ban, integrity and refunds written by the thread; one ban per game while it answers', async (t) => {
    const { config, store, writer, a, b } = setup(t);
    // Rated records (K 20) so that the game below moves both ratings by the K formula.
    const raw = new DatabaseSync(config.dbPath);
    const seed = raw.prepare(`INSERT INTO ratings (user_id, category, rating, games, peak, rated, updated_at) VALUES (?, '3+2', 1500, 40, 1500, 1, 0)`);
    for (const u of [a, b]) seed.run(u);
    raw.close();
    await writer.finishBatch([record(a, b, { endedAt: T - 86400000 })]);      // Ann (the cheater) beats Ben: Ben -10
    assert.equal(store.ratings.get(b, '3+2').rating, 1490);

    const sent = [];
    const primary = { request: async (type, payload) => { sent.push({ type, payload }); return {}; } };
    const ac = createAnticheat({ config, store: readOnlyView(store), writer, primary, log: quiet, now: () => T });
    assert.equal(ac.recordAnomaly({ userId: a, gameId: 42, kind: 'illegal_move', posMatched: true }).certain, true);
    const p = ac.sanctionCertain({ userId: a, gameId: 42, kind: 'illegal_move' });
    assert.equal(typeof p.then, 'function', 'the thread answers later');
    assert.deepEqual(ac.sanctionCertain({ userId: a, gameId: 42, kind: 'out_of_turn' }), { banUntil: T + config.banDurationHours * 3600000, applied: false, refunds: 0 });
    assert.deepEqual(await p, { banUntil: T + config.banDurationHours * 3600000, applied: true, refunds: 1 });

    assert.equal(store.anomalies.forUser(a, 10)[0].severity, 'certain');
    const ban = store.sanctions.activeBan(a, T);
    assert.equal(ban.reason, 'certain_cheat:illegal_move');
    assert.equal(store.sanctions.list(a).length, 1);
    assert.equal(store.integrity.get(a).level, 'confirmed');
    assert.equal(store.ratings.get(b, '3+2').rating, 1500, 'Ben got his 10 points back');
    assert.deepEqual(store.refunds.list().map((r) => [r.victimId, r.points, r.sanctionId]), [[b, 10, ban.id]]);
    await new Promise((r) => setImmediate(r));
    assert.deepEqual(sent.map((s) => s.payload.refunds), [1]);
    ac.close();
});

test('a batch the thread cannot receive is counted as lost; the next ones are written', async (t) => {
    const { config, store, writer, a } = setup(t);
    const dropped = () => metrics.metrics.get('scacelith_anticheat_anomalies_dropped_total')?.root.value ?? 0;
    const before = dropped();
    const ac = createAnticheat({ config, store: readOnlyView(store), writer, log: quiet, now: () => T });
    ac.recordAnomaly({ userId: a, gameId: 1, kind: 'bad_seq', detail: { callback: () => 1 } });   // not cloneable
    assert.equal(ac.flush(), 1);
    ac.recordAnomaly({ userId: a, gameId: 2, kind: 'flood' });
    assert.equal(ac.flush(), 1);
    await writer.insertAnomalies([]);                  // the thread has handled everything sent before
    assert.equal(dropped() - before, 1);
    assert.deepEqual(store.anomalies.forUser(a, 10).map((x) => x.kind), ['flood']);
    ac.close();
});

test('a writer that fails: the anomalies are counted as lost, the sanction can be tried again', async () => {
    const dropped = () => metrics.metrics.get('scacelith_anticheat_anomalies_dropped_total')?.root.value ?? 0;
    const errors = [];
    const log = { ...quiet, error: (m) => errors.push(m) };
    const writer = { insertAnomalies: () => Promise.reject(new Error('exited')), sanction: () => Promise.reject(new Error('exited')) };
    const ac = createAnticheat({ config: testConfig(), store: {}, writer, log, now: () => T });
    const before = dropped();
    ac.recordAnomaly({ userId: 1, gameId: 2, kind: 'bad_seq' });
    ac.flush();
    const r = await ac.sanctionCertain({ userId: 1, gameId: 2, kind: 'forged_type' });
    assert.deepEqual(r, { banUntil: 0, applied: false, refunds: 0 });
    assert.equal(dropped() - before, 1);
    assert.deepEqual(errors, ['anomaly batch lost', 'automatic ban not stored']);
    // Not remembered as sanctioned: the next certain anomaly of the game asks the writer again.
    let asked = 0;
    writer.sanction = async (s) => { asked++; return { until: s.at + 1000, created: true, sanctionId: 5, refunds: [] }; };
    assert.deepEqual(await ac.sanctionCertain({ userId: 1, gameId: 2, kind: 'forged_type' }), { banUntil: T + 1000, applied: true, refunds: 0 });
    assert.equal(asked, 1);
    ac.close();
});
