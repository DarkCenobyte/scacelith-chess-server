// Global rate limiter and single-use keys, kept in the primary (IPC 'ratelimit.take' and
// 'once.consume', DESIGN 5.7) so that every shard sees the same counters.
//
// SlidingWindowLimiter: sliding-window counter (the previous fixed window weighted by how much
// of it still overlaps the sliding window + the current window). Accurate to a few percent,
// O(1) time and ~100 bytes per key. Keys expire two windows after their last use; the map is
// bounded (maxKeys): beyond it the least recently inserted keys are evicted first, which can only
// make the limiter more lenient for the evicted keys, never block an innocent client.
//
// OnceStore: remembers keys until their TTL (single-use tokens: proof-of-work challenges, TOTP
// steps...). Bounded too; when full, the oldest insertions are evicted first. With the default
// bound (1M keys) and TTLs of minutes this would take a sustained flood of more than 10k fresh
// keys per second, which the per-IP rate limits make impossible before this point.

/**
 * Sliding-window counter keyed by string.
 */
export class SlidingWindowLimiter {
    /**
     * @param {{ maxKeys?: number, now?: () => number }} [o]
     */
    constructor({ maxKeys = 200000, now = Date.now } = {}) {
        this.maxKeys = maxKeys;
        this.now = now;
        /** @type {Map<string, {start:number, cur:number, prev:number, windowMs:number, last:number}>} */
        this.entries = new Map();
        this.evicted = 0;
    }

    /**
     * Takes `cost` units from the key's allowance of `limit` per `windowMs`.
     * @param {{ key: string, limit: number, windowMs: number, cost?: number }} p
     * @returns {{ allowed: boolean, retryAfterMs: number, count: number }}
     */
    take({ key, limit, windowMs, cost = 1 }, now = this.now()) {
        windowMs = Math.max(1, windowMs | 0);
        limit = Math.max(0, +limit || 0);
        cost = Math.max(0, +cost || 0);
        let e = this.entries.get(key);
        if (!e || e.windowMs !== windowMs) {
            if (!e && this.entries.size >= this.maxKeys) this._evict();
            e = { start: now - (now % windowMs), cur: 0, prev: 0, windowMs, last: now };
            this.entries.set(key, e);
        }
        this._advance(e, now);
        e.last = now;
        const elapsed = now - e.start;
        const weight = 1 - elapsed / windowMs;
        const estimate = e.prev * weight + e.cur;
        if (estimate + cost <= limit) {
            e.cur += cost;
            return { allowed: true, retryAfterMs: 0, count: Math.ceil(estimate + cost) };
        }
        return { allowed: false, retryAfterMs: this._retryAfter(e, now, limit, cost), count: Math.ceil(estimate) };
    }

    /**
     * Current estimate for a key (0 when unknown), without taking anything.
     * @param {string} key
     */
    peek(key, now = this.now()) {
        const e = this.entries.get(key);
        if (!e) return 0;
        this._advance(e, now);
        return e.prev * (1 - (now - e.start) / e.windowMs) + e.cur;
    }

    _advance(e, now) {
        const w = e.windowMs;
        if (now < e.start + w) return;
        const start = now - (now % w);
        e.prev = start - e.start === w ? e.cur : 0;
        e.cur = 0;
        e.start = start;
    }

    // Time until `cost` more units fit: the previous window's weight decays linearly, and the
    // current count only leaves when the window rolls over.
    _retryAfter(e, now, limit, cost) {
        const w = e.windowMs;
        const toNext = e.start + w - now;
        // Within the current window: prev * (1 - (t - start)/w) + cur + cost <= limit
        if (e.cur + cost <= limit && e.prev > 0) {
            const t = e.start + w * (1 - (limit - cost - e.cur) / e.prev);
            if (t > now && t <= e.start + w) return Math.ceil(t - now);
        }
        // After the rollover: cur becomes prev and decays over the next window.
        if (cost > limit) return toNext + w;
        if (e.cur === 0) return toNext;
        const frac = 1 - (limit - cost) / e.cur;          // fraction of the next window to wait
        return Math.ceil(toNext + Math.max(0, frac) * w);
    }

    _evict() {
        // Drop expired keys first; if none, the oldest insertions.
        const now = this.now();
        let removed = 0;
        for (const [k, e] of this.entries) {
            if (now - e.last > 2 * e.windowMs) { this.entries.delete(k); removed++; }
            if (removed >= 64) break;
        }
        if (removed) return;
        const it = this.entries.keys();
        for (let i = 0; i < 16; i++) {
            const r = it.next();
            if (r.done) break;
            this.entries.delete(r.value);
            this.evicted++;
        }
    }

    /** Removes keys unused for two windows. Call periodically. */
    sweep(now = this.now()) {
        let n = 0;
        for (const [k, e] of this.entries) if (now - e.last > 2 * e.windowMs) { this.entries.delete(k); n++; }
        return n;
    }

    get size() { return this.entries.size; }
}

/**
 * Single-use keys with a time to live.
 */
export class OnceStore {
    /**
     * @param {{ maxKeys?: number, now?: () => number }} [o]
     */
    constructor({ maxKeys = 1000000, now = Date.now } = {}) {
        this.maxKeys = maxKeys;
        this.now = now;
        /** @type {Map<string, number>} key -> expiresAt */
        this.keys = new Map();
        this.evicted = 0;
    }

    /**
     * Marks the key as used. `fresh` is true the first time (and again after its TTL).
     * @param {{ key: string, ttlMs: number }} p
     * @returns {{ fresh: boolean }}
     */
    consume({ key, ttlMs }, now = this.now()) {
        const exp = this.keys.get(key);
        if (exp !== undefined && exp > now) return { fresh: false };
        if (exp !== undefined) this.keys.delete(key);
        else if (this.keys.size >= this.maxKeys) this._evict(now);
        this.keys.set(key, now + Math.max(1, ttlMs | 0));
        return { fresh: true };
    }

    _evict(now) {
        const it = this.keys.entries();
        for (let i = 0; i < 64; i++) {
            const r = it.next();
            if (r.done) break;
            const [k, exp] = r.value;
            this.keys.delete(k);
            if (exp > now) this.evicted++;
        }
    }

    /** Removes expired keys. Call periodically. */
    sweep(now = this.now()) {
        let n = 0;
        for (const [k, exp] of this.keys) if (exp <= now) { this.keys.delete(k); n++; }
        return n;
    }

    get size() { return this.keys.size; }
}
