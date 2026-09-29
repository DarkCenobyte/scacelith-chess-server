// Timer wiring of the primary's retention purge (src/cluster/retention.js): first run about a
// minute after the start, then every RETENTION_INTERVAL_MS after the previous run ended, never two
// at once, counts logged and counted, a clean stop that waits for the run in progress, and
// nothing after store.close().

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { startRetention } from '../../src/cluster/retention.js';
import { Registry } from '../../src/metrics.js';
import { configureLogging, logger } from '../../src/log.js';
import { openStore, migrate } from '../../src/store/index.js';
import { testConfig, CONFIG_KEYS } from '../../src/config.js';

const DAY = 86400000;
const ZERO = { sessions: 0, tokens: 0, securityEvents: 0, anomalies: 0, conductEvents: 0, analysisJobs: 0, ipErased: 0 };

function fakeTimers() {
    const pending = [];
    return {
        pending,
        setTimeout(fn, ms) { const h = { fn, ms, cleared: false, unref() { this.unrefed = true; return this; } }; pending.push(h); return h; },
        clearTimeout(h) { if (h) h.cleared = true; },
        /** Runs the oldest live timer; returns its delay. */
        fire() {
            const i = pending.findIndex((h) => !h.cleared);
            assert.ok(i >= 0, 'a timer is pending');
            const [h] = pending.splice(i, 1);
            h.fn();
            return h.ms;
        },
        live() { return pending.filter((h) => !h.cleared); },
    };
}

// store.retention.runAsync whose runs the test resolves or rejects by hand.
function fakeStore() {
    const calls = [];
    return {
        calls,
        retention: {
            runAsync(now, cfg, opts) {
                let resolve, reject;
                const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
                calls.push({ now, cfg, opts, resolve, reject });
                return promise;
            },
        },
    };
}

function fakeLog() {
    const lines = [];
    const at = (level) => (msg, fields) => lines.push({ level, msg, fields });
    return { lines, info: at('info'), warn: at('warn'), error: at('error'), debug: at('debug') };
}

const value = (registry, name, ...labels) => registry.metrics.get(name)?.children.get(labels.join('\u0001'))?.value ?? 0;
const settle = () => new Promise((resolve) => setImmediate(resolve));

test('first run a minute after the start, then every RETENTION_INTERVAL_MS after the previous one ends', async (t) => {
    const config = testConfig({ RETENTION_INTERVAL_MS: '120000' });
    // The real logger (with its redaction of credential-like field names), writing to an array.
    const out = [];
    configureLogging({ level: 'info', format: 'json', out: { write: (s) => out.push(s) } });
    t.after(() => configureLogging({ level: 'error' }));
    const timers = fakeTimers(), store = fakeStore(), registry = new Registry();
    const r = startRetention({ config, store, log: logger.child('retention'), registry, timers, now: () => 1234 });
    assert.equal(timers.live().length, 1);
    assert.equal(timers.live()[0].ms, 60000);
    assert.ok(timers.live()[0].unrefed, 'the timer never keeps the process alive');
    assert.equal(store.calls.length, 0);

    assert.equal(timers.fire(), 60000);
    assert.equal(store.calls.length, 1);
    assert.equal(store.calls[0].now, 1234);
    assert.equal(store.calls[0].cfg, config);
    assert.equal(store.calls[0].opts.sliceMs, 10);
    assert.ok(r.running);
    assert.equal(timers.live().length, 0, 'the next run is scheduled only when this one ends');

    store.calls[0].resolve({ ...ZERO, sessions: 3, tokens: 2, securityEvents: 5, analysisJobs: 1, ipErased: 4 });
    await settle();
    assert.equal(r.running, false);
    assert.equal(timers.live().length, 1);
    assert.equal(timers.live()[0].ms, 120000);
    // One info line with the counts only: numbers, no personal data, none of them redacted.
    const done = out.map((s) => JSON.parse(s)).find((l) => l.msg === 'retention purge done');
    assert.equal(done.level, 'info');
    const { t: _t, level: _l, c: _c, msg: _m, ...fields } = done;
    assert.deepEqual(fields, { sessions: 3, singleUse: 2, securityEvents: 5, anomalies: 0, conductEvents: 0, analysisJobs: 1, ipErased: 4, ms: fields.ms });
    assert.equal(typeof fields.ms, 'number');
    assert.equal(value(registry, 'scacelith_retention_purged_total', 'sessions'), 3);
    assert.equal(value(registry, 'scacelith_retention_purged_total', 'tokens'), 2);
    assert.equal(value(registry, 'scacelith_retention_purged_total', 'security_events'), 5);
    assert.equal(value(registry, 'scacelith_retention_purged_total', 'analysis_jobs'), 1);
    assert.equal(value(registry, 'scacelith_retention_ip_erased_total'), 4);
    assert.equal(value(registry, 'scacelith_retention_runs_total', 'ok'), 1);

    timers.fire();
    assert.equal(store.calls.length, 2);
    store.calls[1].resolve(ZERO);
    await settle();
    assert.equal(value(registry, 'scacelith_retention_runs_total', 'ok'), 2);
    await r.stop();
    assert.equal(timers.live().length, 0);
});

test('RETENTION_INTERVAL_MS: default one hour, at least one minute, at most the longest Node.js timer', () => {
    assert.equal(testConfig().retentionIntervalMs, 3600000);
    assert.throws(() => testConfig({ RETENTION_INTERVAL_MS: '59999' }), /RETENTION_INTERVAL_MS: at least 60000/);
    // 30 days would overflow setTimeout, which then fires after 1 ms: the purge would run in a loop.
    assert.throws(() => testConfig({ RETENTION_INTERVAL_MS: '2592000000' }), /RETENTION_INTERVAL_MS: at most 2147483647/);
    assert.equal(testConfig({ RETENTION_INTERVAL_MS: '2147483647' }).retentionIntervalMs, 2147483647);
});

test('an interval beyond the longest timer (a configuration not checked by loadConfig) is clamped, never a 1 ms loop', async () => {
    const timers = fakeTimers(), store = fakeStore(), registry = new Registry();
    const config = { ...testConfig(), retentionIntervalMs: 30 * DAY };
    startRetention({ config, store, registry, timers });
    timers.fire();
    store.calls[0].resolve(ZERO);
    await settle();
    assert.equal(timers.live().length, 1);
    assert.equal(timers.live()[0].ms, 2147483647);
});

// Every key whose value reaches a Node.js timer (setTimeout / setInterval) is bounded below the
// longest delay (2^31 - 1 ms), since a longer one fires after 1 ms instead.
test('configuration keys that reach a timer are bounded (loadConfig refuses an overflowing value)', () => {
    const TIMER_KEYS = ['RETENTION_INTERVAL_MS', 'ANALYSIS_POLL_MS', 'ANALYSIS_POSITION_TIMEOUT_MS', 'WS_HELLO_TIMEOUT_MS',
        'MATCH_TICK_MS', 'SHUTDOWN_GRACE_MS', 'JOURNAL_FLUSH_MS', 'DB_COMMIT_MS', 'PASSWORD_HASH_QUEUE_TIMEOUT_MS',
        'CLIENT_PING_INTERVAL_MS'];
    for (const name of TIMER_KEYS) {
        const k = CONFIG_KEYS.find((x) => x.name === name);
        assert.ok(k, name);
        assert.ok(Number.isInteger(k.max) && k.max <= 2147483647, `${name} has a max below 2^31 (${k.max})`);
        assert.throws(() => testConfig({ [name]: String(k.max + 1) }), new RegExp(`${name}: at most ${k.max}`));
    }
});

test('runs never overlap: runNow waits for the run in progress, the timer waits for runNow', async () => {
    const timers = fakeTimers(), store = fakeStore(), registry = new Registry();
    const r = startRetention({ config: testConfig(), store, registry, timers });
    timers.fire();
    const manual = r.runNow();
    await settle();
    assert.equal(store.calls.length, 1, 'runNow waits');
    store.calls[0].resolve(ZERO);
    await settle();
    assert.equal(store.calls.length, 2, 'then runs');
    // The interval timer fires while runNow's run is in progress: it does not start another one.
    timers.fire();
    assert.equal(store.calls.length, 2);
    store.calls[1].resolve({ ...ZERO, conductEvents: 7 });
    assert.deepEqual(await manual, { ...ZERO, conductEvents: 7 });
    await settle();
    assert.equal(timers.live().length, 1, 'one next run scheduled');
    await r.stop();
});

test('a failed run is counted and logged, with the partial counts, and the next one still happens', async () => {
    const timers = fakeTimers(), store = fakeStore(), log = fakeLog(), registry = new Registry();
    const r = startRetention({ config: testConfig(), store, log, registry, timers });
    timers.fire();
    store.calls[0].reject(Object.assign(new Error('database is locked'), { code: 'busy', counts: { ...ZERO, sessions: 1000 } }));
    await settle();
    assert.equal(value(registry, 'scacelith_retention_runs_total', 'failed'), 1);
    assert.equal(value(registry, 'scacelith_retention_purged_total', 'sessions'), 1000);
    const line = log.lines.find((l) => l.msg.startsWith('retention purge failed'));
    assert.equal(line.level, 'warn');
    assert.equal(timers.live().length, 1);
    timers.fire();
    store.calls[1].reject(new Error('disk I/O error'));
    await settle();
    assert.equal(log.lines.filter((l) => l.msg.startsWith('retention purge failed')).at(-1).level, 'error');
    assert.equal(value(registry, 'scacelith_retention_runs_total', 'failed'), 2);
    await r.stop();
});

test('stop() cancels the first run, aborts a run in progress and waits for it; nothing runs afterwards', async () => {
    // Before the first run.
    let timers = fakeTimers(), store = fakeStore();
    let r = startRetention({ config: testConfig(), store, registry: new Registry(), timers });
    await r.stop();
    assert.equal(timers.live().length, 0);
    assert.equal(await r.runNow(), null);
    assert.equal(store.calls.length, 0);

    // During a run.
    timers = fakeTimers(); store = fakeStore();
    const log = fakeLog(), registry = new Registry();
    r = startRetention({ config: testConfig(), store, log, registry, timers });
    timers.fire();
    const signal = store.calls[0].opts.signal;
    assert.equal(signal.aborted, false);
    let stopped = false;
    const stopping = r.stop().then(() => { stopped = true; });
    await settle();
    assert.equal(signal.aborted, true, 'the run is asked to stop between two statements');
    assert.equal(stopped, false, 'stop() waits for the run to end');
    store.calls[0].resolve({ ...ZERO, anomalies: 2 });
    await stopping;
    assert.equal(value(registry, 'scacelith_retention_runs_total', 'aborted'), 1);
    assert.equal(value(registry, 'scacelith_retention_purged_total', 'anomalies'), 2);
    assert.ok(log.lines.some((l) => l.msg === 'retention purge interrupted by the shutdown'));
    assert.equal(timers.live().length, 0, 'no next run');
    assert.equal(await r.runNow(), null);
    assert.equal(store.calls.length, 1);
});

test('with a real database: the scheduled run purges, stop() then store.close() leaves nothing running', async (t) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-retention-timer-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const config = testConfig({ DB_PATH: path.join(dir, 'db.sqlite') });
    const store = openStore(config);
    migrate(store);
    const now = Date.now();
    const a = store.users.create({ username: 'Ann', email: 'ann@example.org' });
    store.security.insertBatch(Array.from({ length: 1500 }, () => ({ kind: 'login_failed', userId: a, ip: '192.0.2.1', at: now - 200 * DAY })));
    store.tokens.create({ kind: 'password_reset', tokenHash: 'expired', userId: a, expiresAt: now - 1 });

    const timers = fakeTimers(), registry = new Registry(), log = fakeLog();
    const r = startRetention({ config, store, log, registry, timers, sliceMs: 0 });
    timers.fire();
    assert.ok(r.running);
    while (r.running) await settle();
    assert.equal(value(registry, 'scacelith_retention_purged_total', 'security_events'), 1500);
    assert.equal(value(registry, 'scacelith_retention_purged_total', 'tokens'), 1);
    assert.equal(store.security.forUser(a, 10).length, 0);
    assert.equal(timers.live().length, 1);

    await r.stop();
    store.close();
    assert.equal(timers.live().length, 0);
    assert.equal(await r.runNow(), null, 'no run after store.close()');
    assert.ok(!log.lines.some((l) => l.level === 'error' || l.level === 'warn'));
});
