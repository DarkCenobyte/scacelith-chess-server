import test from 'node:test';
import assert from 'node:assert/strict';
import {
    FailureCounter, LruMap, SlidingWindowCounter, TokenBucketLimiter, createLocalControl, ipKey, normalizeIp, prefixKey, workerShare,
} from '../../src/security/ratelimit.js';
import { createClock } from './helpers/auth-fakes.js';

test('LruMap evicts the least recently used entry', () => {
    const m = new LruMap(2);
    m.set('a', 1); m.set('b', 2);
    m.get('a');
    m.set('c', 3);
    assert.deepEqual([...m.keys()], ['a', 'c']);
    assert.equal(m.size, 2);
});

test('token bucket: burst, refusal with retry time, refill', () => {
    const now = createClock();
    const l = new TokenBucketLimiter({ now });
    for (let i = 0; i < 3; i++) assert.ok(l.take('k', 3, 60000).allowed);
    const r = l.take('k', 3, 60000);
    assert.equal(r.allowed, false);
    assert.equal(r.retryAfterMs, 20000);
    now.advance(20000);
    assert.ok(l.take('k', 3, 60000).allowed);
    assert.ok(l.take('other', 3, 60000).allowed, 'keys are independent');
});

test('token bucket: give() returns granted tokens, never beyond the limit', () => {
    const now = createClock();
    const l = new TokenBucketLimiter({ now });
    for (let i = 0; i < 3; i++) assert.ok(l.take('k', 3, 60000).allowed);
    assert.equal(l.take('k', 3, 60000).allowed, false);
    l.give('k', 3, 60000);
    assert.ok(l.take('k', 3, 60000).allowed, 'the given-back token is taken again');
    assert.equal(l.take('k', 3, 60000).allowed, false);
    // The refill up to now counts first, then the token; the bucket never holds more than the limit.
    now.advance(20000);
    l.give('k', 3, 60000);
    l.give('k', 3, 60000);
    l.give('k', 3, 60000);
    for (let i = 0; i < 3; i++) assert.ok(l.take('k', 3, 60000).allowed);
    assert.equal(l.take('k', 3, 60000).allowed, false);
    l.give('unknown', 3, 60000);       // an evicted or unknown bucket is full already
    assert.equal(l.buckets.size, 1);
});

test('local control: ratelimit.refund takes back a counted take, in its own window', async () => {
    const now = createClock(0);
    const ctl = createLocalControl({ now });
    const take = () => ctl.request('ratelimit.take', { key: 'k', limit: 2, windowMs: 1000, cost: 1 });
    const refund = (ageMs) => ctl.request('ratelimit.refund', { key: 'k', windowMs: 1000, cost: 1, ageMs });
    assert.equal((await take()).allowed, true);
    assert.equal((await take()).allowed, true);
    assert.equal((await take()).allowed, false);
    assert.deepEqual(await refund(0), { refunded: true });
    assert.equal((await take()).allowed, true, 'the refunded unit is free again');
    assert.equal((await take()).allowed, false);
    // A take of the previous window is taken back from that window.
    now.advance(1250);                 // window [1000, 2000): the previous window (2) weighs 0.75
    assert.deepEqual(await refund(1150), { refunded: true });   // taken at 100, in window [0, 1000)
    assert.equal((await take()).allowed, true, 'prev 1 x 0.75 + 1 <= 2 (without the refund: 2 x 0.75 + 1 > 2)');
    assert.equal((await take()).allowed, false);
    // Unknown keys and takes older than the previous window change nothing.
    assert.deepEqual(await ctl.request('ratelimit.refund', { key: 'nope', windowMs: 1000, cost: 1, ageMs: 0 }), { refunded: false });
    assert.deepEqual(await refund(5000), { refunded: false });
});

test('token bucket memory is bounded', () => {
    const l = new TokenBucketLimiter({ maxKeys: 100 });
    for (let i = 0; i < 1000; i++) l.take(`k${i}`, 1, 1000);
    assert.equal(l.buckets.size, 100);
});

test('failure counter: exponential delay from the threshold, capped, reset, forgotten', () => {
    const now = createClock();
    const f = new FailureCounter({ threshold: 3, baseDelayMs: 1000, maxDelayMs: 8000, forgetMs: 3600000, now });
    f.fail('a'); f.fail('a');
    assert.equal(f.retryAfter('a'), 0);
    assert.equal(f.fail('a').retryAfterMs, 1000);
    assert.equal(f.retryAfter('a'), 1000);
    now.advance(1000);
    assert.equal(f.retryAfter('a'), 0);
    assert.equal(f.fail('a').retryAfterMs, 2000);
    assert.equal(f.fail('a').retryAfterMs, 4000);
    assert.equal(f.fail('a').retryAfterMs, 8000);
    assert.equal(f.fail('a').retryAfterMs, 8000, 'capped');
    f.reset('a');
    assert.equal(f.retryAfter('a'), 0);
    f.fail('b'); f.fail('b'); f.fail('b');
    now.advance(3600000 + 60001);
    assert.equal(f.failures('b'), 0, 'forgotten after forgetMs');
});

test('sliding window counter decays over the next window', () => {
    const now = createClock(0);
    const c = new SlidingWindowCounter(60000, now);
    for (let i = 0; i < 10; i++) c.add();
    assert.equal(c.count(), 10);
    now.advance(60000 + 30000);
    assert.equal(c.count(), 5);
    now.advance(60000);
    assert.equal(c.count(), 0);
});

test('local control implements ratelimit.take and once.consume', async () => {
    const now = createClock(0);
    const ctl = createLocalControl({ now });
    const take = () => ctl.request('ratelimit.take', { key: 'k', limit: 2, windowMs: 1000, cost: 1 });
    assert.deepEqual(await take(), { allowed: true, retryAfterMs: 0, count: 1 });
    assert.deepEqual(await take(), { allowed: true, retryAfterMs: 0, count: 2 });
    const r = await take();
    assert.equal(r.allowed, false);
    assert.ok(r.retryAfterMs > 0 && r.retryAfterMs <= 1000);
    now.advance(2000);
    assert.equal((await take()).allowed, true);
    assert.deepEqual(await ctl.request('once.consume', { key: 'x', ttlMs: 100 }), { fresh: true });
    assert.deepEqual(await ctl.request('once.consume', { key: 'x', ttlMs: 100 }), { fresh: false });
    now.advance(101);
    assert.deepEqual(await ctl.request('once.consume', { key: 'x', ttlMs: 100 }), { fresh: true });
});

test('client address keys: IPv4, IPv4-mapped, IPv6 /64', () => {
    assert.equal(ipKey('198.51.100.4'), '198.51.100.4');
    assert.equal(ipKey('::ffff:198.51.100.4'), '198.51.100.4');
    assert.equal(ipKey('2001:db8:aa:bb:1:2:3:4'), '2001:db8:aa:bb::/64');
    assert.equal(ipKey('2001:db8:aa:bb::9'), '2001:db8:aa:bb::/64');
    assert.equal(normalizeIp('::ffff:10.1.2.3'), '10.1.2.3');
    assert.equal(normalizeIp('::1'), '::1');
});

test('client source keys: IPv4 address, IPv6 /48', () => {
    assert.equal(prefixKey('198.51.100.4'), '198.51.100.4');
    assert.equal(prefixKey('::ffff:198.51.100.4'), '198.51.100.4');
    assert.equal(prefixKey('2001:db8:aa:bb:1:2:3:4'), '2001:db8:aa::/48');
    assert.equal(prefixKey('2001:db8:aa:ffff::9'), '2001:db8:aa::/48', 'every /64 of the /48 has the same key');
    assert.equal(prefixKey('2001:DB8:0AA::1'), '2001:db8:aa::/48');
    assert.notEqual(prefixKey('2001:db8:ab::1'), prefixKey('2001:db8:aa::1'));
});

test('workerShare: all of a limit on 1 or 2 workers, 2 L / N beyond, never 0', () => {
    assert.equal(workerShare(600, 1), 600);
    assert.equal(workerShare(600, 2), 600);
    assert.equal(workerShare(600, 4), 300);
    assert.equal(workerShare(600, 16), 75);
    assert.equal(workerShare(10, 4), 5);
    assert.equal(workerShare(1, 16), 1, 'ceil, at least 1');
    assert.equal(workerShare(3, 16), 1);
    assert.equal(workerShare(0, 4), 1, 'never 0');
    assert.equal(workerShare(128, 0), 128, 'workers below 1 count as 1');
    // A client spread over every worker gets at most 2 L; one on a single connection at least 2 L / N.
    for (const n of [1, 2, 3, 4, 8, 16]) {
        const s = workerShare(600, n);
        assert.ok(s * n <= Math.max(600, 2 * 600 + n), `${n} workers: ${s}`);
        assert.ok(s >= Math.min(600, 2 * 600 / n));
    }
});

test('token bucket: a burst and a rate expressed as limit and window', () => {
    // 600 per minute with a burst of half a minute: take(key, 300, 300 * 60000 / 600 = 30000).
    const now = createClock();
    const l = new TokenBucketLimiter({ now });
    for (let i = 0; i < 300; i++) assert.ok(l.take('k', 300, 30000).allowed);
    const r = l.take('k', 300, 30000);
    assert.equal(r.allowed, false);
    assert.equal(r.retryAfterMs, 100, 'one token per 100 ms: 600 per minute');
    now.advance(60000);
    let n = 0;
    while (l.take('k', 300, 30000).allowed) n++;
    assert.equal(n, 300, 'a minute refills the burst, not more');
});
