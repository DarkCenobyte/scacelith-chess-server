// Rate limiting building blocks, all memory-bounded:
//
//   LruMap                 bounded map, least recently used entries evicted first
//   TokenBucketLimiter     per-key token buckets (per-IP API limits, local first stage)
//   FailureCounter         per-key failure counter with exponential delay (login brute force)
//   SlidingWindowCounter   approximate count of events over the last window (failure-rate detector)
//   createLocalControl()   in-process implementation of the primary's `ratelimit.take` and
//                          `once.consume` IPC requests (docs/DESIGN.md 5.7). Used when no primary
//                          is available (single process, tests) and as the fallback when the
//                          primary does not answer.
//
// Complexity: every operation is O(1) (amortised), no timers: expiry is lazy.

import { expandIPv6 } from '../log.js';

/** Map with a maximum size; get() refreshes an entry, set() evicts the least recently used. */
export class LruMap {
    /** @param {number} max */
    constructor(max) {
        this.max = Math.max(1, max | 0);
        this.map = new Map();
    }
    get size() { return this.map.size; }
    get(key) {
        const v = this.map.get(key);
        if (v === undefined) return undefined;
        this.map.delete(key);
        this.map.set(key, v);
        return v;
    }
    peek(key) { return this.map.get(key); }
    has(key) { return this.map.has(key); }
    set(key, value) {
        if (this.map.has(key)) this.map.delete(key);
        else if (this.map.size >= this.max) this.map.delete(this.map.keys().next().value);
        this.map.set(key, value);
        return this;
    }
    delete(key) { return this.map.delete(key); }
    clear() { this.map.clear(); }
    keys() { return this.map.keys(); }
    entries() { return this.map.entries(); }
}

/**
 * Normalises a client address for rate limiting: IPv4-mapped IPv6 becomes IPv4, IPv6 is reduced
 * to its /64 (one customer network), everything else is kept as is. See also prefixKey().
 * @param {string} ip
 * @returns {string}
 */
export function ipKey(ip) {
    let a = String(ip || '');
    if (a.startsWith('::ffff:') && a.includes('.')) a = a.slice(7);
    if (a.includes(':')) {
        const parts = expandIPv6(a);
        if (parts) return parts.slice(0, 4).join(':') + '::/64';
    }
    return a;
}

/**
 * The wider source of a client address: an IPv4 address itself (IPv4-mapped IPv6 included), or the
 * /48 of an IPv6 address. A /48 is one site: it holds 65536 /64 networks, each with its own ipKey()
 * bucket, and one customer or one hosting provider's client often gets a whole /48 or /56.
 * @param {string} ip
 * @returns {string}
 */
export function prefixKey(ip) {
    const a = normalizeIp(ip);
    if (a.includes(':')) {
        const parts = expandIPv6(a);
        if (parts) return parts.slice(0, 3).join(':') + '::/48';
    }
    return a;
}

/**
 * Normalises the address itself (IPv4-mapped IPv6 -> IPv4).
 * @param {string} ip
 * @returns {string}
 */
export function normalizeIp(ip) {
    const a = String(ip || '');
    return a.startsWith('::ffff:') && a.includes('.') ? a.slice(7) : a;
}

/** Per-key token buckets: `limit` tokens refilled evenly over `windowMs`. */
export class TokenBucketLimiter {
    /** @param {{ maxKeys?: number, now?: () => number }} [opts] */
    constructor({ maxKeys = 100000, now = Date.now } = {}) {
        this.buckets = new LruMap(maxKeys);
        this.now = now;
    }
    /**
     * Takes `cost` tokens from the bucket of `key`.
     * @returns {{ allowed: boolean, retryAfterMs: number, remaining: number }}
     */
    take(key, limit, windowMs, cost = 1) {
        const t = this.now();
        const rate = limit / windowMs;
        let b = this.buckets.get(key);
        if (!b) { b = { tokens: limit, at: t }; this.buckets.set(key, b); }
        else { b.tokens = Math.min(limit, b.tokens + Math.max(0, t - b.at) * rate); b.at = t; }
        if (b.tokens >= cost) {
            b.tokens -= cost;
            return { allowed: true, retryAfterMs: 0, remaining: Math.floor(b.tokens) };
        }
        return { allowed: false, retryAfterMs: Math.ceil((cost - b.tokens) / rate), remaining: 0 };
    }
    reset(key) { this.buckets.delete(key); }
}

/**
 * Failures per key; from the `threshold`-th failure on, each further attempt must wait
 * `baseDelayMs * 2^(failures - threshold)` (at most `maxDelayMs`) after the last failure.
 * A key is forgotten `forgetMs` after its last failure.
 */
export class FailureCounter {
    constructor({ threshold, baseDelayMs = 2000, maxDelayMs = 15 * 60000, forgetMs = 60 * 60000, maxKeys = 100000, now = Date.now }) {
        this.threshold = Math.max(1, threshold | 0);
        this.baseDelayMs = baseDelayMs;
        this.maxDelayMs = maxDelayMs;
        this.forgetMs = forgetMs;
        this.entries = new LruMap(maxKeys);
        this.now = now;
    }
    delayFor(failures) {
        if (failures < this.threshold) return 0;
        const exp = Math.min(30, failures - this.threshold);
        return Math.min(this.maxDelayMs, this.baseDelayMs * 2 ** exp);
    }
    _entry(key, t) {
        const e = this.entries.peek(key);
        if (!e) return null;
        if (t - e.last > Math.max(this.forgetMs, this.delayFor(e.failures) + 60000)) { this.entries.delete(key); return null; }
        return e;
    }
    /** Milliseconds before `key` may try again (0 = now). */
    retryAfter(key) {
        const t = this.now();
        const e = this._entry(key, t);
        if (!e) return 0;
        return Math.max(0, e.last + this.delayFor(e.failures) - t);
    }
    /** Records a failure; returns the new count and the delay it imposes. */
    fail(key) {
        const t = this.now();
        let e = this._entry(key, t);
        if (!e) e = { failures: 0, last: t };
        e.failures++;
        e.last = t;
        this.entries.set(key, e);
        return { failures: e.failures, retryAfterMs: this.delayFor(e.failures) };
    }
    failures(key) { const e = this._entry(key, this.now()); return e ? e.failures : 0; }
    reset(key) { this.entries.delete(key); }
}

/** Approximate count of events in the last `windowMs` (two fixed windows, weighted). */
export class SlidingWindowCounter {
    constructor(windowMs, now = Date.now) {
        this.windowMs = windowMs;
        this.now = now;
        this.start = 0;
        this.cur = 0;
        this.prev = 0;
    }
    _roll(t) {
        const w = Math.floor(t / this.windowMs) * this.windowMs;
        if (w === this.start) return;
        this.prev = w - this.start === this.windowMs ? this.cur : 0;
        this.cur = 0;
        this.start = w;
    }
    add(n = 1) { const t = this.now(); this._roll(t); this.cur += n; return this.count(); }
    count() {
        const t = this.now();
        this._roll(t);
        const frac = (t - this.start) / this.windowMs;
        return this.prev * (1 - frac) + this.cur;
    }
}

/**
 * In-process implementation of the primary's rate-limit and single-use-token requests, with the
 * reply shapes of the IPC catalog:
 *   ratelimit.take { key, limit, windowMs, cost } -> { allowed, retryAfterMs, count }
 *   once.consume   { key, ttlMs }                 -> { fresh }
 * ratelimit.take counts over a sliding window (two weighted fixed windows); a refused take is
 * not counted.
 * @param {{ now?: () => number, maxKeys?: number }} [opts]
 * @returns {{ request(type: string, payload: object): Promise<object>, take(p: object): object, consume(p: object): object }}
 */
export function createLocalControl({ now = Date.now, maxKeys = 200000 } = {}) {
    const windows = new LruMap(maxKeys);
    const once = new LruMap(maxKeys);
    function take({ key, limit, windowMs, cost = 1 }) {
        const t = now();
        const k = `${key}\u0001${windowMs}`;
        let w = windows.get(k);
        const start = Math.floor(t / windowMs) * windowMs;
        if (!w) { w = { start, cur: 0, prev: 0 }; windows.set(k, w); }
        if (w.start !== start) {
            w.prev = start - w.start === windowMs ? w.cur : 0;
            w.cur = 0;
            w.start = start;
        }
        const frac = (t - start) / windowMs;
        const est = w.prev * (1 - frac) + w.cur;
        if (est + cost > limit) {
            let retryAfterMs = start + windowMs - t;
            const free = limit - w.cur - cost;
            if (free >= 0 && w.prev > 0) retryAfterMs = Math.max(1, Math.ceil(windowMs * (1 - free / w.prev) - (t - start)));
            return { allowed: false, retryAfterMs: Math.max(1, retryAfterMs), count: Math.ceil(est) };
        }
        w.cur += cost;
        return { allowed: true, retryAfterMs: 0, count: Math.ceil(est + cost) };
    }
    function consume({ key, ttlMs }) {
        const t = now();
        const exp = once.peek(key);
        if (exp !== undefined && exp > t) return { fresh: false };
        once.set(key, t + Math.max(1, ttlMs));
        return { fresh: true };
    }
    return {
        take,
        consume,
        async request(type, payload) {
            if (type === 'ratelimit.take') return take(payload);
            if (type === 'once.consume') return consume(payload);
            return {};
        },
    };
}
