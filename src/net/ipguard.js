// Protection per address of one worker: the background layer that every request and every
// connection meets first, before routing, before authentication and, in TLS_MODE=native, before
// any TLS work (docs/DESIGN.md section 8, docs/SIZING.md "Protection per address"). Quotas per
// signed-in account come later in the pipeline (http/server.js); this layer only stops one network
// address from saturating the server, and is loose enough for the players a school or a mobile
// operator (carrier-grade NAT) puts behind one address.
//
// An address is an IPv4 address or an IPv6 /64 ("k64"); an IPv6 address also counts toward its /48
// ("k48", one customer's site, 65536 /64 networks) with 4 times the limit, so that rotating over
// the /64s of a /48 does not multiply it. A whole-server limit L is enforced in each worker as
// workerShare(L, WORKERS) = max(1, min(L, ceil(2 L / WORKERS))) (security/ratelimit.js): no IPC on
// the request path; the exact sum over the workers is only needed to decide a block, below.
//
//   request(keys)      every HTTP request (any path or method, health checks and WebSocket upgrades
//                      included): blocked? then one token of HTTP_RATE_PER_IP (k64) and of
//                      HTTP_RATE_PER_PREFIX (k48), burst half a minute. A refusal by the /48 gives
//                      the /64 token back.
//   enter / leave      requests in progress: IP_MAX_INFLIGHT per k64 (4x per k48) in this worker.
//   connection(socket) a new TCP connection (TlsGate, native mode): blocked? then IP_CONN_RATE new
//                      connections per second (burst 4 s; 4x per /48) and IP_MAX_CONNECTIONS open
//                      ones (4x per /48); counted open until the socket closes (connectionClosed,
//                      idempotent).
//   noteRefusal(k, w)  a refusal counted toward a block (weights in DESIGN 8): the per-address
//                      refusals above, the TLS gate's per-address refusals, malformed HTTP. Not
//                      counted: per-account limits, server-wide capacity refusals, and requests
//                      refused because the address is already blocked.
//
// Blocks: the refusals of each key are summed per interval and reported to the primary once per
// REPORT_INTERVAL_MS (one 'abuse.report' notification with at most REPORT_MAX_ENTRIES keys, the
// largest first; at most REFUSAL_KEYS_PER_INTERVAL keys are counted in an interval), never per
// request. The primary's AbuseTracker (cluster/abuse.js) sums every worker over a sliding minute
// and sends the new blocks to every worker ('abuse.block' -> applyBlocks). Local fast path: a key
// that reaches ABUSE_BLOCK_REFUSALS_PER_MIN within one interval in this worker alone (a flood of
// 600 refusals in a second with the defaults) is blocked here at once for ABUSE_BLOCK_BASE_SEC,
// and still reported (scacelith_abuse_local_blocks_total). Without a primary (`report` null: tests,
// an API handler used alone), the guard runs its own AbuseTracker.
// A blocked address gets an RST before TLS for its new connections (TlsGate), 429 rate_limited with
// the time left and Connection: close on the connections it already has, and 429 on WebSocket
// upgrades. WebSockets already open are never closed by a block: a player who shares the address
// with an abuser keeps the game in progress.
//
// ABUSE_EXEMPT (addresses and CIDR subnets) skips all of this: no budget, no connection or
// in-flight cap, never counted, never blocked. Login, registration and per-account limits apply
// to exempt addresses as to the others (they are not part of this layer).
//
// Cost: keys are computed once per socket (cached on it, re-computed only when the client address
// of a request differs, as behind a proxy; with native TLS, twice per connection: for the raw
// socket at admission and for its TLS socket, another object, at the first request or upgrade);
// a request then costs one or two Map lookups for the blocks and one or two token-bucket updates
// (about 1 µs measured, see test/unit/net.ipguard.test.js and docs/SIZING.md). Memory is
// bounded: 50,000 buckets per limiter (an evicted bucket is full again, which only makes the
// limit more lenient; the /48 bucket bounds /64 rotation), counters only for open connections and
// requests in progress, ABUSE_MAX_BLOCKS blocks.

import { performance } from 'node:perf_hooks';
import { AbuseTracker, ABUSE_MAX_BLOCKS, ABUSE_PREFIX_FACTOR } from '../cluster/abuse.js';
import { expandIPv6 } from '../log.js';
import { metrics as defaultRegistry } from '../metrics.js';
import { TokenBucketLimiter, workerShare } from '../security/ratelimit.js';
import { ipMatcher, normalizeIp } from './ip.js';

/** Interval of the refusal reports to the primary. */
export const REPORT_INTERVAL_MS = 1000;
/** Keys in one report (the largest weights). */
export const REPORT_MAX_ENTRIES = 512;
/** Keys counted in one interval (new keys beyond it are dropped, counted). */
export const REFUSAL_KEYS_PER_INTERVAL = 10000;
/** Refusal weight of the login, registration, password reset and MFA family (http/server.js). */
export const AUTH_REFUSAL_WEIGHT = 5;
/** Request budget burst: this much of the per-minute rate. */
const REQUEST_BURST_MS = 30000;
/** New-connection burst: this much of the per-second rate. */
const CONN_BURST_MS = 4000;
const MAX_TTL_MS = 7 * 86400000;
const LIMITER_KEYS = 50000;

const kKeys = Symbol('scacelith.ipguardKeys');     // cached keys of a socket ({ ip, k64, k48, exempt })
const kOpen = Symbol('scacelith.ipguardOpen');     // keys of a socket counted open

/**
 * The keys of a client address: k64 (IPv4 address or IPv6 /64, as net/ip.js ipGroupKey) and k48
 * (IPv6 /48, null for IPv4). An address that cannot be read is 'unknown' and exempt (the socket
 * of such an address is gone already).
 * @param {string} ip
 * @returns {{ ip: string, k64: string, k48: string|null, exempt: boolean }}
 */
export function addressKeys(ip, exempt = null) {
    const a = normalizeIp(ip);
    if (!a) return { ip: String(ip ?? ''), k64: 'unknown', k48: null, exempt: true };
    const ex = exempt !== null && exempt(a);
    if (!a.includes(':')) return { ip: String(ip), k64: a, k48: null, exempt: ex };
    const parts = expandIPv6(a);
    if (!parts) return { ip: String(ip), k64: a, k48: null, exempt: ex };
    return { ip: String(ip), k64: parts.slice(0, 4).join(':') + '::/64', k48: parts.slice(0, 3).join(':') + '::/48', exempt: ex };
}

function dec(map, key) {
    const n = map.get(key);
    if (n === undefined) return;
    if (n <= 1) map.delete(key); else map.set(key, n - 1);
}

export class IpGuard {
    /**
     * @param {object} o
     * @param {object} o.config HTTP_RATE_PER_IP, HTTP_RATE_PER_PREFIX, IP_CONN_RATE, IP_MAX_CONNECTIONS,
     *   IP_MAX_INFLIGHT, ABUSE_BLOCK_*, ABUSE_EXEMPT
     * @param {number} [o.workers] worker processes sharing the limits (default config.workers)
     * @param {object} [o.registry]
     * @param {() => number} [o.now] monotonic clock, ms
     * @param {((entries: Array<[string, string|null, number]>) => void)|null} [o.report] sends a report to
     *   the primary ('abuse.report'); null: a local AbuseTracker decides
     * @param {object} [o.log]
     * @param {number} [o.reportIntervalMs]
     */
    constructor({ config, workers = config.workers, registry = defaultRegistry, now = () => performance.now(), report = null, log = null,
        reportIntervalMs = REPORT_INTERVAL_MS }) {
        const n = Math.max(1, Math.floor(workers) || 1);
        const exemptList = config.abuseExempt || [];
        this.exempt = exemptList.length ? ipMatcher(exemptList) : null;
        this.now = now;
        this.log = log;
        const perIp = config.httpRatePerIp ?? 600;
        const rate = workerShare(perIp, n), rate48 = workerShare(config.httpRatePerPrefix || 4 * perIp, n);
        // A bucket of `burst` tokens refilled at `rate` per minute: take(key, burst, burst * 60000 / rate).
        this.reqBurst = Math.max(1, Math.round(rate * REQUEST_BURST_MS / 60000));
        this.reqWindow = this.reqBurst * 60000 / rate;
        this.reqBurst48 = Math.max(1, Math.round(rate48 * REQUEST_BURST_MS / 60000));
        this.reqWindow48 = this.reqBurst48 * 60000 / rate48;
        const conn = workerShare(config.ipConnRate ?? 10, n);
        this.connBurst = Math.max(1, Math.round(conn * CONN_BURST_MS / 1000));
        this.connBurst48 = ABUSE_PREFIX_FACTOR * this.connBurst;
        this.maxOpen = workerShare(config.ipMaxConnections ?? 128, n);
        this.maxOpen48 = ABUSE_PREFIX_FACTOR * this.maxOpen;
        this.maxInflight = Math.max(1, config.ipMaxInflight ?? 32);
        this.maxInflight48 = ABUSE_PREFIX_FACTOR * this.maxInflight;
        this.threshold = Math.max(0, config.abuseBlockRefusalsPerMin ?? 600);
        this.baseMs = 1000 * Math.max(1, config.abuseBlockBaseSec ?? 60);
        this.reportIntervalMs = reportIntervalMs;

        this.reqs = new TokenBucketLimiter({ maxKeys: LIMITER_KEYS, now });
        this.reqs48 = new TokenBucketLimiter({ maxKeys: LIMITER_KEYS, now });
        this.conns = new TokenBucketLimiter({ maxKeys: LIMITER_KEYS, now });
        this.conns48 = new TokenBucketLimiter({ maxKeys: LIMITER_KEYS, now });
        /** @type {Map<string, number>} open connections per k64 / k48 */
        this.open = new Map();
        this.open48 = new Map();
        this.openTotal = 0;
        /** @type {Map<string, number>} requests in progress per k64 / k48 */
        this.inflight = new Map();
        this.inflight48 = new Map();
        this.inflightTotal = 0;
        /** @type {Map<string, {until: number, level: number}>} blocked keys (k64 or k48), monotonic deadline */
        this.blocks = new Map();
        /** @type {Map<string, {k48: string|null, w: number, local: boolean}>} refusals of this interval */
        this.refusals = new Map();
        this.report = report;
        this.tracker = report ? null : new AbuseTracker({ config, now, broadcast: (b) => this.applyBlocks(b), log, registry });
        this._timer = null;
        this._onTimer = () => { this._timer = null; this.flushReports(); };
        const guard = this;
        this._onSocketClose = function onGuardedSocketClose() { guard.connectionClosed(this); };

        const r = registry;
        const limited = r.counter('scacelith_http_rate_limited_total', 'API requests refused by a rate limit', ['limit']);
        this._rlIp = limited.labels('ip');
        this._rlIp48 = limited.labels('ip48');
        this._rlInflight = limited.labels('inflight');
        this._rlBlocked = limited.labels('blocked');
        this._localBlocks = r.counter('scacelith_abuse_local_blocks_total', 'Addresses a worker blocked by itself (a flood within one report interval)');
        this._dropped = r.counter('scacelith_abuse_report_entries_dropped_total', 'Refusal keys left out of a report to the primary');
        r.gaugeFn('scacelith_tls_connections_open', 'Connections admitted by the TLS gate and still open (TLS_MODE=native)', () => this.openTotal, { perShard: true });
        r.gaugeFn('scacelith_http_inflight', 'HTTP requests in progress (exempt addresses not counted)', () => this.inflightTotal, { perShard: true });
        r.gaugeFn('scacelith_abuse_blocked_keys', 'Blocked addresses this worker knows', () => this.blockedCount(), { perShard: true });
    }

    /**
     * The keys of an address (see addressKeys), with ABUSE_EXEMPT applied.
     * @param {string} ip
     */
    keys(ip) { return addressKeys(ip, this.exempt); }

    /**
     * The keys of the client of a request, cached on its socket while the address stays the same
     * (one computation per socket; behind a proxy, per change of client).
     * @param {string} ip client address
     * @param {object} [socket]
     */
    keysOf(ip, socket) {
        if (socket) {
            const c = socket[kKeys];
            if (c !== undefined && c.ip === ip) return c;
            const k = this.keys(ip);
            socket[kKeys] = k;
            return k;
        }
        return this.keys(ip);
    }

    /** Whether an address is in ABUSE_EXEMPT. */
    isExempt(ip) { return this.keys(ip).exempt; }

    /**
     * One request: the block, then the budget of its address. Returns null when it may go on, or
     * why it is refused and when to try again.
     * @param {string|{k64: string, k48: string|null, exempt: boolean}} k keys (or an address)
     * @returns {null|{ reason: 'blocked'|'ip'|'ip48', retryAfterMs: number }}
     */
    request(k) {
        if (typeof k !== 'object' || k === null) k = this.keys(k);
        if (k.exempt) return null;
        if (this.blocks.size !== 0) {
            const left = this._blockedFor(k, this.now());
            if (left > 0) { this._rlBlocked.inc(); return { reason: 'blocked', retryAfterMs: left }; }
        }
        const a = this.reqs.take(k.k64, this.reqBurst, this.reqWindow, 1);
        if (!a.allowed) {
            this._rlIp.inc();
            this.noteRefusal(k, 1);
            return { reason: 'ip', retryAfterMs: a.retryAfterMs };
        }
        if (k.k48 !== null) {
            const b = this.reqs48.take(k.k48, this.reqBurst48, this.reqWindow48, 1);
            if (!b.allowed) {
                this.reqs.give(k.k64, this.reqBurst, this.reqWindow, 1);
                this._rlIp48.inc();
                this.noteRefusal(k, 1);
                return { reason: 'ip48', retryAfterMs: b.retryAfterMs };
            }
        }
        return null;
    }

    /**
     * Counts a request in progress; false (counted as a refusal) when its address already has
     * IP_MAX_INFLIGHT of them in this worker (4x for its /48). Exempt addresses are not counted.
     * Every true must be followed by exactly one leave().
     */
    enter(k) {
        if (k.exempt) return true;
        const n = this.inflight.get(k.k64) || 0;
        if (n >= this.maxInflight) return this._inflightRefused(k);
        if (k.k48 !== null) {
            const m = this.inflight48.get(k.k48) || 0;
            if (m >= this.maxInflight48) return this._inflightRefused(k);
            this.inflight48.set(k.k48, m + 1);
        }
        this.inflight.set(k.k64, n + 1);
        this.inflightTotal++;
        return true;
    }

    /** Ends a request counted by enter(). */
    leave(k) {
        if (k.exempt) return;
        dec(this.inflight, k.k64);
        if (k.k48 !== null) dec(this.inflight48, k.k48);
        this.inflightTotal--;
    }

    _inflightRefused(k) {
        this._rlInflight.inc();
        this.noteRefusal(k, 1);
        return false;
    }

    /**
     * Admission of a new TCP connection (TlsGate, before anything else). Returns null when it may
     * go on (and counts it open until its close), or the reason to close it with an RST.
     * @param {import('node:net').Socket} socket
     * @returns {null|'blocked'|'conn_rate'|'conn_open'}
     */
    connection(socket) {
        const k = this.keys(socket.remoteAddress);
        socket[kKeys] = k;
        if (k.exempt) return null;
        if (this.blocks.size !== 0 && this._blockedFor(k, this.now()) > 0) return 'blocked';
        if (!this.conns.take(k.k64, this.connBurst, CONN_BURST_MS, 1).allowed) { this.noteRefusal(k, 1); return 'conn_rate'; }
        if (k.k48 !== null && !this.conns48.take(k.k48, this.connBurst48, CONN_BURST_MS, 1).allowed) {
            this.conns.give(k.k64, this.connBurst, CONN_BURST_MS, 1);
            this.noteRefusal(k, 1);
            return 'conn_rate';
        }
        const n = this.open.get(k.k64) || 0;
        if (n >= this.maxOpen) { this.noteRefusal(k, 1); return 'conn_open'; }
        if (k.k48 !== null) {
            const m = this.open48.get(k.k48) || 0;
            if (m >= this.maxOpen48) { this.noteRefusal(k, 1); return 'conn_open'; }
            this.open48.set(k.k48, m + 1);
        }
        this.open.set(k.k64, n + 1);
        this.openTotal++;
        socket[kOpen] = k;
        socket.once('close', this._onSocketClose);
        return null;
    }

    /** A connection counted by connection() is closed (idempotent; other sockets are ignored). */
    connectionClosed(socket) {
        const k = socket ? socket[kOpen] : undefined;
        if (k === undefined || k === null) return;
        socket[kOpen] = null;
        socket.removeListener('close', this._onSocketClose);
        dec(this.open, k.k64);
        if (k.k48 !== null) dec(this.open48, k.k48);
        this.openTotal--;
    }

    /** The keys the gate computed for a raw socket (connection()), or computed now. */
    socketKeys(socket) {
        const k = socket ? socket[kKeys] : undefined;
        return k !== undefined ? k : this.keys(socket?.remoteAddress);
    }

    /**
     * Counts a refusal of an address toward a block (a few Map operations; the report leaves on
     * a timer, at most once per REPORT_INTERVAL_MS).
     * @param {string|object} k keys or an address
     * @param {number} [weight] 1, or AUTH_REFUSAL_WEIGHT for the login / registration / reset / MFA family
     */
    noteRefusal(k, weight = 1) {
        if (!(this.threshold > 0)) return;
        if (typeof k !== 'object' || k === null) k = this.keys(k);
        if (k.exempt || !(weight > 0)) return;
        if (this.blocks.size !== 0 && this._blockedFor(k, this.now()) > 0) return;
        let e = this.refusals.get(k.k64);
        if (e === undefined) {
            if (this.refusals.size >= REFUSAL_KEYS_PER_INTERVAL) { this._dropped.inc(); return; }
            e = { k48: k.k48, w: 0, local: false };
            this.refusals.set(k.k64, e);
        }
        e.w += weight;
        if (!e.local && e.w >= this.threshold) {
            e.local = true;
            this._localBlocks.inc();
            this.applyBlocks([[k.k64, this.baseMs, 1]]);
        }
        if (this._timer === null) {
            this._timer = setTimeout(this._onTimer, this.reportIntervalMs);
            this._timer.unref?.();
        }
    }

    /**
     * Sends the refusals of the interval (largest first, at most REPORT_MAX_ENTRIES) to the
     * primary, or to the local tracker. Called by the timer; tests call it directly.
     * @returns {Array<[string, string|null, number]>} what was reported
     */
    flushReports() {
        if (this._timer !== null) { clearTimeout(this._timer); this._timer = null; }
        if (this.refusals.size === 0) return [];
        const entries = [];
        for (const [k64, e] of this.refusals) entries.push([k64, e.k48, e.w]);
        this.refusals.clear();
        entries.sort((a, b) => b[2] - a[2]);
        if (entries.length > REPORT_MAX_ENTRIES) {
            this._dropped.inc(entries.length - REPORT_MAX_ENTRIES);
            entries.length = REPORT_MAX_ENTRIES;
        }
        try {
            if (this.report) this.report(entries);
            else this.tracker.report(entries);
        } catch (err) {
            this.log?.warn?.('abuse report failed', { err });
        }
        return entries;
    }

    /**
     * Applies blocks decided by the primary ('abuse.block'): [[key, ttlMs, level], ...]. A key
     * already blocked for longer keeps its deadline. Returns the number of blocks set or extended.
     */
    applyBlocks(blocks) {
        if (!Array.isArray(blocks)) return 0;
        const now = this.now();
        let n = 0;
        for (const b of blocks) {
            if (!Array.isArray(b)) continue;
            const key = b[0], ttl = +b[1], level = Math.max(1, Math.floor(+b[2]) || 1);
            if (typeof key !== 'string' || !key || !(ttl > 0)) continue;
            const until = now + Math.min(ttl, MAX_TTL_MS);
            const cur = this.blocks.get(key);
            if (cur !== undefined) {
                if (level > cur.level) cur.level = level;
                if (cur.until >= until) continue;
                cur.until = until;
                n++;
                continue;
            }
            while (this.blocks.size >= ABUSE_MAX_BLOCKS) this.blocks.delete(this.blocks.keys().next().value);
            this.blocks.set(key, { until, level });
            n++;
        }
        return n;
    }

    /**
     * Milliseconds left in the block of an address (its k64 or its k48), 0 when it is not blocked.
     * @param {string|object} k keys or an address
     */
    blockedFor(k) {
        if (typeof k !== 'object' || k === null) k = this.keys(k);
        if (k.exempt || this.blocks.size === 0) return 0;
        return this._blockedFor(k, this.now());
    }

    _blockedFor(k, now) {
        let b = this.blocks.get(k.k64);
        if (b !== undefined) {
            if (b.until > now) return b.until - now;
            this.blocks.delete(k.k64);
        }
        if (k.k48 !== null) {
            b = this.blocks.get(k.k48);
            if (b !== undefined) {
                if (b.until > now) return b.until - now;
                this.blocks.delete(k.k48);
            }
        }
        return 0;
    }

    /** Running blocks (expired ones are dropped). */
    blockedCount() {
        const now = this.now();
        for (const [key, b] of this.blocks) if (b.until <= now) this.blocks.delete(key);
        return this.blocks.size;
    }

    /** Stops the report timer (the refusals not reported yet are dropped). */
    close() {
        if (this._timer !== null) { clearTimeout(this._timer); this._timer = null; }
        this.refusals.clear();
    }
}
