import test from 'node:test';
import assert from 'node:assert/strict';
import { testConfig } from '../../src/config.js';
import { createAnticheat, startAnalysisProcess } from '../../src/anticheat/index.js';
import { createFakeStore } from '../../src/anticheat/testing/fake-store.js';

const quiet = { debug() {}, info() {}, warn() {}, error() {}, security() {}, child() { return this; } };

function fakePrimary() {
    const sent = [];
    return { sent, request: async (type, payload) => { sent.push({ type, payload }); return {}; } };
}

test('certain cheat: one ban per game, integrity confirmed with evidence, primary notified once', async () => {
    const store = createFakeStore();
    const primary = fakePrimary();
    let t = 1_800_000_000_000;
    const config = testConfig({ BAN_DURATION_HOURS: '48' });
    const ac = createAnticheat({ config, store, primary, log: quiet, now: () => t });
    const a = ac.sanctionCertain({ userId: 5, gameId: 77, kind: 'illegal_move' });
    assert.equal(a.banUntil, t + 48 * 3600000);
    assert.equal(a.applied, true);
    t += 10;
    const b = ac.sanctionCertain({ userId: 5, gameId: 77, kind: 'out_of_turn' });
    const c = ac.sanctionCertain({ userId: 5, gameId: 77, kind: 'illegal_move' });
    assert.equal(b.banUntil, a.banUntil);
    assert.equal(c.applied, false);
    assert.equal(store._.sanctions.length, 1);
    const ban = store._.sanctions[0];
    assert.equal(ban.kind, 'ban');
    assert.equal(ban.source, 'auto');
    assert.equal(ban.gameId, 77);
    assert.equal(ban.reason, 'certain_cheat:illegal_move');
    const integ = store.integrity.get(5);
    assert.equal(integ.level, 'confirmed');
    assert.equal(integ.evidence.certain.length, 1);
    assert.deepEqual({ ...integ.evidence.certain[0], at: 0 }, { kind: 'illegal_move', gameId: 77, at: 0, banUntil: a.banUntil });
    await new Promise((r) => setImmediate(r));
    assert.deepEqual(primary.sent, [{ type: 'sanction.applied', payload: { userId: 5, until: a.banUntil, reason: 'certain_cheat:illegal_move', refunds: 0 } }]);
    assert.equal(store._.security.filter((e) => e.kind === 'sanction_auto').length, 1);
    ac.close();
});

test('idempotency across processes: an existing ban of the same game is reused', async () => {
    const store = createFakeStore();
    const t = 1_800_000_000_000;
    const config = testConfig();
    const shard1 = createAnticheat({ config, store, primary: fakePrimary(), log: quiet, now: () => t });
    const primary2 = fakePrimary();
    const shard2 = createAnticheat({ config, store, primary: primary2, log: quiet, now: () => t + 5 });
    const a = shard1.sanctionCertain({ userId: 9, gameId: 1, kind: 'forged_type' });
    const b = shard2.sanctionCertain({ userId: 9, gameId: 1, kind: 'foreign_game' });
    assert.equal(store._.sanctions.length, 1);
    assert.equal(b.banUntil, a.banUntil);
    assert.equal(b.applied, false);
    await new Promise((r) => setImmediate(r));
    assert.equal(primary2.sent.length, 0);
    // Another game is another offence: a new ban (ending later) is created.
    const c = shard2.sanctionCertain({ userId: 9, gameId: 2, kind: 'illegal_move' });
    assert.equal(c.applied, true);
    assert.equal(store._.sanctions.length, 2);
    assert.equal(store.integrity.get(9).evidence.certain.length, 3, 'every sanction call adds evidence');
});

test('a longer ban stands for the automatic one only when it is a ban for cheating that refunds', () => {
    const t = 1_800_000_000_000;
    const long = { kind: 'ban', source: 'moderator', gameId: null, startsAt: t - 1000, endsAt: t + 30 * 24 * 3600000, createdBy: 'mod' };
    for (const [reason, reused] of [['confirmed: engine', true], ['confirmed, no refund: engine', false], ['abusive chat', false]]) {
        const store = createFakeStore();
        store.sanctions.create({ userId: 3, reason, ...long });
        const ac = createAnticheat({ config: testConfig(), store, log: quiet, now: () => t });
        const r = ac.sanctionCertain({ userId: 3, gameId: 8, kind: 'illegal_move' });
        assert.deepEqual([r.applied, store._.sanctions.length], reused ? [false, 1] : [true, 2], reason);
        assert.equal(r.banUntil, reused ? long.endsAt : t + 24 * 3600000, reason);
        ac.close();
    }
});

test('a new game after the ban expired gets a new ban', () => {
    const store = createFakeStore();
    let t = 1_800_000_000_000;
    const ac = createAnticheat({ config: testConfig({ BAN_DURATION_HOURS: '1' }), store, log: quiet, now: () => t });
    ac.sanctionCertain({ userId: 4, gameId: 10, kind: 'illegal_move' });
    t += 2 * 3600000;
    const r = ac.sanctionCertain({ userId: 4, gameId: 11, kind: 'illegal_move' });
    assert.equal(r.applied, true);
    assert.equal(store._.sanctions.length, 2);
});

test('AUTO_SANCTION_CERTAIN_CHEATS=false: nothing happens', () => {
    const store = createFakeStore();
    const ac = createAnticheat({ config: testConfig({ AUTO_SANCTION_CERTAIN_CHEATS: 'false' }), store, primary: fakePrimary(), log: quiet });
    assert.deepEqual(ac.sanctionCertain({ userId: 1, gameId: 2, kind: 'illegal_move' }), { banUntil: 0, applied: false, refunds: 0 });
    assert.equal(store._.sanctions.length, 0);
    assert.equal(store.integrity.get(1), null);
});

test('confirmed keeps the statistical evidence already present', () => {
    const store = createFakeStore();
    store.integrity.set(3, { level: 'suspected', score: 3.9, evidence: { statistics: { score: 3.9 } }, updatedAt: 1 });
    const ac = createAnticheat({ config: testConfig(), store, log: quiet });
    ac.sanctionCertain({ userId: 3, gameId: 5, kind: 'out_of_turn' });
    const i = store.integrity.get(3);
    assert.equal(i.level, 'confirmed');
    assert.equal(i.score, 3.9);
    assert.equal(i.evidence.statistics.score, 3.9);
});

test('a rejected sanction.applied request is logged, not thrown', async () => {
    const store = createFakeStore();
    const warns = [];
    const log = { ...quiet, warn: (m) => warns.push(m) };
    const ac = createAnticheat({ config: testConfig(), store, log, primary: { request: async () => { throw new Error('timeout'); } } });
    ac.sanctionCertain({ userId: 1, gameId: 1, kind: 'forged_type' });
    await new Promise((r) => setTimeout(r, 5));
    assert.deepEqual(warns, ['sanction.applied not delivered']);
});

test('startAnalysisProcess is disabled without an engine or workers', async () => {
    const h1 = startAnalysisProcess(testConfig(), { log: quiet });
    assert.equal(h1.enabled, false);
    await h1.stop();
    const h2 = startAnalysisProcess(testConfig({ ANALYSIS_ENGINE_PATH: '/usr/games/stockfish', ANALYSIS_WORKERS: '0' }), { log: quiet });
    assert.equal(h2.enabled, false);
    assert.equal(await h2.metricsSnapshot(), null);
});

test('startAnalysisProcess serves the metrics of the analysis process to the primary', async (t) => {
    const fs = await import('node:fs');
    const os = await import('node:os');
    const path = await import('node:path');
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ac-proc-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    // The IPC side of bin/analysis-worker.js, with one gauge of its own.
    const script = path.join(dir, 'metrics.mjs');
    fs.writeFileSync(script, `
import { Ipc } from ${JSON.stringify(new URL('../../src/cluster/ipc.js', import.meta.url).href)};
import { Registry } from ${JSON.stringify(new URL('../../src/metrics.js', import.meta.url).href)};
const registry = new Registry();
registry.gauge('scacelith_anticheat_analysis_engines_shared', 'test').set(2);
const ipc = new Ipc(process);
ipc.on('metrics.snapshot', () => registry.snapshot());
process.on('message', (m) => { if (m.type === 'shutdown') { ipc.close(); process.disconnect(); } });
`);
    const h = startAnalysisProcess(testConfig({ ANALYSIS_ENGINE_PATH: '/bin/true' }), { log: quiet, script });
    let snapshot = null;
    for (const end = Date.now() + 10000; !snapshot && Date.now() < end;) snapshot = await h.metricsSnapshot(500);
    assert.deepEqual(snapshot.map((m) => [m.name, m.children[0].v]), [['scacelith_anticheat_analysis_engines_shared', 2]]);
    await h.stop(3000);
    assert.equal(await h.metricsSnapshot(), null, 'no process, no metrics');
});

test('startAnalysisProcess restarts a crashing worker with backoff and stops cleanly', async (t) => {
    const fs = await import('node:fs');
    const os = await import('node:os');
    const path = await import('node:path');
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ac-proc-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const crash = path.join(dir, 'crash.mjs');
    fs.writeFileSync(crash, 'process.exit(3);\n');
    const h = startAnalysisProcess(testConfig({ ANALYSIS_ENGINE_PATH: '/bin/true' }), { log: quiet, script: crash, minBackoffMs: 20, maxBackoffMs: 80 });
    assert.equal(h.enabled, true);
    // Each restart forks a Node process: poll instead of a fixed delay (slow on a loaded machine).
    for (const end = Date.now() + 10000; h.restarts < 3 && Date.now() < end;) await new Promise((r) => setTimeout(r, 50));
    assert.ok(h.restarts >= 3, `restarted ${h.restarts} times`);
    await h.stop(500);
    const n = h.restarts;
    await new Promise((r) => setTimeout(r, 200));
    assert.equal(h.restarts, n, 'no restart after stop');

    const idle = path.join(dir, 'idle.mjs');
    fs.writeFileSync(idle, "process.on('message', (m) => { if (m.type === 'shutdown') process.disconnect(); });\n");
    const h2 = startAnalysisProcess(testConfig({ ANALYSIS_ENGINE_PATH: '/bin/true' }), { log: quiet, script: idle });
    await new Promise((r) => setTimeout(r, 150));
    assert.ok(h2.pid > 0);
    const t0 = Date.now();
    await h2.stop(3000);
    assert.ok(Date.now() - t0 < 2500, 'graceful shutdown through IPC');
    assert.equal(h2.pid, null);
});
