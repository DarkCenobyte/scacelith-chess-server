import { test } from 'node:test';
import assert from 'node:assert/strict';
import { testConfig } from '../../src/config.js';
import { Conduct, CONDUCT_COOLDOWNS_MS, CONDUCT_WINDOW_MS } from '../../src/match/conduct.js';

const cfg = testConfig();   // CONDUCT_ABANDON_LIMIT = 3
const MIN = 60000, HOUR = 3600000, DAY = CONDUCT_WINDOW_MS;

// In-memory stand-in for store.conduct (DESIGN 5.5).
class FakeConductStore {
    constructor() { this.incidents = []; this.cooldowns = new Map(); this.calls = { cooldown: 0, countSince: 0, setCooldown: 0 }; }
    record(userId, kind, at) { this.incidents.push({ userId, kind, at }); }
    countSince(userId, since) {
        this.calls.countSince++;
        const out = { abandon: 0, abort: 0, noshow: 0 };
        for (const i of this.incidents) if (i.userId === userId && i.at >= since) out[i.kind]++;
        return out;
    }
    cooldown(userId) { this.calls.cooldown++; return this.cooldowns.get(userId) || null; }
    setCooldown(userId, until, level) { this.calls.setCooldown++; this.cooldowns.set(userId, { until, level }); }
}

function make(store = new FakeConductStore(), config = cfg) {
    return { store, conduct: new Conduct({ config, store: { conduct: store }, now: () => 0 }) };
}

test('conduct: below the limit nothing happens', () => {
    const { conduct, store } = make();
    let r = conduct.record(1, 'abandon', 0);
    assert.deepEqual(r, { until: 0, level: 0, incidents: 1, started: false });
    r = conduct.record(1, 'abort', 10 * MIN);
    assert.equal(r.started, false);
    assert.equal(conduct.cooldownUntil(1, 10 * MIN), 0);
    assert.equal(store.incidents.length, 2);
    assert.equal(store.calls.setCooldown, 0);
});

test('conduct: the limit in 24 h pauses rated matchmaking 15 min, then 1 h, then 6 h', () => {
    const { conduct, store } = make();
    conduct.record(1, 'abandon', 0);
    conduct.record(1, 'noshow', HOUR);
    let r = conduct.record(1, 'abort', 2 * HOUR);
    assert.deepEqual(r, { until: 2 * HOUR + 15 * MIN, level: 1, incidents: 3, started: true });
    assert.deepEqual(store.cooldowns.get(1), { until: 2 * HOUR + 15 * MIN, level: 1 });
    assert.equal(conduct.cooldownUntil(1, 2 * HOUR), 2 * HOUR + 15 * MIN);
    assert.equal(conduct.cooldownUntil(1, 2 * HOUR + 15 * MIN), 0);
    // Repeated offences.
    r = conduct.record(1, 'abandon', 5 * HOUR);
    assert.deepEqual([r.until, r.level], [6 * HOUR, 2]);
    r = conduct.record(1, 'abandon', 7 * HOUR);
    assert.deepEqual([r.until, r.level], [13 * HOUR, 3]);
    r = conduct.record(1, 'abandon', 14 * HOUR);
    assert.deepEqual([r.until, r.level], [20 * HOUR, 3]);
    assert.deepEqual(CONDUCT_COOLDOWNS_MS, [15 * MIN, HOUR, 6 * HOUR]);
    // Other users are not affected.
    assert.equal(conduct.cooldownUntil(2, 14 * HOUR), 0);
});

test('conduct: incidents older than 24 h do not count', () => {
    const { conduct } = make();
    conduct.record(1, 'abandon', 0);
    conduct.record(1, 'abandon', 12 * HOUR);
    const r = conduct.record(1, 'abandon', DAY + 1);     // the first one left the window
    assert.equal(r.incidents, 2);
    assert.equal(r.started, false);
    assert.equal(conduct.cooldownUntil(1, DAY + 1), 0);
});

test('conduct: an active cooldown is never shortened', () => {
    const { conduct } = make();
    for (let i = 0; i < 5; i++) conduct.record(1, 'abandon', i * MIN);      // levels 1, 2, 3: 6 h from 4 min
    assert.equal(conduct.cooldownUntil(1, 5 * MIN), 4 * MIN + 6 * HOUR);
    const store = new FakeConductStore();
    store.cooldowns.set(7, { until: 10 * DAY, level: 0 });                    // e.g. set by an administrator
    const c = new Conduct({ config: cfg, store, now: () => 0 });
    for (let i = 0; i < 3; i++) c.record(7, 'noshow', i);
    assert.equal(c.cooldownUntil(7, 3), 10 * DAY);
    assert.equal(c.state(7).level, 1);
});

test('conduct: the level decays by one per full day without incident', () => {
    const { conduct, store } = make();
    // Three cooldowns in a row: level 3.
    for (let i = 0; i < 5; i++) conduct.record(1, 'abandon', i * HOUR);
    assert.equal(conduct.state(1).level, 3);
    // Two clean days (the last incident was at 4 h): one more incident brings the level to 1.
    let t = 4 * HOUR + 2 * DAY + HOUR;
    let r = conduct.record(1, 'abandon', t);
    assert.equal(r.level, 1);
    assert.equal(r.started, false);
    assert.deepEqual(store.cooldowns.get(1).level, 1);     // decay persisted
    // Two more incidents within 24 h: limit reached at level 1 -> 1 h, level 2.
    conduct.record(1, 'abandon', t + HOUR);
    r = conduct.record(1, 'abandon', t + 2 * HOUR);
    assert.equal(r.started, true);
    assert.equal(r.until, t + 3 * HOUR);
    assert.equal(r.level, 2);
    // Three clean days or more: back to the first cooldown (15 min).
    t += 2 * HOUR + 5 * DAY;
    conduct.record(1, 'abandon', t);
    conduct.record(1, 'abandon', t + 1);
    r = conduct.record(1, 'abandon', t + 2);
    assert.equal(r.until, t + 2 + 15 * MIN);
    assert.equal(r.level, 1);
    // An incident inside the last 24 h stops the decay.
    const b = make();
    for (let i = 0; i < 4; i++) b.conduct.record(2, 'abandon', i * HOUR);    // level 2
    b.conduct.record(2, 'abandon', 30 * HOUR);                                  // 27 h clean: level 1
    assert.equal(b.conduct.state(2).level, 1);
    b.conduct.record(2, 'abandon', 40 * HOUR);                                  // 10 h since the last: stays 1
    assert.equal(b.conduct.state(2).level, 1);
});

test('conduct: survives a restart through the store, reads are cached', () => {
    const store = new FakeConductStore();
    const a = new Conduct({ config: cfg, store: { conduct: store }, now: () => 0 });
    for (let i = 0; i < 3; i++) a.record(1, 'abandon', i);
    const until = a.cooldownUntil(1, 3);
    assert.equal(until, 2 + 15 * MIN);
    // A new process on the same database.
    const b = new Conduct({ config: cfg, store: { conduct: store }, now: () => 5 });
    assert.equal(b.cooldownUntil(1), until);
    assert.equal(b.state(1).level, 1);
    const reads = store.calls.cooldown;
    for (let i = 0; i < 100; i++) b.cooldownUntil(1, 10);
    for (let i = 0; i < 100; i++) b.cooldownUntil(2, 10);
    assert.equal(store.calls.cooldown, reads + 1);            // user 2 read once, user 1 cached
    // invalidate() forgets the cache (e.g. after an administrator cleared the cooldown).
    store.cooldowns.delete(1);
    assert.equal(b.cooldownUntil(1, 10), until);
    b.invalidate(1);
    assert.equal(b.cooldownUntil(1, 10), 0);
    b.invalidate();
    assert.equal(b.cooldownUntil(2, 10), 0);
});

test('conduct: store variants, validation and configurable limit', () => {
    const store = new FakeConductStore();
    store.cooldown = (userId) => (userId === 1 ? 5000 : null);                 // bare number
    const c = new Conduct({ config: cfg, store, now: () => 0 });
    assert.equal(c.cooldownUntil(1, 0), 5000);
    assert.throws(() => c.record(1, 'rage', 0), TypeError);
    assert.throws(() => new Conduct({ config: cfg, store: {} }), TypeError);
    assert.throws(() => new Conduct({ store }), TypeError);

    const one = make(new FakeConductStore(), testConfig({ CONDUCT_ABANDON_LIMIT: '1' }));
    const r = one.conduct.record(9, 'noshow', 100);
    assert.equal(r.started, true);
    assert.equal(r.until, 100 + 15 * MIN);
});
