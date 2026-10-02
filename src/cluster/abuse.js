// Escalating temporary blocks of client addresses (primary). The workers count what they refuse
// to each address (net/ipguard.js IpGuard.noteRefusal: rate-limit 429s, connections refused
// before TLS, malformed requests, failed TLS handshakes) and report the sums once a second at most
// ('abuse.report' { entries: [[key64, key48|null, weight], ...] }, never per request). The tracker
// adds them up over every worker in a sliding minute and, when an address keeps going after being
// refused, blocks it; the new blocks go to every worker ('abuse.block' { blocks: [[key, ttlMs,
// level], ...] }), which then close the address's new connections with an RST before any TLS work
// and answer its requests 429 (docs/DESIGN.md sections 5.7 and 8).
//
// Keys are those of net/ip.js ipGroupKey: an IPv4 address, an IPv6 /64 ('a:b:c:d::/64') or, for
// the sum of a whole site, an IPv6 /48 ('a:b:c::/48'; IPv4 has none).
//
// Decision:
//   - an IPv4 address or IPv6 /64 is blocked when its weighted refusals of the last minute reach
//     ABUSE_BLOCK_REFUSALS_PER_MIN (T), all workers together; an IPv6 /48 when its /64s together
//     reach 4 T, or when ABUSE_PREFIX_BLOCK_AFTER (4) of its /64s are blocked at the same time;
//   - level: the key's previous level + 1, or 1 when its last block began more than
//     ABUSE_BLOCK_FORGET_MS (6 h) ago; duration min(ABUSE_BLOCK_MAX_SEC, ABUSE_BLOCK_BASE_SEC *
//     4^(level - 1)): 1 min, 4 min, 16 min, then 1 h with the defaults;
//   - reports for a key that is blocked (or whose /48 is) are ignored: the workers no longer
//     count its refused requests anyway, and a flood that resumes after the block escalates. The
//     refusals that led to a block are forgotten with it: after the block the key's count starts
//     from zero (the /48 of a blocked /64 keeps its own);
//   - at most ABUSE_MAX_BLOCKS (20,000) running blocks: beyond that the oldest end first
//     (scacelith_abuse_blocks_evicted_total); that many attacking addresses is the provider's case
//     (docs/SIZING.md, provider firewall);
//   - T = 0 turns blocking off (the per-address budgets of the workers still apply).
//
// The clock is monotonic (performance.now) and durations travel as durations: each worker sets the
// deadline on its own monotonic clock, so a step of the wall clock never moves a block. A worker
// that (re)starts gets the running blocks at its 'shard.ready' (snapshot()). A worker with no
// primary (tests, an API handler used alone) runs its own AbuseTracker (IpGuard).
//
// Cost: one sliding-window update per reported key (at most 512 per worker and second), O(1),
// except a new key when the window holds maxKeys keys that keep expiring: the eviction then walks
// the window's map (limits.js).

import { performance } from 'node:perf_hooks';
import { ipForLog } from '../log.js';
import { metrics as defaultRegistry } from '../metrics.js';
import { SlidingWindowLimiter } from './limits.js';

/** A key whose last block began longer ago than this starts again at level 1. */
export const ABUSE_BLOCK_FORGET_MS = 6 * 3600000;
/** /64 networks of one /48 blocked at the same time that block the /48 itself. */
export const ABUSE_PREFIX_BLOCK_AFTER = 4;
/** Running blocks kept by the primary (and by each worker). */
export const ABUSE_MAX_BLOCKS = 20000;
/** Each new block of a key within ABUSE_BLOCK_FORGET_MS lasts this many times longer. */
export const ABUSE_BLOCK_FACTOR = 4;
/** The /48 threshold, in /64 thresholds. */
export const ABUSE_PREFIX_FACTOR = 4;
const WINDOW_MS = 60000;
const MAX_WEIGHT = 1e6;           // one report entry (a worker refuses far less in one second)

export class AbuseTracker {
    /**
     * @param {object} o
     * @param {object} o.config ABUSE_BLOCK_REFUSALS_PER_MIN, ABUSE_BLOCK_BASE_SEC, ABUSE_BLOCK_MAX_SEC
     * @param {() => number} [o.now] monotonic clock, ms
     * @param {((blocks: Array<[string, number, number]>) => void)|null} [o.broadcast] receives the new blocks of each report
     * @param {object} [o.log]
     * @param {object} [o.registry]
     * @param {number} [o.maxBlocks]
     * @param {number} [o.maxKeys] keys of the sliding window
     */
    constructor({ config, now = () => performance.now(), broadcast = null, log = null, registry = defaultRegistry,
        maxBlocks = ABUSE_MAX_BLOCKS, maxKeys = 100000 }) {
        this.threshold = Math.max(0, Math.floor(config.abuseBlockRefusalsPerMin ?? 600));
        this.baseMs = 1000 * Math.max(1, config.abuseBlockBaseSec ?? 60);
        this.maxMs = Math.max(this.baseMs, 1000 * (config.abuseBlockMaxSec ?? 3600));
        this.now = now;
        this.broadcast = broadcast;
        this.log = log;
        this.maxBlocks = Math.max(1, maxBlocks);
        this.window = new SlidingWindowLimiter({ maxKeys, now });
        /** @type {Map<string, {until: number, level: number, scope: 'ip'|'prefix', key48: string|null}>} running blocks, oldest first */
        this.blocks = new Map();
        /** @type {Map<string, {level: number, at: number}>} the last block of each key */
        this.history = new Map();
        /** @type {Map<string, number>} /48 -> its /64s blocked now */
        this.blockedPerPrefix = new Map();
        this.counts = { ip: 0, prefix: 0 };
        const r = registry;
        this._blocksTotal = r.counter('scacelith_abuse_blocks_total', 'Addresses blocked before TLS by the primary, by scope (ip: IPv4 address or IPv6 /64; prefix: IPv6 /48) and level (4: the fourth block within 6 h or later)', ['scope', 'level']);
        const blocked = r.gauge('scacelith_abuse_blocked', 'Running blocks of addresses, by scope', ['scope']);
        this._gIp = blocked.labels('ip');
        this._gPrefix = blocked.labels('prefix');
        this._evicted = r.counter('scacelith_abuse_blocks_evicted_total', 'Blocks ended early because ABUSE_MAX_BLOCKS were running');
    }

    /** Whether blocking is on (ABUSE_BLOCK_REFUSALS_PER_MIN > 0). */
    get enabled() { return this.threshold > 0; }

    /** Running blocks (expired ones may linger until the next sweep). */
    get size() { return this.blocks.size; }

    /**
     * Adds the refusals one worker counted and blocks the keys that went over. The new blocks are
     * passed to `broadcast` (one call) and returned.
     * @param {Array<[string, string|null, number]>} entries
     * @returns {Array<[string, number, number]>} new blocks: [key, ttlMs, level]
     */
    report(entries) {
        if (!this.enabled || !Array.isArray(entries)) return [];
        const now = this.now();
        const fresh = [];
        const t = this.threshold;
        for (const e of entries) {
            if (!Array.isArray(e)) continue;
            const key64 = e[0], key48 = typeof e[1] === 'string' && e[1] ? e[1] : null, w = Math.min(MAX_WEIGHT, +e[2]);
            if (typeof key64 !== 'string' || !key64 || !(w > 0)) continue;
            if (this._isBlocked(key64, now) || (key48 !== null && this._isBlocked(key48, now))) continue;
            // limit T - 1: the take that brings the sum to T is the one refused, so T refusals block.
            const r = this.window.take({ key: key64, limit: t - 1, windowMs: WINDOW_MS, cost: w }, now);
            if (!r.allowed) {
                this._block(key64, 'ip', key48, now, r.count + w, fresh);
                if (key48 !== null && (this.blockedPerPrefix.get(key48) || 0) >= ABUSE_PREFIX_BLOCK_AFTER) {
                    this._block(key48, 'prefix', null, now, 0, fresh);
                }
                continue;
            }
            if (key48 !== null) {
                const p = this.window.take({ key: key48, limit: ABUSE_PREFIX_FACTOR * t - 1, windowMs: WINDOW_MS, cost: w }, now);
                if (!p.allowed) this._block(key48, 'prefix', null, now, p.count + w, fresh);
            }
        }
        if (fresh.length && this.broadcast) {
            try { this.broadcast(fresh); } catch (err) { this.log?.error?.('abuse block broadcast failed', { err }); }
        }
        return fresh;
    }

    /**
     * The running blocks with the time each has left, for a worker that (re)starts.
     * @returns {Array<[string, number, number]>}
     */
    snapshot() {
        const now = this.now();
        const out = [];
        for (const [key, b] of this.blocks) {
            if (b.until > now) out.push([key, Math.ceil(b.until - now), b.level]);
        }
        return out;
    }

    /** Whether `key` is blocked now. */
    isBlocked(key) { return this._isBlocked(key, this.now()); }

    /** Ends the expired blocks and forgets old history (ControlPlane.sweep, every 10 s). */
    sweep() {
        const now = this.now();
        for (const [key, b] of this.blocks) if (b.until <= now) this._unblock(key, b);
        for (const [key, h] of this.history) if (now - h.at > ABUSE_BLOCK_FORGET_MS) this.history.delete(key);
        this.window.sweep(now);
    }

    _isBlocked(key, now) {
        const b = this.blocks.get(key);
        if (b === undefined) return false;
        if (b.until > now) return true;
        this._unblock(key, b);
        return false;
    }

    _block(key, scope, key48, now, refusals, fresh) {
        if (this._isBlocked(key, now)) return;
        const h = this.history.get(key);
        const level = h && now - h.at <= ABUSE_BLOCK_FORGET_MS ? h.level + 1 : 1;
        const ttlMs = Math.min(this.maxMs, this.baseMs * ABUSE_BLOCK_FACTOR ** Math.min(30, level - 1));
        while (this.blocks.size >= this.maxBlocks) {
            const [oldKey, old] = this.blocks.entries().next().value;
            this._unblock(oldKey, old);
            this._evicted.inc();
        }
        this.blocks.set(key, { until: now + ttlMs, level, scope, key48 });
        this.window.forget(key);
        this.history.delete(key);              // re-inserted: the Map keeps the newest last
        this.history.set(key, { level, at: now });
        if (this.history.size > 4 * this.maxBlocks) this.history.delete(this.history.keys().next().value);
        if (scope === 'prefix') { this.counts.prefix++; this._gPrefix.set(this.counts.prefix); } else { this.counts.ip++; this._gIp.set(this.counts.ip); }
        if (key48 !== null) this.blockedPerPrefix.set(key48, (this.blockedPerPrefix.get(key48) || 0) + 1);
        this._blocksTotal.labels(scope, String(Math.min(level, 4))).inc();
        fresh.push([key, ttlMs, level]);
        this.log?.warn?.('ip blocked', { ip: ipForLog(key), scope, blockLevel: level, ttlSec: Math.round(ttlMs / 1000), refusals: Math.round(refusals) || undefined });
    }

    _unblock(key, b) {
        if (this.blocks.get(key) !== b) return;
        this.blocks.delete(key);
        if (b.scope === 'prefix') { this.counts.prefix--; this._gPrefix.set(this.counts.prefix); } else { this.counts.ip--; this._gIp.set(this.counts.ip); }
        if (b.key48 !== null) {
            const n = (this.blockedPerPrefix.get(b.key48) || 0) - 1;
            if (n > 0) this.blockedPerPrefix.set(b.key48, n); else this.blockedPerPrefix.delete(b.key48);
        }
    }
}
