import test from 'node:test';
import assert from 'node:assert/strict';
import { testConfig } from '../../src/config.js';
import { metrics } from '../../src/metrics.js';
import { createAnticheat, classify, ANOMALY_KINDS } from '../../src/anticheat/index.js';
import { createFakeStore } from '../../src/anticheat/testing/fake-store.js';

function fakeLog() {
    const events = [];
    return { events, debug() {}, info() {}, warn() {}, error(msg, f) { events.push(['error', msg, f]); }, security(ev, f) { events.push(['security', ev, f]); }, child() { return this; } };
}

function counterValue(kind, severity) {
    const snap = metrics.snapshot().find((m) => m.name === 'scacelith_anticheat_anomalies_total');
    const c = snap?.children.find((x) => x.l[0] === kind && x.l[1] === severity);
    return c ? c.v : 0;
}

test('classification table matches DESIGN 6.5', () => {
    const expected = {
        malformed: 'suspicious', forged_type: 'certain', bad_seq: 'suspicious', flood: 'suspicious',
        foreign_game: 'certain', out_of_turn: 'certain', illegal_move: 'certain', repeated_desync: 'suspicious',
        clock_implausible: 'suspicious', stale_ply: 'info', desync: 'info', nothing_to_claim: 'info',
    };
    assert.deepEqual({ ...ANOMALY_KINDS }, expected);
    for (const [kind, severity] of Object.entries(expected)) {
        const c = classify(kind, { posMatched: true });
        assert.equal(c.severity, severity, kind);
        assert.equal(c.certain, severity === 'certain', kind);
    }
});

test('unknown kinds are info; position-dependent cheats need a synchronised position', () => {
    assert.deepEqual(classify('made_up'), { severity: 'info', certain: false, known: false });
    assert.equal(classify('toString').severity, 'info');
    assert.equal(classify('illegal_move', { posMatched: false }).severity, 'suspicious');
    assert.equal(classify('out_of_turn', { posMatched: false }).certain, false);
    assert.equal(classify('illegal_move').certain, true);          // host did not say: as in 6.2
    assert.equal(classify('forged_type', { posMatched: false }).certain, true);   // not position dependent
});

test('non-certain anomalies are batched, coalesced, and flushed on the timer', async () => {
    const store = createFakeStore();
    const log = fakeLog();
    const ac = createAnticheat({ config: testConfig(), store, log, flushMs: 30 });
    const before = counterValue('bad_seq', 'suspicious');
    assert.deepEqual(ac.recordAnomaly({ userId: 7, gameId: 11, kind: 'bad_seq', detail: { expected: 4, got: 9 } }), { severity: 'suspicious', certain: false });
    ac.recordAnomaly({ userId: 7, gameId: 11, kind: 'bad_seq' });
    ac.recordAnomaly({ userId: 7, gameId: 11, kind: 'bad_seq' });
    ac.recordAnomaly({ userId: 7, gameId: 11, kind: 'stale_ply' });
    ac.recordAnomaly({ userId: 8, gameId: 11, kind: 'desync', posMatched: false });
    assert.equal(store._.anomalies.length, 0, 'nothing written before the flush');
    assert.equal(ac.pendingCount, 3);
    assert.equal(ac.pendingSignalCount, 1, 'bad_seq; stale_ply and desync are info');
    assert.equal(counterValue('bad_seq', 'suspicious') - before, 3);
    await new Promise((r) => setTimeout(r, 80));
    const batches = store.calls.filter((c) => c.name === 'anomalies.insertBatch');
    assert.equal(batches.length, 1, 'one insertBatch per period');
    assert.deepEqual([ac.pendingCount, ac.pendingSignalCount], [0, 0]);
    assert.equal(store._.anomalies.length, 3);
    const seq = store._.anomalies.find((a) => a.kind === 'bad_seq');
    assert.equal(seq.detail.count, 3);
    assert.equal(seq.detail.expected, 4);
    assert.equal(store._.anomalies.find((a) => a.kind === 'desync').detail.posMatched, false);
    assert.ok(log.events.some(([k, ev, f]) => k === 'security' && ev === 'anomaly' && f.kind === 'bad_seq'));
    assert.ok(!log.events.some(([, , f]) => f?.kind === 'stale_ply'), 'info anomalies are not security-logged');
    ac.close();
});

test('certain anomalies are written immediately', () => {
    const store = createFakeStore();
    const ac = createAnticheat({ config: testConfig(), store, log: fakeLog(), flushMs: 60000 });
    const r = ac.recordAnomaly({ userId: 3, gameId: 99, kind: 'illegal_move', detail: 'e1e8', posMatched: true });
    assert.deepEqual(r, { severity: 'certain', certain: true });
    assert.equal(store._.anomalies.length, 1);
    assert.deepEqual(store._.anomalies[0].detail, { info: 'e1e8', posMatched: true });
    assert.equal(store._.anomalies[0].severity, 'certain');
    ac.close();
});

test('stores that want text columns get JSON text', () => {
    const store = createFakeStore({ textColumns: true });
    const ac = createAnticheat({ config: testConfig(), store, log: fakeLog() });
    ac.recordAnomaly({ userId: 3, gameId: 1, kind: 'forged_type', detail: { type: 0x80 } });
    assert.equal(typeof store._.anomalies[0].detail, 'string');
    assert.deepEqual(JSON.parse(store._.anomalies[0].detail), { type: 0x80 });
    ac.recordAnomaly({ userId: 3, gameId: 1, kind: 'flood' });
    assert.equal(ac.flush(), 1);
    ac.close();
});

test('unknown kinds are recorded as info under the label unknown', () => {
    const store = createFakeStore();
    const ac = createAnticheat({ config: testConfig(), store, log: fakeLog() });
    ac.recordAnomaly({ userId: 1, gameId: 0, kind: 'weird\nkind' });
    ac.flush();
    assert.equal(store._.anomalies[0].kind, 'unknown');
    assert.equal(store._.anomalies[0].severity, 'info');
    assert.equal(store._.anomalies[0].detail.reportedKind, 'weird\nkind');
    ac.close();
});

test('a failing store loses the batch without throwing', () => {
    const store = createFakeStore();
    store.anomalies.insertBatch = () => { throw new Error('SQLITE_BUSY'); };
    const log = fakeLog();
    const ac = createAnticheat({ config: testConfig(), store, log });
    assert.doesNotThrow(() => ac.recordAnomaly({ userId: 1, gameId: 2, kind: 'out_of_turn' }));
    ac.recordAnomaly({ userId: 1, gameId: 2, kind: 'flood' });
    assert.equal(ac.flush(), 0);
    assert.ok(log.events.filter(([k]) => k === 'error').length >= 2);
    ac.close();
});
