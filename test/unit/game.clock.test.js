import test from 'node:test';
import assert from 'node:assert/strict';
import { now, graceFor, clockPolicy, rttEmaStep, GameClock, INITIAL_RTT_MS, RTT_EMA_MAX_MS } from '../../src/game/clock.js';
import { testConfig } from '../../src/config.js';

const CFG = testConfig();

test('now() is the monotonic epoch clock', () => {
    const a = now(), b = now();
    assert.ok(b >= a);
    assert.ok(Math.abs(a - Date.now()) < 1000);
});

test('grace = clamp(base / 10, RECONNECT_GRACE_MIN_MS, RECONNECT_GRACE_MAX_MS)', () => {
    assert.equal(graceFor(60000, CFG), 15000);
    assert.equal(graceFor(180000, CFG), 18000);
    assert.equal(graceFor(5400000, CFG), 60000);
    assert.equal(graceFor(180000, testConfig({ RECONNECT_GRACE_MIN_MS: '20000' })), 20000);
});

test('clock policy reads the games section of the configuration', () => {
    const p = clockPolicy(testConfig({ FIRST_MOVE_TIMEOUT_MS: '10000', LAG_COMP_MAX_MS: '500', LAG_QUOTA_MAX_MS: '4000' }));
    assert.deepEqual(p, { firstMoveMs: 10000, lagCompMaxMs: 500, quotaInitialMs: 2000, quotaGainMs: 100, quotaMaxMs: 4000 });
});

test('round-trip average: first sample, exponential average, cap', () => {
    assert.equal(rttEmaStep(NaN, 80), 80);
    assert.equal(rttEmaStep(80, 160), 100);
    assert.equal(rttEmaStep(100, 99999), 100 + 0.25 * (RTT_EMA_MAX_MS - 100));
    const c = new GameClock({ baseMs: 60000, incMs: 0, policy: clockPolicy(CFG), startAt: 0 });
    assert.equal(c.rtt[0], INITIAL_RTT_MS);
    c.setRtt(0, 42.4);
    assert.equal(c.rtt[0], 42);
});

test('check() is pure; the flag deadline is exactly where a zero-think move flags', () => {
    const c = new GameClock({ baseMs: 60000, incMs: 1000, policy: clockPolicy(CFG), startAt: 1000 });
    const r0 = c.check(0, 0, 50000, 0);
    assert.deepEqual([r0.charged, r0.clockAfter, r0.quotaAfter, r0.flagged], [0, 60000, 2000, false]);
    c.apply(0, 60000, 2000, 2000);
    c.apply(1, 60000, 2000, 3000);                  // White's clock starts at 3000
    const d = c.flagDeadline(0);
    assert.equal(d, 3000 + 60000 + 150);
    assert.equal(c.check(0, 2, d - 1, 0).flagged, false);
    assert.equal(c.check(0, 2, d, 0).flagged, true);
    assert.equal(c.ms[0], 60000, 'check() changes nothing');
    const r = c.check(0, 2, 13000, 9990);           // 10 ms of lag
    assert.deepEqual([r.elapsed, r.comp, r.charged, r.clockAfter, r.quotaAfter, r.implausible], [10000, 10, 9990, 60000 - 9990 + 1000, 2000 - 10 + 100, false]);
    assert.equal(c.check(0, 2, 13000, 10101).implausible, true);
    assert.equal(c.remainingAt(0, 2, 13000), 50000);
    assert.equal(c.remainingAt(1, 2, 13000), 60000);
    assert.equal(c.remainingAt(0, 2, 99999999), 0);
});
