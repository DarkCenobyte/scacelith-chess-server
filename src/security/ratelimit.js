// Rate limiting building blocks, all memory-bounded:
//
//   LruMap                 bounded map, least recently used entries evicted first
//   TokenBucketLimiter     per-key token buckets (per-IP API limits, local first stage)
//   FailureCounter         per-key failure counter with exponential delay (login brute force)
//   SlidingWindowCounter   approximate count of events over the last window (failure-rate detector)
//   workerShare()          the part of a whole-server per-address limit one worker enforces
//   createLocalControl()   in-process implementation of the primary's `ratelimit.take`,
//                          `ratelimit.refund` and `once.consume` IPC requests (docs/DESIGN.md
//                          5.7). Used when no primary is available (single process, tests) and as
//                          the fallback when the primary does not answer.
//
// Complexity: every operation is O(1) (amortised), no timers: expiry is lazy.

import { OnceStore, SlidingWindowLimiter } from '../cluster/limits.js';
import { ipGroupKey, normalizeIp as normalizeAddress } from '../net/ip.js';

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
 * to its /64 (one customer network; net/ip.js ipGroupKey), anything that is not an address is
 * kept as is. See also prefixKey().
 * @param {string} ip
 * @returns {string}
 */
export function ipKey(ip) {
    const k = ipGroupKey(ip, 64);           // 'unknown' exactly when ip is not an address
    return k === 'unknown' ? String(ip || '') : k;
}

/**
 * The wider source of a client address: an IPv4 address itself (IPv4-mapped IPv6 included), or the
 * /48 of an IPv6 address. A /48 is one site: it holds 65536 /64 networks, each with its own ipKey()
 * bucket, and one customer or one hosting provider's client often gets a whole /48 or /56.
 * @param {string} ip
 * @returns {string}
 */
export function prefixKey(ip) {
    const k = ipGroupKey(ip, 48);
    return k === 'unknown' ? String(ip || '') : k;
}

/**
 * Normalises the address itself (net/ip.js normalizeIp: IPv4-mapped IPv6 -> IPv4, IPv6
 * lower-cased); anything that is not an address is kept as is.
 * @param {string} ip
 * @returns {string}
 */
export function normalizeIp(ip) {
    return normalizeAddress(ip) || String(ip || '');
}

/**
 * The part of a whole-server per-address limit `limit` that one of `workers` worker processes
 * enforces on its own, without asking the primary: max(1, min(limit, ceil(2 * limit / workers))).
 * The kernel spreads the connections of a client over the workers (round robin, or SO_REUSEPORT by
 * 4-tuple), so a client spread over every worker gets at most twice the limit, and one that uses a
 * single keep-alive connection at least 2 / workers of it; with 1 or 2 workers each one allows all
 * of it. The exact sum over the workers is only needed to decide a block, which the primary does
 * from the refusals the workers report (net/ipguard.js, cluster/abuse.js), off the request path.
 * @param {number} limit
 * @param {number} workers
 * @returns {number} at least 1
 */
export function workerShare(limit, workers) {
    const l = Math.max(0, Math.floor(+limit || 0));
    const n = Math.max(1, Math.floor(+workers || 1));
    return Math.max(1, Math.min(l, Math.ceil(2 * l / n)));
}

/**
 * Per-key token buckets: `limit` tokens refilled evenly over `windowMs`. A bucket that holds a
 * burst `b` and refills at `r` tokens per minute is `take(key, b, b * 60000 / r)`.
 */
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
    /**
     * Gives back `cost` tokens that take() granted to `key` (a request that did nothing, see
     * http/server.js), never beyond `limit`. A bucket evicted meanwhile is full already.
     */
    give(key, limit, windowMs, cost = 1) {
        const b = this.buckets.peek(key);
        if (!b) return;
        const t = this.now();
        b.tokens = Math.min(limit, b.tokens + Math.max(0, t - b.at) * (limit / windowMs) + cost);
        b.at = t;
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
 *   ratelimit.take   { key, limit, windowMs, cost }   -> { allowed, retryAfterMs, count }
 *   ratelimit.refund { key, windowMs, cost, ageMs }   -> { refunded }
 *   once.consume     { key, ttlMs }                   -> { fresh }
 * They are the primary's own SlidingWindowLimiter and OnceStore (cluster/limits.js), so that the
 * fallback answers exactly as the primary would (its Retry-After included). ratelimit.take counts
 * over a sliding window (two weighted fixed windows); a refused take is not counted.
 * ratelimit.refund takes back `cost` from the fixed window that counted a take made `ageMs` ago
 * (the current one or the one before; an older one no longer counts anyway).
 * @param {{ now?: () => number, maxKeys?: number }} [opts]
 * @returns {{ request(type: string, payload: object): Promise<object>, take(p: object): object, consume(p: object): object }}
 */
export function createLocalControl({ now = Date.now, maxKeys = 200000 } = {}) {
    const windows = new SlidingWindowLimiter({ maxKeys, now });
    const once = new OnceStore({ maxKeys, now });
    const take = (p) => windows.take(p);
    const refund = (p) => windows.refund(p);
    const consume = (p) => once.consume(p);
    return {
        take,
        refund,
        consume,
        async request(type, payload) {
            if (type === 'ratelimit.take') return take(payload);
            if (type === 'ratelimit.refund') return refund(payload);
            if (type === 'once.consume') return consume(payload);
            return {};
        },
    };
}
