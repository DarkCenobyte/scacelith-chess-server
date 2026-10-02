// Escalating blocks of addresses decided by the primary (src/cluster/abuse.js AbuseTracker) from
// the refusals the workers report: sums over workers and over a sliding minute, the ladder and its
// reset after 6 hours, the /48 rules, reports for blocked keys, snapshots, the cap, and T = 0.

import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { ABUSE_BLOCK_FORGET_MS, AbuseTracker } from '../../src/cluster/abuse.js';
import { testConfig } from '../../src/config.js';
import { Registry } from '../../src/metrics.js';
import { createClock } from './helpers/auth-fakes.js';

function tracker(env = {}, o = {}) {
    const now = createClock(1000000);
    const registry = new Registry();
    const sent = [];
    const warned = [];
    const log = { warn: (msg, f) => warned.push({ msg, ...f }), error() {} };
    const t = new AbuseTracker({ config: testConfig({ ABUSE_BLOCK_REFUSALS_PER_MIN: '100', ...env }), now, registry, log,
        broadcast: (blocks) => sent.push(blocks), ...o });
    return { t, now, registry, sent, warned };
}

function metric(registry, name, labels) {
    const m = registry.metrics.get(name);
    let sum = 0;
    for (const c of m?.children.values() ?? []) if (!labels || labels.every((l, i) => l === undefined || c.labelValues[i] === l)) sum += c.value;
    return sum;
}

describe('AbuseTracker', () => {
    it('sums the reports of several workers; T refusals in a minute block for ABUSE_BLOCK_BASE_SEC', () => {
        const { t, sent, registry, warned } = tracker();
        assert.deepEqual(t.report([['198.51.100.1', null, 60]]), []);          // shard 0
        assert.deepEqual(t.report([['198.51.100.1', null, 39]]), []);          // shard 1: 99
        assert.deepEqual(t.report([['198.51.100.1', null, 1], ['198.51.100.2', null, 5]]), [['198.51.100.1', 60000, 1]]);
        assert.deepEqual(sent, [[['198.51.100.1', 60000, 1]]], 'one broadcast for the report');
        assert.ok(t.isBlocked('198.51.100.1'));
        assert.ok(!t.isBlocked('198.51.100.2'));
        assert.equal(metric(registry, 'scacelith_abuse_blocks_total', ['ip', '1']), 1);
        assert.equal(metric(registry, 'scacelith_abuse_blocked', ['ip']), 1);
        assert.equal(warned.length, 1);
        assert.equal(warned[0].msg, 'ip blocked');
        assert.deepEqual([warned[0].ip, warned[0].scope, warned[0].blockLevel, warned[0].ttlSec, warned[0].refusals], ['198.51.100.0/24', 'ip', 1, 60, 100]);
    });

    it('counts over a sliding minute', () => {
        const { t, now } = tracker();
        t.report([['192.0.2.1', null, 90]]);
        now.advance(120000);
        assert.deepEqual(t.report([['192.0.2.1', null, 90]]), [], 'the old refusals left the window');
        now.advance(30000);
        assert.equal(t.report([['192.0.2.1', null, 60]]).length, 1, 'half of the previous minute still counts');
    });

    it('ladder: 60, 240, 960 then 3600 s within 6 hours; back to 60 after 6 hours without a block', () => {
        const { t, now, registry } = tracker();
        const flood = () => t.report([['192.0.2.9', null, 1000]]);
        const ttls = [];
        for (let i = 0; i < 5; i++) {
            const [[, ttl, level]] = flood();
            ttls.push([ttl / 1000, level]);
            assert.deepEqual(flood(), [], 'reports while blocked are ignored');
            now.advance(ttl);
        }
        assert.deepEqual(ttls, [[60, 1], [240, 2], [960, 3], [3600, 4], [3600, 5]]);
        assert.equal(metric(registry, 'scacelith_abuse_blocks_total', ['ip', '4']), 2, 'level 4 and beyond');
        now.advance(ABUSE_BLOCK_FORGET_MS + 1);
        t.sweep();
        assert.deepEqual(flood(), [['192.0.2.9', 60000, 1]]);
    });

    it('after a block the key counts from zero: T new refusals block it again, not a few', () => {
        const { t, now } = tracker();
        now.set(1020000 + 1);                     // just after the start of a fixed minute
        assert.deepEqual(t.report([['192.0.2.7', '2001:db8::/48', 99]]), []);
        assert.deepEqual(t.report([['192.0.2.7', '2001:db8::/48', 1]]), [['192.0.2.7', 60000, 1]]);
        now.advance(60000);                       // the block ends; the refusals before it are a minute old
        assert.ok(!t.isBlocked('192.0.2.7'));
        assert.deepEqual(t.report([['192.0.2.7', null, 99]]), [], 'T - 1 refusals after the block');
        assert.deepEqual(t.report([['192.0.2.7', null, 1]]), [['192.0.2.7', 240000, 2]], 'the T-th escalates');
    });

    it('ABUSE_BLOCK_BASE_SEC and ABUSE_BLOCK_MAX_SEC shape the ladder', () => {
        const { t, now } = tracker({ ABUSE_BLOCK_BASE_SEC: '10', ABUSE_BLOCK_MAX_SEC: '100' });
        const ttls = [];
        for (let i = 0; i < 4; i++) { const [[, ttl]] = t.report([['192.0.2.9', null, 1000]]); ttls.push(ttl); now.advance(ttl); }
        assert.deepEqual(ttls, [10000, 40000, 100000, 100000]);
    });

    it('blocks a /48 at 4 times the threshold over its /64s, or once 4 of its /64s are blocked', () => {
        const { t, registry } = tracker();
        // 50 /64s of one /48, 9 refusals each: none reaches 100, the /48 reaches 400 at the 45th.
        let fresh = [];
        for (let i = 0; i < 50; i++) fresh = fresh.concat(t.report([[`2001:db8:1:${i.toString(16)}::/64`, '2001:db8:1::/48', 9]]));
        assert.deepEqual(fresh, [['2001:db8:1::/48', 60000, 1]]);
        assert.equal(metric(registry, 'scacelith_abuse_blocks_total', ['prefix']), 1);
        assert.deepEqual(t.report([['2001:db8:1:99::/64', '2001:db8:1::/48', 1000]]), [], 'a /64 of a blocked /48 is ignored');

        const b = tracker().t;
        const out = [];
        for (let i = 0; i < 4; i++) out.push(...b.report([[`2001:db8:2:${i}::/64`, '2001:db8:2::/48', 100]]));
        assert.deepEqual(out.map(([k]) => k), ['2001:db8:2:0::/64', '2001:db8:2:1::/64', '2001:db8:2:2::/64', '2001:db8:2:3::/64', '2001:db8:2::/48']);
    });

    it('snapshot: the running blocks with the time each has left; sweep ends the expired ones', () => {
        const { t, now, registry } = tracker();
        t.report([['192.0.2.1', null, 100]]);
        now.advance(10000);
        t.report([['192.0.2.2', null, 100]]);
        assert.deepEqual(t.snapshot(), [['192.0.2.1', 50000, 1], ['192.0.2.2', 60000, 1]]);
        now.advance(50000);
        assert.deepEqual(t.snapshot(), [['192.0.2.2', 10000, 1]]);
        t.sweep();
        assert.equal(t.size, 1);
        assert.equal(metric(registry, 'scacelith_abuse_blocked', ['ip']), 1);
    });

    it('keeps at most maxBlocks running blocks, the oldest ending first', () => {
        const { t, registry } = tracker({}, { maxBlocks: 3 });
        for (let i = 1; i <= 4; i++) t.report([[`192.0.2.${i}`, null, 100]]);
        assert.equal(t.size, 3);
        assert.ok(!t.isBlocked('192.0.2.1') && t.isBlocked('192.0.2.4'));
        assert.equal(metric(registry, 'scacelith_abuse_blocks_evicted_total'), 1);
    });

    it('ABUSE_BLOCK_REFUSALS_PER_MIN=0 never blocks; malformed entries are ignored', () => {
        const { t, sent } = tracker({ ABUSE_BLOCK_REFUSALS_PER_MIN: '0' });
        assert.equal(t.enabled, false);
        assert.deepEqual(t.report([['192.0.2.1', null, 1e9]]), []);
        const u = tracker().t;
        assert.deepEqual(u.report([null, [], [42, null, 500], ['192.0.2.3', null, -5], ['192.0.2.3', null, 'x'], 'junk']), []);
        assert.deepEqual(u.report('not an array'), []);
        assert.equal(sent.length, 0);
    });
});
