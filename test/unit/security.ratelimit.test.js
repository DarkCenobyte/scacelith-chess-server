import test from 'node:test';
import assert from 'node:assert/strict';
import {
    FailureCounter, LruMap, SlidingWindowCounter, TokenBucketLimiter, createLocalControl, ipKey, normalizeIp,
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
