// Global rate limiter and single-use keys, kept in the primary (IPC 'ratelimit.take',
// 'ratelimit.refund' and 'once.consume', DESIGN 5.7) so that every shard sees the same counters.
//
// SlidingWindowLimiter: sliding-window counter (the previous fixed window weighted by how much
// of it still overlaps the sliding window + the current window). Accurate to a few percent,
// O(1) time below capacity and ~200 bytes per key. Keys expire two windows after their last use;
// the map is bounded (maxKeys): beyond it the expired keys go first, then the least recently
// inserted ones, which can only make the limiter more lenient for the evicted keys, never block
// an innocent client. The expired keys are found through a min-heap of the keys by expiry, not a
// walk of the map: a new key at capacity costs O(log n), whether no key has expired (a flood of
// fresh keys) or about one per new key. Only when more than 64 have expired does a walk from the
// oldest insertion pick the 64 to drop, and it then makes room for 64 new keys.
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
        /** @type {Map<string, {start:number, cur:number, prev:number, windowMs:number, last:number, key:string, exp:number, hi:number}>} */
        this.entries = new Map();
        this.evicted = 0;
        // Every entry, as a min-heap on `exp`: a lower bound of its expiry (last + 2 windows), which
        // take() lowers when the clock stepped back and _expired() raises; `hi` is its index here.
        this._heap = [];
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
            if (e) this._unfile(e);
            e = { start: now - (now % windowMs), cur: 0, prev: 0, windowMs, last: now, key, exp: now + 2 * windowMs, hi: -1 };
            this.entries.set(key, e);
            this._file(e);
        }
        this._advance(e, now);
        e.last = now;
        // A later expiry waits for _expired(); an earlier one (the clock stepped back) moves up now.
        const exp = now + 2 * windowMs;
        if (exp < e.exp) { e.exp = exp; this._up(e.hi); }
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
        // Drop expired keys first, the first 64 in insertion order (every one when fewer have
        // expired); if none, the oldest insertions.
        const now = this.now();
        const expired = this._expired(now, 65);
        if (expired.length > 64) {
            for (const e of expired) this._file(e);
            let removed = 0;
            for (const [k, e] of this.entries) {
                if (now - e.last > 2 * e.windowMs) { this._drop(k, e); removed++; }
                if (removed >= 64) break;
            }
            return;
        }
        for (const e of expired) this.entries.delete(e.key);
        if (expired.length) return;
        let n = 0;
        for (const [k, e] of this.entries) {
            this._drop(k, e);
            this.evicted++;
            if (++n >= 16) break;
        }
    }

    // Takes up to `max` expired entries out of the heap. The top entries whose bound has passed
    // but which were used since are filed again at their expiry: each is seen about once per two
    // windows of use. The bound never exceeds last + 2 windows, so once it is above `now` at the
    // top (an exact comparison: rounding cannot make it pass a key the expiry test would drop),
    // no key has expired.
    _expired(now, max) {
        const h = this._heap, out = [], live = [];
        while (h.length && h[0].exp <= now && out.length < max) {
            const e = h[0];
            this._unfile(e);
            if (now - e.last > 2 * e.windowMs) out.push(e);
            else { e.exp = e.last + 2 * e.windowMs; live.push(e); }
        }
        for (const e of live) this._file(e);
        return out;
    }

    /** Removes keys unused for two windows. Call periodically. */
    sweep(now = this.now()) {
        // Past a sixteenth of the keys (after a lull), building the heap again costs less than
        // taking each of them out.
        const few = this.entries.size >> 4;
        let n = 0;
        for (const [k, e] of this.entries) {
            if (now - e.last > 2 * e.windowMs) {
                this.entries.delete(k);
                if (++n <= few) this._unfile(e);
            }
        }
        if (n > few) this._rebuild();
        return n;
    }

    /** Forgets a key's counts: its next take() starts from zero. */
    forget(key) {
        const e = this.entries.get(key);
        if (e) this._drop(key, e);
    }

    get size() { return this.entries.size; }

    _drop(key, e) {
        this.entries.delete(key);
        this._unfile(e);
    }

    // ---- the heap (index `hi` kept in each entry) ----

    _file(e) {
        e.hi = this._heap.length;
        this._heap.push(e);
        this._up(e.hi);
    }

    _rebuild() {
        const h = this._heap = [...this.entries.values()];
        for (let i = 0; i < h.length; i++) h[i].hi = i;
        for (let i = (h.length >> 1) - 1; i >= 0; i--) this._down(i);
    }

    _unfile(e) {
        const h = this._heap, i = e.hi, last = h.pop();
        e.hi = -1;
        if (last === e) return;
        h[i] = last;
        last.hi = i;
        this._up(i);
        this._down(last.hi);
    }

    _up(i) {
        const h = this._heap, e = h[i];
        while (i > 0) {
            const p = (i - 1) >> 1;
            if (h[p].exp <= e.exp) break;
            h[i] = h[p]; h[i].hi = i;
            i = p;
        }
        h[i] = e; e.hi = i;
    }

    _down(i) {
        const h = this._heap, n = h.length, e = h[i];
        for (;;) {
            let c = 2 * i + 1;
            if (c >= n) break;
            if (c + 1 < n && h[c + 1].exp < h[c].exp) c++;
            if (h[c].exp >= e.exp) break;
            h[i] = h[c]; h[i].hi = i;
            i = c;
        }
        h[i] = e; e.hi = i;
    }
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
