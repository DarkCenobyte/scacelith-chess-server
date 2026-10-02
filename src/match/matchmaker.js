// Matchmaker (primary process): one queue per (official category, rated flag), paired every
// MATCH_TICK_MS by tick(now). Pure: no timer, no I/O; time is passed in (or read from the
// injected `now` clock), colours use the injected RNG.
//
// Pairing rule (DESIGN 5.4):
//   * window(player) = min(MATCH_WINDOW_START + floor(wait / MATCH_WINDOW_STEP_MS) * MATCH_WINDOW_STEP,
//                          MATCH_WINDOW_MAX) (+ MATCH_PROVISIONAL_BONUS when provisional)
//   * A and B may be paired only when |rA - rB| <= window(A) AND <= window(B) (mutual rule), they
//     are not in each other's `recentOpponents`, and (rated queues) they have not been paired for
//     MATCH_REPEAT_LIMIT rated games within MATCH_REPEAT_WINDOW_MS.
//   * Each tick walks every queue from the longest-waiting player to the newest; each player still
//     unpaired takes the valid partner with the closest rating (ties: the one who waited longer).
//   * Colours: the player with the higher colour balance (whites minus blacks) gets Black; equal
//     balances are drawn at random.
//
// Data structures (built for 100k+ waiting players):
//   * Each queue keeps its players in a doubly-linked FIFO list ordered by joinedAt, and buckets
//     them by rating: one bucket per rating point (exact, so "closest rating" is exact), each
//     bucket holding two FIFO lists (established and provisional players).
//   * A bitset of the non-empty buckets lets a search jump over empty ratings 32 at a time, going
//     outwards from the seeker's rating, closest first.
//   * Inside one bucket and one list, every player has the same rating difference to the seeker
//     and windows only shrink from the head (oldest) to the tail (newest): the first player that
//     is not excluded decides for the whole list. A search therefore costs O(number of non-empty
//     rating points inside the window) with a tiny constant, and a successful search in a dense
//     queue stops at the first bucket. Players left unpaired are more than MATCH_WINDOW_START
//     apart from each other (else they would have been paired), so a failing search meets at most
//     about 2 * (MATCH_WINDOW_MAX + bonus) / MATCH_WINDOW_START of them. A tick is O(n) per queue
//     (tools/bench-matchmaker.js: about 75 ms for 100k players joining at once, 50k pairs).
//   * Joins are appended: a joinedAt older than the last MAX_REORDER queued players is raised to
//     theirs (a late IPC message loses a few ms of waiting, the insertion stays O(1)).
//   * Repeat limit: a count per pair of users plus a time-ordered log that expires them; pair keys
//     are numbers (no allocation) while user ids are below 2^26.
//   * Colour balances: a Map userId -> balance (zero balances are dropped; beyond BALANCE_CAP
//     users the least recently updated are forgotten, i.e. reset to 0).
//
// Deviations and clarifications of the contract:
//   * join() refuses a user already queued with ErrorCode.QueueNotAllowed (leave first), and a
//     category that is not official with ErrorCode.InvalidCategory. The conduct cooldown and the
//     "already in a game" checks belong to the caller (the primary), before join().
//   * tick() does not count its rated pairings toward the repeat limit: the caller does, with
//     recordPairing(), once their game exists (a pairing whose game cannot be created counts
//     nothing, and its players may be paired again at once).
//   * join({ colorBalance }) overrides the balance tracked here; without it the matchmaker uses its
//     own record, updated at each pairing (and by recordColors() for games made elsewhere).
//   * join({ recentOpponents }) is an optional iterable of user ids the player must not be paired
//     with (for instance from store.games.countBetween after a restart); it applies in both
//     directions and in the queue it was given for.
//   * Exclusions (recentOpponents, repeat limit) are skipped at most SCAN_CAP times per bucket list
//     and search, which bounds a search even against a pathological exclusion list; a partner
//     hidden behind more than that is found by its own search or at a later tick.
//   * statusOf() returns null when the user is not queued.
//   * A pairing's white/black are plain copies of the queue entries plus `waitMs`.

import { enums } from '../protocol/schema.js';
import { metrics } from '../metrics.js';
import { parseCategory } from './elo.js';

const { ErrorCode, QueueState } = enums;

const MAX_RATING = 65535;          // PlayerInfo.rating is a u16
const INITIAL_CAPACITY = 4096;     // rating points covered before the first growth
const SCAN_CAP = 64;               // excluded entries skipped per list and bucket before giving up on it
const MAX_REORDER = 64;            // queued players a late-arriving join may be placed before
const BALANCE_CAP = 500000;        // colour balances kept in memory (least recently updated evicted)
const PAIR_KEY_SHIFT = 67108864;   // 2^26: numeric pair keys stay below 2^52

const mPairs = metrics.counter('scacelith_mm_pairs_total', 'Pairs made by the matchmaker', ['rated']);
const mPairsRated = mPairs.labels('true');
const mPairsCasual = mPairs.labels('false');
const mWait = metrics.histogram('scacelith_mm_wait_ms', 'Time spent in the queue before being paired',
    [1000, 2500, 5000, 10000, 20000, 30000, 60000, 120000, 300000]);
const mQueued = metrics.gauge('scacelith_mm_queued', 'Players waiting in the matchmaking queues');

function pairKey(a, b) {
    const lo = a < b ? a : b, hi = a < b ? b : a;
    return hi < PAIR_KEY_SHIFT ? lo * PAIR_KEY_SHIFT + hi : `${lo}:${hi}`;
}

function toSet(ids) {
    if (!ids) return null;
    const s = ids instanceof Set ? ids : new Set(ids);
    return s.size ? s : null;
}

// A waiting player.
class Entry {
    constructor(req, category, rated, rating, joinedAt, seq, colorBalance, recent) {
        this.userId = req.userId;
        this.username = req.username ?? '';
        this.category = category;
        this.rated = rated;
        this.rating = rating;
        this.provisional = !!req.provisional;
        this.shard = req.shard ?? 0;
        this.connId = req.connId ?? 0;
        this.colorBalance = colorBalance;
        this.joinedAt = joinedAt;
        this.seq = seq;             // tie-break of equal joinedAt: insertion order
        this.recent = recent;       // Set of excluded user ids, or null
        this.prev = null;           // queue FIFO
        this.next = null;
        this.lprev = null;          // bucket list
        this.lnext = null;
        this.queue = null;
    }
}

function before(a, b) {
    return a.joinedAt < b.joinedAt || (a.joinedAt === b.joinedAt && a.seq < b.seq);
}

// One rating point of one queue: FIFO lists of established (e*) and provisional (p*) players.
class Bucket {
    constructor() {
        this.eh = null; this.et = null;
        this.ph = null; this.pt = null;
        this.n = 0;
    }
}

class Queue {
    constructor(category, rated) {
        this.category = category;
        this.rated = rated;
        this.size = 0;
        this.head = null;
        this.tail = null;
        this.cap = INITIAL_CAPACITY;
        this.bits = new Uint32Array(INITIAL_CAPACITY >>> 5);
        this.buckets = [];
    }

    grow(rating) {
        let cap = this.cap;
        while (cap <= rating) cap *= 2;
        if (cap === this.cap) return;
        const bits = new Uint32Array(cap >>> 5);
        bits.set(this.bits);
        this.bits = bits;
        this.cap = cap;
    }

    insert(e) {
        if (e.rating >= this.cap) this.grow(e.rating);
        // Queue FIFO, ordered by joinedAt (appending is the normal case). A joinedAt older than
        // MAX_REORDER queued players is moved up to theirs, so an insertion stays O(1).
        let p = this.tail;
        let steps = 0;
        while (p !== null && before(e, p)) {
            if (++steps > MAX_REORDER) { e.joinedAt = p.joinedAt; break; }
            p = p.prev;
        }
        e.prev = p;
        e.next = p === null ? this.head : p.next;
        if (e.next !== null) e.next.prev = e; else this.tail = e;
        if (p !== null) p.next = e; else this.head = e;
        // Bucket list.
        let b = this.buckets[e.rating];
        if (b === undefined) { b = new Bucket(); this.buckets[e.rating] = b; }
        const prov = e.provisional;
        let q = prov ? b.pt : b.et;
        while (q !== null && before(e, q)) q = q.lprev;
        e.lprev = q;
        e.lnext = q === null ? (prov ? b.ph : b.eh) : q.lnext;
        if (e.lnext !== null) e.lnext.lprev = e; else if (prov) b.pt = e; else b.et = e;
        if (q !== null) q.lnext = e; else if (prov) b.ph = e; else b.eh = e;
        if (b.n++ === 0) this.bits[e.rating >>> 5] |= 1 << (e.rating & 31);
        e.queue = this;
        this.size++;
    }

    remove(e) {
        if (e.prev !== null) e.prev.next = e.next; else this.head = e.next;
        if (e.next !== null) e.next.prev = e.prev; else this.tail = e.prev;
        const b = this.buckets[e.rating];
        if (e.lprev !== null) e.lprev.lnext = e.lnext; else if (e.provisional) b.ph = e.lnext; else b.eh = e.lnext;
        if (e.lnext !== null) e.lnext.lprev = e.lprev; else if (e.provisional) b.pt = e.lprev; else b.et = e.lprev;
        if (--b.n === 0) this.bits[e.rating >>> 5] &= ~(1 << (e.rating & 31));
        e.prev = e.next = e.lprev = e.lnext = null;
        e.queue = null;
        this.size--;
    }

    // Lowest non-empty rating in [from, limit], or -1.
    nextSet(from, limit) {
        if (from > limit) return -1;
        const bits = this.bits;
        let w = from >>> 5;
        const last = limit >>> 5;
        let m = bits[w] & (-1 << (from & 31));
        while (m === 0) {
            if (++w > last) return -1;
            m = bits[w];
        }
        const r = (w << 5) + (31 - Math.clz32(m & -m));
        return r <= limit ? r : -1;
    }

    // Highest non-empty rating in [limit, from], or -1.
    prevSet(from, limit) {
        if (from < limit || from < 0) return -1;
        const bits = this.bits;
        let w = from >>> 5;
        const first = limit >>> 5;
        let m = bits[w] & (0xFFFFFFFF >>> (31 - (from & 31)));
        while (m === 0) {
            if (--w < first) return -1;
            m = bits[w];
        }
        const r = (w << 5) + 31 - Math.clz32(m);
        return r >= limit ? r : -1;
    }
}

/**
 * Search window of a player who has waited `waitMs` (DESIGN 5.4).
 * @param {number} waitMs
 * @param {boolean} provisional
 * @param {object} cfg configuration (matchWindow*, matchProvisionalBonus)
 * @returns {number}
 */
export function searchWindow(waitMs, provisional, cfg) {
    let w = cfg.matchWindowStart + (waitMs > 0 ? Math.floor(waitMs / cfg.matchWindowStepMs) * cfg.matchWindowStep : 0);
    if (w > cfg.matchWindowMax) w = cfg.matchWindowMax;
    return provisional ? w + cfg.matchProvisionalBonus : w;
}

/**
 * Matchmaking queues of the primary process.
 */
export class Matchmaker {
    /**
     * @param {object} opts
     * @param {object} opts.config frozen configuration (categories, matchmaking settings)
     * @param {() => number} [opts.now] clock (epoch ms), used when a time is not passed in
     * @param {() => number} [opts.random] RNG in [0, 1) for colour draws
     */
    constructor({ config, now = Date.now, random = Math.random } = {}) {
        if (!config) throw new TypeError('Matchmaker: config required');
        this.config = config;
        this.now = typeof now === 'function' ? now : Date.now;
        this.random = random;
        this.wStart = config.matchWindowStart;
        this.wStep = config.matchWindowStep;
        this.wStepMs = Math.max(1, config.matchWindowStepMs);
        this.wMax = config.matchWindowMax;
        this.bonus = config.matchProvisionalBonus;
        this.repeatLimit = config.matchRepeatLimit;
        this.repeatWindowMs = config.matchRepeatWindowMs;
        this.queues = new Map();         // 'category|r' or 'category|c' -> Queue
        this.byUser = new Map();         // userId -> Entry
        this.balances = new Map();       // userId -> whites minus blacks (non-zero only)
        this.pairCounts = new Map();     // pairKey -> rated pairings inside the repeat window
        this.logKeys = [];               // pairings in time order (for expiry)
        this.logTimes = [];
        this.logHead = 0;
        this.seq = 0;
    }

    /** Players waiting in all queues. */
    get size() { return this.byUser.size; }

    _queue(category, rated, create) {
        const key = rated ? category + '|r' : category + '|c';
        let q = this.queues.get(key);
        if (!q && create) { q = new Queue(category, rated); this.queues.set(key, q); }
        return q;
    }

    _window(e, now) {
        const wait = now - e.joinedAt;
        let w = wait > 0 ? this.wStart + Math.floor(wait / this.wStepMs) * this.wStep : this.wStart;
        if (w > this.wMax) w = this.wMax;
        return e.provisional ? w + this.bonus : w;
    }

    /**
     * Adds a player to the queue of (category, rated).
     * @param {object} req { userId, username, category, rated, rating, provisional, shard, connId,
     *   colorBalance?, joinedAt?, recentOpponents? }
     * @returns {{ok: true} | {error: number}} error: ErrorCode.QueueNotAllowed (already queued),
     *   ErrorCode.InvalidCategory (not an official category)
     */
    join(req) {
        const userId = req && req.userId;
        if (!Number.isSafeInteger(userId) || userId <= 0) throw new TypeError('Matchmaker.join: userId must be a positive integer');
        if (this.byUser.has(userId)) return { error: ErrorCode.QueueNotAllowed };
        const cat = parseCategory(req.category, this.config);
        if (!cat) return { error: ErrorCode.InvalidCategory };
        let rating = Math.round(Number(req.rating));
        if (!Number.isFinite(rating)) rating = this.config.initialRating;
        if (rating < 0) rating = 0; else if (rating > MAX_RATING) rating = MAX_RATING;
        const joinedAt = Number.isFinite(req.joinedAt) ? req.joinedAt : this.now();
        const balance = Number.isFinite(req.colorBalance) ? Math.trunc(req.colorBalance) : (this.balances.get(userId) || 0);
        const rated = !!req.rated;
        const e = new Entry(req, cat.id, rated, rating, joinedAt, ++this.seq, balance, toSet(req.recentOpponents));
        this._queue(cat.id, rated, true).insert(e);
        this.byUser.set(userId, e);
        mQueued.set(this.byUser.size);
        return { ok: true };
    }

    /**
     * Removes a player from its queue.
     * @param {number} userId
     * @returns {boolean} whether the player was queued
     */
    leave(userId) {
        const e = this.byUser.get(userId);
        if (!e) return false;
        e.queue.remove(e);
        this.byUser.delete(userId);
        mQueued.set(this.byUser.size);
        return true;
    }

    /** @param {number} userId @returns {boolean} */
    has(userId) { return this.byUser.has(userId); }

    /**
     * QueueStatus fields of a waiting player, or null when the player is not queued.
     * @param {number} userId
     * @param {number} [now]
     * @returns {{category:string, rated:boolean, state:number, waitMs:number, window:number, queued:number}|null}
     */
    statusOf(userId, now = this.now()) {
        const e = this.byUser.get(userId);
        if (!e) return null;
        const wait = Math.max(0, Math.min(0xFFFFFFFF, Math.floor(now - e.joinedAt)));
        return {
            category: e.category, rated: e.rated, state: QueueState.Searching,
            waitMs: wait, window: Math.min(0xFFFF, this._window(e, now)), queued: e.queue.size,
        };
    }

    /**
     * One pairing round over every queue. The paired players leave their queues.
     * @param {number} [now]
     * @returns {Array<{category:string, rated:boolean, white:object, black:object}>}
     */
    tick(now = this.now()) {
        this._expireRepeats(now);
        const pairs = [];
        for (const q of this.queues.values()) {
            let a = q.head;
            while (a !== null && q.size >= 2) {
                const b = this._search(q, a, now);
                if (b === null) { a = a.next; continue; }
                q.remove(b);                 // b may be a.next: unlink it before reading a.next
                const next = a.next;
                q.remove(a);
                this.byUser.delete(a.userId);
                this.byUser.delete(b.userId);
                pairs.push(this._pair(q, a, b, now));
                a = next;
            }
        }
        if (pairs.length) mQueued.set(this.byUser.size);
        return pairs;
    }

    // Closest valid partner of `a` (ties: longest wait), or null.
    _search(q, a, now) {
        const ra = a.rating;
        const wa = this._window(a, now);
        const lo = ra - wa > 0 ? ra - wa : 0;
        const hi = ra + wa < q.cap ? ra + wa : q.cap - 1;
        let up = q.nextSet(ra, hi);
        let down = ra > lo ? q.prevSet(ra - 1, lo) : -1;
        while (up >= 0 || down >= 0) {
            const du = up >= 0 ? up - ra : Infinity;
            const dd = down >= 0 ? ra - down : Infinity;
            const d = du < dd ? du : dd;
            let best = null;
            if (du === d) {
                best = this._scanBucket(q, q.buckets[up], a, d, now);
                up = up < hi ? q.nextSet(up + 1, hi) : -1;
            }
            if (dd === d) {
                const c = this._scanBucket(q, q.buckets[down], a, d, now);
                if (c !== null && (best === null || before(c, best))) best = c;
                down = down > lo ? q.prevSet(down - 1, lo) : -1;
            }
            if (best !== null) return best;
        }
        return null;
    }

    // Oldest valid partner of `a` in one bucket (rating difference d, already inside a's window).
    _scanBucket(q, bucket, a, d, now) {
        let best = null;
        for (let list = 0; list < 2; list++) {
            let b = list === 0 ? bucket.eh : bucket.ph;
            let scanned = 0;
            while (b !== null && scanned < SCAN_CAP) {
                if (b !== a) {
                    if (!this._excluded(q, a, b)) {
                        // Windows shrink along the list and d is the same for all of it: the first
                        // entry that is not excluded decides for the whole list.
                        if (d <= this._window(b, now) && (best === null || before(b, best))) best = b;
                        break;
                    }
                    scanned++;
                }
                b = b.lnext;
            }
        }
        return best;
    }

    _excluded(q, a, b) {
        if (a.recent !== null && a.recent.has(b.userId)) return true;
        if (b.recent !== null && b.recent.has(a.userId)) return true;
        if (q.rated && this.pairCounts.size !== 0) {
            const c = this.pairCounts.get(pairKey(a.userId, b.userId));
            if (c !== undefined && c >= this.repeatLimit) return true;
        }
        return false;
    }

    _pair(q, a, b, now) {
        let white, black;
        if (a.colorBalance > b.colorBalance) { white = b; black = a; }
        else if (b.colorBalance > a.colorBalance) { white = a; black = b; }
        else if (this.random() < 0.5) { white = a; black = b; }
        else { white = b; black = a; }
        this._setBalance(white.userId, white.colorBalance + 1);
        this._setBalance(black.userId, black.colorBalance - 1);
        if (q.rated) mPairsRated.inc(); else mPairsCasual.inc();
        mWait.observe(now - a.joinedAt);
        mWait.observe(now - b.joinedAt);
        return { category: q.category, rated: q.rated, white: view(white, now), black: view(black, now) };
    }

    _setBalance(userId, v) {
        const b = this.balances;
        b.delete(userId);                // re-inserted last: the Map stays in update order
        if (v === 0) return;
        b.set(userId, v);
        if (b.size > BALANCE_CAP) {
            let drop = b.size - Math.floor(BALANCE_CAP * 0.9);
            for (const k of b.keys()) { if (drop-- <= 0) break; b.delete(k); }
        }
    }

    /**
     * Colour balance (whites minus blacks) the matchmaker keeps for a user.
     * @param {number} userId
     */
    colorBalanceOf(userId) { return this.balances.get(userId) || 0; }

    /**
     * Records the colours of a game created outside the matchmaker (challenge, rematch).
     * @param {number} whiteId
     * @param {number} blackId
     */
    recordColors(whiteId, blackId) {
        this._setBalance(whiteId, this.colorBalanceOf(whiteId) + 1);
        this._setBalance(blackId, this.colorBalanceOf(blackId) - 1);
    }

    /**
     * Counts one rated game between two users for the repeat limit. The primary calls it once the
     * game of a rated pairing of tick() exists; games made outside the matchmaker (challenges,
     * rematches) are not counted.
     * @param {number|{userId:number}} a
     * @param {number|{userId:number}} b
     * @param {number} [now]
     */
    recordPairing(a, b, now = this.now()) {
        const ua = typeof a === 'object' ? a.userId : a;
        const ub = typeof b === 'object' ? b.userId : b;
        const key = pairKey(ua, ub);
        this.pairCounts.set(key, (this.pairCounts.get(key) || 0) + 1);
        this.logKeys.push(key);
        this.logTimes.push(now);
    }

    /**
     * Rated pairings of two users inside the repeat window.
     * @param {number} a
     * @param {number} b
     * @param {number} [now]
     */
    repeatCount(a, b, now = this.now()) {
        this._expireRepeats(now);
        return this.pairCounts.get(pairKey(a, b)) || 0;
    }

    _expireRepeats(now) {
        const cutoff = now - this.repeatWindowMs;
        const keys = this.logKeys, times = this.logTimes;
        let h = this.logHead;
        while (h < times.length && times[h] <= cutoff) {
            const k = keys[h];
            const c = this.pairCounts.get(k);
            if (c === undefined || c <= 1) this.pairCounts.delete(k); else this.pairCounts.set(k, c - 1);
            keys[h] = 0;
            h++;
        }
        if (h === times.length) { keys.length = 0; times.length = 0; h = 0; }
        else if (h > 4096 && h * 2 > times.length) { keys.splice(0, h); times.splice(0, h); h = 0; }
        this.logHead = h;
    }

    /**
     * Queue sizes, for the tests (no metric or admin command reads it).
     * @returns {{queued:number, queues:Array<{category:string, rated:boolean, size:number}>}}
     */
    stats() {
        const queues = [];
        for (const q of this.queues.values()) queues.push({ category: q.category, rated: q.rated, size: q.size });
        return { queued: this.byUser.size, queues };
    }
}

function view(e, now) {
    return {
        userId: e.userId, username: e.username, category: e.category, rated: e.rated,
        rating: e.rating, provisional: e.provisional, shard: e.shard, connId: e.connId,
        colorBalance: e.colorBalance, joinedAt: e.joinedAt, waitMs: Math.max(0, now - e.joinedAt),
    };
}
