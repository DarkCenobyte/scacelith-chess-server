// Global rate limiter and single-use keys, kept in the primary (IPC 'ratelimit.take',
// 'ratelimit.refund' and 'once.consume', DESIGN 5.7) so that every shard sees the same counters.
//
// SlidingWindowLimiter: sliding-window counter (the previous fixed window weighted by how much
// of it still overlaps the sliding window + the current window). Accurate to a few percent,
// amortised O(1) time and ~100 bytes per key. Keys expire two windows after their last use; the
// map is bounded (maxKeys): beyond it the expired keys go first, then the least recently inserted
// ones, which can only make the limiter more lenient for the evicted keys, never block an
// innocent client. The search for expired keys walks the map only when one may have expired
// since the last walk that found none (a lower bound of the expiries is kept), so a flood of
// fresh keys at capacity costs O(1) per key, not a walk of the whole map every 16 keys.
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
        // No key expires before this time (a lower bound; -Infinity: unknown). See _evict.
        this._noExpiryBefore = -Infinity;
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
        // On every take: a clock that stepped back lowers this key's expiry.
        const exp = now + 2 * windowMs - 1;
        if (exp < this._noExpiryBefore) this._noExpiryBefore = exp;
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
     * Gives back `cost` units that take() granted `ageMs` ago, from the fixed window that counted
     * them (the current one or the one before; an older window no longer counts). Used for a
     * request that ended without doing the work the limit protects (http/server.js).
     * @param {{ key: string, windowMs: number, cost?: number, ageMs?: number }} p
     * @returns {{ refunded: boolean }}
     */
    refund({ key, windowMs, cost = 1, ageMs = 0 }, now = this.now()) {
        windowMs = Math.max(1, windowMs | 0);
        cost = Math.max(0, +cost || 0);
        const e = this.entries.get(key);
        if (!e || e.windowMs !== windowMs) return { refunded: false };
        this._advance(e, now);
        const at = now - Math.max(0, +ageMs || 0);
        const start = at - (at % windowMs);
        if (start === e.start) e.cur = Math.max(0, e.cur - cost);
        else if (start === e.start - windowMs) e.prev = Math.max(0, e.prev - cost);
        else return { refunded: false };
        return { refunded: true };
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
        // Drop expired keys first; if none, the oldest insertions. A walk of the whole map records
        // the earliest expiry of the keys it keeps (minus 1 ms for rounding): until then no key can
        // have expired (take() lowers the bound for every key it touches), and the walk is skipped.
        const now = this.now();
        if (now >= this._noExpiryBefore) {
            let removed = 0, minExp = Infinity;
            for (const [k, e] of this.entries) {
                if (now - e.last > 2 * e.windowMs) { this.entries.delete(k); removed++; } else if (e.last + 2 * e.windowMs < minExp) minExp = e.last + 2 * e.windowMs;
                if (removed >= 64) break;
            }
            this._noExpiryBefore = removed >= 64 ? -Infinity : minExp - 1;     // a walk cut short knows no bound
            if (removed) return;
        }
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

    /** Forgets a key's counts: its next take() starts from zero. */
    forget(key) { this.entries.delete(key); }

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
