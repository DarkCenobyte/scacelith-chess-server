// Protection per address of a worker (src/net/ipguard.js IpGuard): the request budget per IPv4
// address or IPv6 /64 and per /48, requests in progress, new and open connections, ABUSE_EXEMPT,
// blocks (from the primary and the local fast path), the refusal reports, and the cost of the
// checks on the request path (a micro-benchmark).

import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { performance } from 'node:perf_hooks';
import { describe, it } from 'node:test';
import { testConfig } from '../../src/config.js';
import { Registry } from '../../src/metrics.js';
import { ipGroupKey } from '../../src/net/ip.js';
import { IpGuard, REPORT_MAX_ENTRIES, addressKeys } from '../../src/net/ipguard.js';
import { admitRequest } from '../../src/net/listeners.js';
import { createClock } from './helpers/auth-fakes.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function value(registry, name, label) {
    const m = registry.metrics.get(name);
    if (!m) return 0;
    if (m.fn) return m.fn();
    for (const c of m.children.values()) if (label === undefined || c.labelValues[0] === label) return c.value;
    return 0;
}

function guardOf(env = {}, o = {}) {
    const now = o.now || createClock(0);
    const registry = new Registry();
    const reports = [];
    const guard = new IpGuard({
        config: testConfig(env), workers: o.workers ?? 1, registry, now,
        report: o.local ? null : (entries) => reports.push(entries), reportIntervalMs: o.reportIntervalMs ?? 1000,
    });
    return { guard, now, registry, reports };
}

class FakeSocket extends EventEmitter {
    constructor(ip) { super(); this.remoteAddress = ip; }
}

describe('IpGuard: keys', () => {
    it('IPv4-mapped, upper-case and expanded IPv6 give the same keys as net/ip.js', () => {
        assert.equal(addressKeys('::ffff:192.0.2.7').k64, '192.0.2.7');
        assert.equal(addressKeys('192.0.2.7').k48, null, 'no /48 for IPv4');
        const a = addressKeys('2001:DB8:1:2::5'), b = addressKeys('2001:0db8:0001:0002:aaaa:0:0:9');
        assert.equal(a.k64, b.k64);
        assert.equal(a.k64, ipGroupKey('2001:db8:1:2::5'));
        assert.equal(a.k48, ipGroupKey('2001:db8:1:2::5', 48));
        assert.equal(addressKeys('2001:db8:1:3::5').k48, a.k48, 'another /64 of the same /48');
        assert.notEqual(addressKeys('2001:db8:1:3::5').k64, a.k64);
        assert.deepEqual([addressKeys('').exempt, addressKeys(undefined).k64], [true, 'unknown'], 'an unreadable address is left alone');
    });
});

describe('IpGuard: request budget', () => {
    it('one /64 or IPv4 address: burst of half a minute, then the rate; another address is independent', () => {
        // 60 per minute on one worker: burst 30, one token per second.
        const { guard, now, registry } = guardOf({ HTTP_RATE_PER_IP: '60' });
        const k = guard.keys('198.51.100.1');
        for (let i = 0; i < 30; i++) assert.equal(guard.request(k), null);
        assert.deepEqual(guard.request(k), { reason: 'ip', retryAfterMs: 1000 });
        assert.equal(guard.request('198.51.100.2'), null, 'another address');
        now.advance(1000);
        assert.equal(guard.request(k), null);
        assert.equal(guard.request(k)?.reason, 'ip');
        assert.equal(value(registry, 'scacelith_http_rate_limited_total', 'ip'), 2);
    });

    it('each worker allows its share of the whole-server limit', () => {
        assert.equal(guardOf({ HTTP_RATE_PER_IP: '600' }, { workers: 2 }).guard.reqBurst, 300, 'VPS-1: all of it per worker');
        const four = guardOf({ HTTP_RATE_PER_IP: '600' }, { workers: 4 }).guard;
        assert.equal(four.reqBurst, 150, '4 workers: half of it');
        assert.equal(four.reqWindow, 30000);
    });

    it('a /48 rotating over its /64s meets the /48 budget, and the /64 token is given back', () => {
        // /64: 60 per minute (burst 30); /48: 120 per minute (burst 60).
        const { guard, registry } = guardOf({ HTTP_RATE_PER_IP: '60', HTTP_RATE_PER_PREFIX: '120' });
        let refused = 0, first = -1;
        for (let i = 0; i < 100; i++) {
            const r = guard.request(guard.keys(`2001:db8:7:${i.toString(16)}::1`));
            if (r) { assert.equal(r.reason, 'ip48'); refused++; if (first < 0) first = i; }
        }
        assert.equal(first, 60);
        assert.equal(refused, 40);
        const k = guard.keys('2001:db8:7:50::1');
        assert.equal(guard.reqs.buckets.peek(k.k64).tokens, 30, 'the refused /64 keeps its whole burst');
        assert.equal(guard.request('2001:db8:8::1'), null, 'another /48');
        assert.equal(value(registry, 'scacelith_http_rate_limited_total', 'ip48'), 40);
    });

    it('HTTP_RATE_PER_PREFIX 0 means 4 times HTTP_RATE_PER_IP', () => {
        const { guard } = guardOf({ HTTP_RATE_PER_IP: '60' });
        assert.equal(guard.reqBurst48, 120);
    });
});

describe('IpGuard: requests in progress', () => {
    it('IP_MAX_INFLIGHT per /64 and 4 times that per /48, given back by leave()', () => {
        const { guard, registry } = guardOf({ IP_MAX_INFLIGHT: '2' });
        const a = guard.keys('2001:db8:1:1::1');
        assert.ok(guard.enter(a) && guard.enter(a));
        assert.equal(guard.enter(a), false);
        assert.equal(value(registry, 'scacelith_http_rate_limited_total', 'inflight'), 1);
        guard.leave(a);
        assert.equal(guard.enter(a), true);
        // The /48: 8 in all, over its /64s.
        let n = 2;
        for (let i = 2; i < 10; i++) if (guard.enter(guard.keys(`2001:db8:1:${i}::1`))) n++;
        assert.equal(n, 8);
        assert.equal(guard.inflightTotal, 8);
        assert.equal(value(registry, 'scacelith_http_inflight'), 8);
    });

    it('admitRequest releases the slot when the response closes, finished or aborted', () => {
        const { guard } = guardOf({ IP_MAX_INFLIGHT: '1' });
        const socket = new FakeSocket('203.0.113.9');
        const fakeRes = () => Object.assign(new EventEmitter(), {
            headersSent: false, writableEnded: false, status: 0, headers: null,
            writeHead(st, h) { this.status = st; this.headers = h; this.headersSent = true; },
            end() { this.writableEnded = true; },
        });
        const req1 = { socket, clientIp: '203.0.113.9' }, res1 = fakeRes();
        assert.equal(admitRequest(guard, req1, res1), true);
        assert.equal(admitRequest(guard, req1, res1), true, 'once per request: the second call is a no-op');
        assert.equal(guard.inflightTotal, 1);
        const res2 = fakeRes();
        assert.equal(admitRequest(guard, { socket, clientIp: '203.0.113.9' }, res2), false);
        assert.equal(res2.status, 429);
        assert.equal(res2.headers['Retry-After'], '1');
        assert.equal(res2.headers.Connection, undefined, 'only a blocked address gets Connection: close');
        res1.emit('close');                                   // aborted by the client, or finished
        res1.emit('close');
        assert.equal(guard.inflightTotal, 0);
        assert.equal(admitRequest(guard, { socket, clientIp: '203.0.113.9' }, fakeRes()), true);
    });
});

describe('IpGuard: connections', () => {
    it('IP_CONN_RATE new connections per second (burst 4 s) and IP_MAX_CONNECTIONS open, released on close', () => {
        const { guard, now, registry } = guardOf({ IP_CONN_RATE: '2', IP_MAX_CONNECTIONS: '5' });
        const open = [];
        for (let i = 0; i < 5; i++) { const s = new FakeSocket('192.0.2.1'); assert.equal(guard.connection(s), null); open.push(s); }
        assert.equal(guard.connection(new FakeSocket('192.0.2.1')), 'conn_open');
        assert.equal(value(registry, 'scacelith_tls_connections_open'), 5);
        open[0].emit('close');
        guard.connectionClosed(open[0]);                    // idempotent
        open[0].emit('close');
        assert.equal(guard.openTotal, 4);
        assert.equal(guard.connection(new FakeSocket('192.0.2.1')), null, 'a freed place');
        assert.equal(guard.connection(new FakeSocket('192.0.2.1')), 'conn_open');
        // 8 tokens in the burst (2 per second x 4 s): 5 + 1 + 1 refused (conn_open spends one too) = 8.
        for (const s of open.slice(1)) s.emit('close');
        assert.equal(guard.connection(new FakeSocket('192.0.2.1')), 'conn_rate');
        now.advance(500);
        assert.equal(guard.connection(new FakeSocket('192.0.2.1')), null, 'one token per 500 ms');
        assert.equal(guard.connection(new FakeSocket('192.0.2.2')), null, 'another address');
    });

    it('the /48 has 4 times the caps of a /64', () => {
        const { guard } = guardOf({ IP_CONN_RATE: '100', IP_MAX_CONNECTIONS: '2' });
        let ok = 0;
        for (let i = 0; i < 12; i++) if (guard.connection(new FakeSocket(`2001:db8:5:${i}::1`)) === null) ok++;
        assert.equal(ok, 8);
    });
});

describe('IpGuard: ABUSE_EXEMPT', () => {
    it('an exempt address has no budget, no caps, is never counted or blocked; others are', () => {
        const { guard, reports } = guardOf({ HTTP_RATE_PER_IP: '2', IP_MAX_INFLIGHT: '1', IP_CONN_RATE: '1', IP_MAX_CONNECTIONS: '1',
            ABUSE_EXEMPT: '203.0.113.0/24,2001:db8:aa::/48' });
        const k = guard.keys('203.0.113.5');
        assert.equal(k.exempt, true);
        assert.ok(guard.isExempt('::ffff:203.0.113.200'));
        for (let i = 0; i < 100; i++) assert.equal(guard.request(k), null);
        for (let i = 0; i < 10; i++) assert.equal(guard.enter(k), true);
        assert.equal(guard.inflightTotal, 0, 'not counted');
        for (let i = 0; i < 10; i++) assert.equal(guard.connection(new FakeSocket('203.0.113.5')), null);
        assert.equal(guard.openTotal, 0);
        guard.applyBlocks([['203.0.113.5', 60000, 1], ['2001:db8:aa::/48', 60000, 1]]);
        assert.equal(guard.request(k), null, 'a block never applies to an exempt address');
        assert.equal(guard.request('2001:db8:aa:1::1'), null);
        guard.noteRefusal(k, 1000);
        assert.deepEqual(guard.flushReports(), [], 'never reported');
        assert.equal(reports.length, 0);
        assert.equal(guard.request('198.51.100.1'), null, '2 per minute: a burst of 1');
        assert.equal(guard.request('198.51.100.1')?.reason, 'ip', 'the others are limited');
    });
});

describe('IpGuard: blocks', () => {
    it('blocks from the primary apply to requests and connections, a /48 block covers its /64s, and they expire', () => {
        const { guard, now, registry } = guardOf();
        assert.equal(guard.applyBlocks([['198.51.100.9', 60000, 1], ['2001:db8:1::/48', 240000, 2], ['bad'], null, ['x', -1, 1]]), 2);
        assert.deepEqual(guard.request('198.51.100.9'), { reason: 'blocked', retryAfterMs: 60000 });
        assert.equal(guard.request('2001:db8:1:77::1')?.reason, 'blocked');
        assert.equal(guard.connection(new FakeSocket('2001:db8:1:1::1')), 'blocked');
        assert.equal(guard.connection(new FakeSocket('198.51.100.9')), 'blocked');
        assert.equal(guard.openTotal, 0, 'a refused connection is not counted open');
        assert.equal(guard.request('198.51.100.10'), null);
        assert.equal(value(registry, 'scacelith_abuse_blocked_keys'), 2);
        assert.equal(value(registry, 'scacelith_http_rate_limited_total', 'blocked'), 2);
        // A shorter block for a key already blocked longer does not shorten it.
        guard.applyBlocks([['2001:db8:1::/48', 1000, 1]]);
        now.advance(60000);
        assert.equal(guard.request('198.51.100.9'), null, 'expired');
        assert.equal(guard.blockedFor('2001:db8:1:77::1'), 180000);
        now.advance(180000);
        assert.equal(guard.request('2001:db8:1:77::1'), null);
        assert.equal(value(registry, 'scacelith_abuse_blocked_keys'), 0);
    });

    it('requests refused because the address is blocked are not counted toward a block again', () => {
        const { guard, reports } = guardOf();
        guard.applyBlocks([['198.51.100.9', 60000, 1]]);
        for (let i = 0; i < 1000; i++) guard.request('198.51.100.9');
        guard.noteRefusal('198.51.100.9', 5);
        guard.flushReports();
        assert.equal(reports.length, 0);
    });

    it('local fast path: ABUSE_BLOCK_REFUSALS_PER_MIN refusals within one interval block at once, and are still reported', () => {
        const { guard, registry, reports } = guardOf({ ABUSE_BLOCK_REFUSALS_PER_MIN: '20', ABUSE_BLOCK_BASE_SEC: '30' });
        for (let i = 0; i < 3; i++) guard.noteRefusal('192.0.2.50', 5);    // the auth family weighs 5
        assert.equal(guard.blockedFor('192.0.2.50'), 0, '15 < 20');
        guard.noteRefusal('192.0.2.50', 5);
        assert.equal(guard.blockedFor('192.0.2.50'), 30000);
        assert.equal(value(registry, 'scacelith_abuse_local_blocks_total'), 1);
        guard.flushReports();
        assert.deepEqual(reports, [[['192.0.2.50', null, 20]]]);
    });

    it('without a primary, a local AbuseTracker sums the intervals and blocks', () => {
        const { guard } = guardOf({ ABUSE_BLOCK_REFUSALS_PER_MIN: '10' }, { local: true });
        for (let i = 0; i < 6; i++) guard.noteRefusal('192.0.2.60', 1);
        guard.flushReports();
        assert.equal(guard.blockedFor('192.0.2.60'), 0, '6 in the minute');
        for (let i = 0; i < 4; i++) guard.noteRefusal('192.0.2.60', 1);
        guard.flushReports();
        assert.equal(guard.blockedFor('192.0.2.60'), 60000, '10 in the minute: blocked for ABUSE_BLOCK_BASE_SEC');
        assert.equal(guard.tracker.size, 1);
    });

    it('ABUSE_BLOCK_REFUSALS_PER_MIN=0: refusals are not counted, the budgets stay', () => {
        const { guard, reports } = guardOf({ ABUSE_BLOCK_REFUSALS_PER_MIN: '0', HTTP_RATE_PER_IP: '2' });
        const k = guard.keys('192.0.2.70');
        guard.request(k); guard.request(k);
        for (let i = 0; i < 100; i++) assert.equal(guard.request(k)?.reason, 'ip');
        assert.deepEqual(guard.flushReports(), []);
        assert.equal(reports.length, 0);
        assert.equal(guard.blockedFor(k), 0);
    });
});

describe('IpGuard: reports', () => {
    it('one report per interval on a timer, never from request(); at most 512 keys, the largest first', async () => {
        const { guard, reports, registry } = guardOf({ HTTP_RATE_PER_IP: '1', ABUSE_BLOCK_REFUSALS_PER_MIN: '100000' }, { reportIntervalMs: 30 });
        const k = guard.keys('192.0.2.80');
        assert.equal(guard.request(k), null);
        for (let i = 0; i < 50; i++) assert.equal(guard.request(k)?.reason, 'ip');
        assert.equal(reports.length, 0, 'no report (no IPC) on the request path');
        for (let i = 0; i < 600; i++) guard.noteRefusal(`10.1.${i >> 8}.${i & 255}`, 1 + (i % 7));
        assert.equal(reports.length, 0);
        await sleep(80);
        assert.equal(reports.length, 1, 'one report for the interval');
        const [entries] = reports;
        assert.equal(entries.length, REPORT_MAX_ENTRIES);
        assert.deepEqual(entries[0], ['192.0.2.80', null, 50], 'the largest first');
        for (let i = 1; i < entries.length; i++) assert.ok(entries[i - 1][2] >= entries[i][2]);
        assert.equal(value(registry, 'scacelith_abuse_report_entries_dropped_total'), 601 - REPORT_MAX_ENTRIES);
        await sleep(60);
        assert.equal(reports.length, 1, 'no timer and no report while nothing is refused');
        guard.close();
    });

    it('a /64 entry carries its /48', () => {
        const { guard, reports } = guardOf();
        guard.noteRefusal('2001:db8:9:1::1', 2);
        guard.noteRefusal('2001:db8:9:1::2', 3);
        guard.flushReports();
        assert.deepEqual(reports, [[['2001:db8:9:1::/64', '2001:db8:9::/48', 5]]]);
    });
});

describe('IpGuard: cost of the checks', () => {
    it('a request costs about a microsecond (micro-benchmark)', () => {
        const { guard } = guardOf({ HTTP_RATE_PER_IP: '1000000000', IP_MAX_INFLIGHT: '100000' });
        const v4 = guard.keysOf('198.51.100.20', {}), v6 = guard.keysOf('2001:db8:3:4::5', {});
        const N = 200000;
        const bench = (fn) => {
            for (let i = 0; i < 20000; i++) fn(i);             // warm-up
            const t0 = performance.now();
            for (let i = 0; i < N; i++) fn(i);
            return (performance.now() - t0) * 1e6 / N;        // ns per call
        };
        const sock = {};
        const reqV4 = bench(() => { guard.request(v4); guard.enter(v4); guard.leave(v4); });
        const reqV6 = bench(() => { guard.request(v6); guard.enter(v6); guard.leave(v6); });
        const cached = bench(() => guard.keysOf('198.51.100.20', sock));
        const keysV6 = bench((i) => guard.keys(`2001:db8:3:${i & 0xffff}::5`));
        const blocked = guardOf().guard;
        blocked.applyBlocks([['198.51.100.21', 3600000, 1]]);
        const bk = blocked.keys('198.51.100.21');
        const refusedBlocked = bench(() => blocked.request(bk));
        process.stdout.write(`# IpGuard cost (ns per call): IPv4 request+enter+leave ${reqV4.toFixed(0)}, IPv6 ${reqV6.toFixed(0)}, `
            + `cached keys ${cached.toFixed(0)}, IPv6 keys computed ${keysV6.toFixed(0)}, blocked request ${refusedBlocked.toFixed(0)}\n`);
        // Generous bounds (a loaded CI machine); the expected values are around 0.3-1.5 µs.
        assert.ok(reqV4 < 20000 && reqV6 < 20000 && cached < 5000 && keysV6 < 20000 && refusedBlocked < 10000);
    });
});
