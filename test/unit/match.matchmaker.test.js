import { test } from 'node:test';
import assert from 'node:assert/strict';
import { testConfig } from '../../src/config.js';
import { enums } from '../../src/protocol/schema.js';
import { Matchmaker, searchWindow } from '../../src/match/matchmaker.js';

const { ErrorCode, QueueState } = enums;
const cfg = testConfig();   // window 100 +50 every 5 s up to 500, provisional +150, repeat 3 per hour

function mm(opts = {}) {
    let t = 0;
    const m = new Matchmaker({ config: opts.config || cfg, now: () => t, random: opts.random || (() => 0.25) });
    m.at = (v) => { t = v; return m; };
    return m;
}

let nextId = 1;
function player(rating, extra = {}) {
    const userId = extra.userId ?? nextId++;
    return { userId, username: `p${userId}`, category: '3+2', rated: true, rating, provisional: false, shard: 0, connId: userId, joinedAt: 0, ...extra };
}

function ids(pair) {
    return [pair.white.userId, pair.black.userId].sort((a, b) => a - b);
}

test('matchmaker: join validation, leave, has', () => {
    const m = mm();
    assert.deepEqual(m.join(player(1500, { userId: 1 })), { ok: true });
    assert.equal(m.has(1), true);
    assert.deepEqual(m.join(player(1500, { userId: 1 })), { error: ErrorCode.QueueNotAllowed });
    assert.deepEqual(m.join(player(1500, { userId: 1, category: '5+0' })), { error: ErrorCode.QueueNotAllowed });
    assert.deepEqual(m.join(player(1500, { userId: 2, category: '4+0' })), { error: ErrorCode.InvalidCategory });
    assert.deepEqual(m.join(player(1500, { userId: 2, category: 'custom' })), { error: ErrorCode.InvalidCategory });
    assert.deepEqual(m.join(player(1500, { userId: 2, category: undefined })), { error: ErrorCode.InvalidCategory });
    assert.throws(() => m.join({ category: '3+2', rating: 1500 }), TypeError);
    assert.equal(m.leave(1), true);
    assert.equal(m.leave(1), false);
    assert.equal(m.has(1), false);
    assert.deepEqual(m.join(player(1500, { userId: 1, category: '5+0' })), { ok: true });
    assert.equal(m.size, 1);
});

test('matchmaker: window growth, cap and provisional bonus', () => {
    assert.equal(searchWindow(0, false, cfg), 100);
    assert.equal(searchWindow(4999, false, cfg), 100);
    assert.equal(searchWindow(5000, false, cfg), 150);
    assert.equal(searchWindow(20000, false, cfg), 300);
    assert.equal(searchWindow(40000, false, cfg), 500);
    assert.equal(searchWindow(600000, false, cfg), 500);
    assert.equal(searchWindow(0, true, cfg), 250);
    assert.equal(searchWindow(600000, true, cfg), 650);

    const m = mm();
    m.join(player(1500, { userId: 10, joinedAt: 1000 }));
    m.join(player(1500, { userId: 11, joinedAt: 1000, provisional: true, category: '5+0' }));
    assert.deepEqual(m.statusOf(10, 1000), { category: '3+2', rated: true, state: QueueState.Searching, waitMs: 0, window: 100, queued: 1 });
    assert.equal(m.statusOf(10, 16000).window, 250);
    assert.equal(m.statusOf(10, 16000).waitMs, 15000);
    assert.equal(m.statusOf(10, 1000000).window, 500);
    assert.equal(m.statusOf(11, 1000).window, 250);
    assert.equal(m.statusOf(11, 1000000).window, 650);
    assert.equal(m.statusOf(12, 1000), null);
});

test('matchmaker: a pair needs each player inside the other\'s window', () => {
    const m = mm();
    m.join(player(1500, { userId: 1, joinedAt: 0 }));        // waits: window 500 after 40 s
    m.join(player(1800, { userId: 2, joinedAt: 60000 }));    // new: window 100
    assert.deepEqual(m.tick(60000), []);
    assert.equal(m.statusOf(1, 60000).window, 500);
    assert.deepEqual(m.tick(79999), []);                     // player 2: window 250 < 300
    const pairs = m.tick(80000);                              // player 2: window 300
    assert.equal(pairs.length, 1);
    assert.deepEqual(ids(pairs[0]), [1, 2]);
    assert.equal(m.size, 0);
    assert.equal(pairs[0].category, '3+2');
    assert.equal(pairs[0].rated, true);
});

test('matchmaker: the provisional bonus widens both directions of the rule', () => {
    const m = mm();
    m.join(player(1500, { userId: 1, provisional: true }));  // window 250
    m.join(player(1700, { userId: 2 }));                      // window 100
    assert.deepEqual(m.tick(0), []);
    m.join(player(1700, { userId: 3, provisional: true }));  // window 250: 200 inside both
    const pairs = m.tick(0);
    assert.deepEqual(pairs.map(ids), [[1, 3]]);
});

test('matchmaker: closest rating inside the window, ties to the longer wait', () => {
    const m = mm();
    m.join(player(1500, { userId: 1, joinedAt: 0 }));
    m.join(player(1580, { userId: 2, joinedAt: 10 }));
    m.join(player(1520, { userId: 3, joinedAt: 20 }));
    m.join(player(1450, { userId: 4, joinedAt: 30 }));
    let pairs = m.tick(100);
    assert.deepEqual(pairs.map(ids), [[1, 3]]);              // 2 and 4 are 130 apart
    assert.deepEqual([m.has(2), m.has(4)], [true, true]);

    const n = mm();
    n.join(player(1500, { userId: 1, joinedAt: 0 }));        // seeker (oldest)
    n.join(player(1480, { userId: 2, joinedAt: 10 }));       // 20 below, older
    n.join(player(1520, { userId: 3, joinedAt: 20 }));       // 20 above, newer
    pairs = n.tick(100);
    assert.deepEqual(pairs.map(ids), [[1, 2]]);

    const o = mm();
    o.join(player(1500, { userId: 1, joinedAt: 0 }));
    o.join(player(1520, { userId: 3, joinedAt: 10 }));       // above, older this time
    o.join(player(1480, { userId: 2, joinedAt: 20 }));
    pairs = o.tick(100);
    assert.deepEqual(pairs.map(ids), [[1, 3]]);
});

test('matchmaker: FIFO fairness: the longest-waiting player chooses first', () => {
    const m = mm();
    m.join(player(1500, { userId: 1, joinedAt: 0 }));
    m.join(player(1620, { userId: 2, joinedAt: 1000 }));     // 120 from player 1: never with 1 early
    m.join(player(1570, { userId: 3, joinedAt: 2000 }));     // 70 from 1, 50 from 2
    const pairs = m.tick(2000);
    assert.deepEqual(pairs.map(ids), [[1, 3]]);
    assert.equal(m.has(2), true);

    // Many players at one rating: pairs come out in joining order.
    const n = mm();
    for (let i = 0; i < 10; i++) n.join(player(1500, { userId: 100 + i, joinedAt: i }));
    assert.deepEqual(n.tick(100).map(ids), [[100, 101], [102, 103], [104, 105], [106, 107], [108, 109]]);
});

test('matchmaker: an out-of-order joinedAt is queued by its time', () => {
    const m = mm();
    m.join(player(1500, { userId: 1, joinedAt: 5000 }));
    m.join(player(1540, { userId: 2, joinedAt: 1000 }));     // waited longer: seeks first
    m.join(player(1530, { userId: 3, joinedAt: 6000 }));
    const pairs = m.tick(6000);
    assert.deepEqual(pairs.map(ids), [[2, 3]]);
});

test('matchmaker: queues are separate per category and rated flag', () => {
    const m = mm();
    m.join(player(1500, { userId: 1, rated: true }));
    m.join(player(1500, { userId: 2, rated: false }));
    m.join(player(1500, { userId: 3, category: '5+0' }));
    assert.deepEqual(m.tick(0), []);
    assert.equal(m.statusOf(1, 0).queued, 1);
    m.join(player(1510, { userId: 4, rated: false }));
    const pairs = m.tick(0);
    assert.deepEqual(pairs.map(ids), [[2, 4]]);
    assert.equal(pairs[0].rated, false);
    assert.deepEqual(m.stats().queued, 2);
});

test('matchmaker: leave removes the player from its rating bucket', () => {
    const m = mm();
    m.join(player(1500, { userId: 1 }));
    m.join(player(1500, { userId: 2 }));
    m.join(player(1500, { userId: 3 }));
    m.leave(2);
    m.leave(1);
    assert.deepEqual(m.tick(0), []);
    m.join(player(1500, { userId: 2 }));
    assert.deepEqual(m.tick(0).map(ids), [[2, 3]]);
    assert.equal(m.size, 0);
    // Rejoin after a pairing.
    assert.deepEqual(m.join(player(1500, { userId: 2 })), { ok: true });
});

test('matchmaker: repeat limit for rated pairings, with expiry', () => {
    const m = mm();
    const H = cfg.matchRepeatWindowMs;
    for (let i = 0; i < cfg.matchRepeatLimit; i++) {
        m.join(player(1500, { userId: 1, joinedAt: i * 1000 }));
        m.join(player(1500, { userId: 2, joinedAt: i * 1000 }));
        const pairs = m.tick(i * 1000);
        assert.equal(pairs.length, 1);
        // A pairing counts once its game exists: the primary records it then.
        assert.equal(m.repeatCount(1, 2, i * 1000), i);
        m.recordPairing(pairs[0].white, pairs[0].black, i * 1000);
    }
    assert.equal(m.repeatCount(1, 2, 3000), 3);
    m.join(player(1500, { userId: 1, joinedAt: 3000 }));
    m.join(player(1500, { userId: 2, joinedAt: 3000 }));
    assert.deepEqual(m.tick(3000), []);
    // A third player can take either of them.
    m.join(player(1600, { userId: 3, joinedAt: 3000 }));
    assert.deepEqual(m.tick(3000).map(ids), [[1, 3]]);
    m.leave(2);
    // Casual games are not limited.
    m.join(player(1500, { userId: 1, rated: false, joinedAt: 3000 }));
    m.join(player(1500, { userId: 2, rated: false, joinedAt: 3000 }));
    assert.equal(m.tick(3000).length, 1);
    // The first pairing leaves the window after an hour.
    m.join(player(1500, { userId: 1, joinedAt: H }));
    m.join(player(1500, { userId: 2, joinedAt: H }));
    assert.deepEqual(m.tick(H - 1), []);
    assert.equal(m.tick(H).length, 1);

    // recordPairing() takes user ids too.
    const n = mm();
    for (let i = 0; i < 3; i++) n.recordPairing({ userId: 7 }, 8, 0);
    n.join(player(1500, { userId: 7 }));
    n.join(player(1500, { userId: 8 }));
    assert.deepEqual(n.tick(0), []);
});

test('matchmaker: recentOpponents exclusions apply both ways', () => {
    const m = mm();
    m.join(player(1500, { userId: 1, recentOpponents: [2] }));
    m.join(player(1500, { userId: 2 }));
    assert.deepEqual(m.tick(0), []);
    m.join(player(1500, { userId: 3, recentOpponents: new Set([1]) }));
    assert.deepEqual(m.tick(0).map(ids), [[2, 3]]);
});

test('matchmaker: colour balance decides colours, ties are drawn', () => {
    const m = mm({ random: () => 0.9 });
    m.join(player(1500, { userId: 1, colorBalance: 2 }));
    m.join(player(1500, { userId: 2, colorBalance: 0 }));
    let [p] = m.tick(0);
    assert.equal(p.black.userId, 1);
    assert.equal(p.white.userId, 2);
    m.join(player(1500, { userId: 3, colorBalance: -1 }));
    m.join(player(1500, { userId: 4, colorBalance: -3 }));
    [p] = m.tick(0);
    assert.equal(p.white.userId, 4);

    // Ties: random() < 0.5 gives White to the seeker (the longer wait).
    const low = mm({ random: () => 0.1 });
    low.join(player(1500, { userId: 1, joinedAt: 0 }));
    low.join(player(1500, { userId: 2, joinedAt: 1 }));
    [p] = low.tick(10);
    assert.equal(p.white.userId, 1);
    const high = mm({ random: () => 0.7 });
    high.join(player(1500, { userId: 1, joinedAt: 0 }));
    high.join(player(1500, { userId: 2, joinedAt: 1 }));
    [p] = high.tick(10);
    assert.equal(p.white.userId, 2);

    // The matchmaker remembers the balance when the caller does not pass one.
    assert.equal(high.colorBalanceOf(2), 1);
    assert.equal(high.colorBalanceOf(1), -1);
    high.join(player(1500, { userId: 1, joinedAt: 20 }));
    high.join(player(1500, { userId: 2, joinedAt: 20 }));
    [p] = high.tick(20);
    assert.equal(p.white.userId, 1);
    assert.equal(high.colorBalanceOf(1), 0);
    high.recordColors(5, 6);
    assert.deepEqual([high.colorBalanceOf(5), high.colorBalanceOf(6)], [1, -1]);
});

test('matchmaker: pair entries carry what the primary needs', () => {
    const m = mm();
    m.join({ userId: 1, username: 'alice', category: '3+2', rated: true, rating: 1500, provisional: true, shard: 2, connId: 77, joinedAt: 0 });
    m.join({ userId: 2, username: 'bob', category: '3+2', rated: true, rating: 1510, provisional: false, shard: 3, connId: 88, joinedAt: 500 });
    const [p] = m.tick(1000);
    const a = p.white.userId === 1 ? p.white : p.black;
    assert.deepEqual(a, { userId: 1, username: 'alice', category: '3+2', rated: true, rating: 1500, provisional: true,
        shard: 2, connId: 77, colorBalance: 0, joinedAt: 0, waitMs: 1000 });
});

test('matchmaker: ratings far apart and extreme values', () => {
    const m = mm();
    m.join(player(0, { userId: 1 }));
    m.join(player(60, { userId: 2 }));
    m.join(player(65535, { userId: 3 }));
    m.join(player(70000, { userId: 4 }));                    // clamped to the u16 range
    m.join(player(9000, { userId: 5 }));                     // grows the bucket index
    const pairs = m.tick(0);
    assert.deepEqual(pairs.map(ids).sort(), [[1, 2], [3, 4]]);
    assert.equal(m.has(5), true);
});

// ---- reference implementation (brute force) -------------------------------------------------

class Reference {
    constructor(config) {
        this.cfg = config; this.queues = new Map(); this.counts = new Map(); this.log = []; this.seq = 0;
        this.pairs = 0; this.repeatSkips = 0; this.recentSkips = 0; this.mutualSkips = 0;
    }
    join(e) {
        const key = e.category + (e.rated ? '|r' : '|c');
        if (!this.queues.has(key)) this.queues.set(key, []);
        this.queues.get(key).push({ ...e, seq: ++this.seq, recent: new Set(e.recentOpponents || []) });
    }
    leave(userId) {
        for (const q of this.queues.values()) { const i = q.findIndex((x) => x.userId === userId); if (i >= 0) q.splice(i, 1); }
    }
    key(a, b) { return a < b ? `${a}:${b}` : `${b}:${a}`; }
    tick(now) {
        this.log = this.log.filter(([k, t]) => {
            if (t <= now - this.cfg.matchRepeatWindowMs) { this.counts.set(k, this.counts.get(k) - 1); return false; }
            return true;
        });
        const out = new Map();
        for (const [key, q] of this.queues) {
            q.sort((x, y) => x.joinedAt - y.joinedAt || x.seq - y.seq);
            const paired = new Set();
            const res = [];
            for (const a of q) {
                if (paired.has(a)) continue;
                const wa = searchWindow(now - a.joinedAt, a.provisional, this.cfg);
                let best = null, bestD = Infinity;
                for (const b of q) {
                    if (b === a || paired.has(b)) continue;
                    const d = Math.abs(a.rating - b.rating);
                    if (d > wa) continue;
                    if (d > searchWindow(now - b.joinedAt, b.provisional, this.cfg)) { this.mutualSkips++; continue; }
                    if (a.recent.has(b.userId) || b.recent.has(a.userId)) { this.recentSkips++; continue; }
                    if (a.rated && (this.counts.get(this.key(a.userId, b.userId)) || 0) >= this.cfg.matchRepeatLimit) { this.repeatSkips++; continue; }
                    if (d < bestD || (d === bestD && (b.joinedAt < best.joinedAt || (b.joinedAt === best.joinedAt && b.seq < best.seq)))) { best = b; bestD = d; }
                }
                if (best) {
                    paired.add(a); paired.add(best);
                    this.pairs++;
                    res.push([a.userId, best.userId].sort((x, y) => x - y));
                    if (a.rated) {
                        const k = this.key(a.userId, best.userId);
                        this.counts.set(k, (this.counts.get(k) || 0) + 1);
                        this.log.push([k, now]);
                    }
                }
            }
            this.queues.set(key, q.filter((x) => !paired.has(x)));
            if (res.length) out.set(key, res);
        }
        return out;
    }
}

function lcg(seed) {
    let s = seed >>> 0;
    return () => { s = (Math.imul(s, 1664525) + 1013904223) >>> 0; return s / 4294967296; };
}

test('matchmaker: same pairs as a brute-force reference over a random simulation', () => {
    const totals = { pairs: 0, repeatSkips: 0, recentSkips: 0, mutualSkips: 0 };
    for (const seed of [1, 2, 3, 4, 5, 6]) {
        const rnd = lcg(seed);
        const m = mm();
        const ref = new Reference(cfg);
        const queued = new Set();
        // Small pools with stable ratings meet the same opponents again (repeat limit, exclusions);
        // larger pools with random ratings stress the window rules.
        const pool = seed % 2 ? 30 : 150;
        const baseRating = (u) => 1300 + ((u * 37) % 400);
        let uid = 1;
        for (let step = 0; step < 400; step++) {
            const now = step * 250;
            const joins = Math.floor(rnd() * 6);
            for (let j = 0; j < joins; j++) {
                const userId = 1 + Math.floor(rnd() * pool);
                if (queued.has(userId)) continue;
                const rating = pool < 100 ? baseRating(userId) + Math.floor(rnd() * 20) : 1200 + Math.floor(rnd() * 900);
                const e = {
                    userId, username: 'u' + userId, category: rnd() < 0.7 ? '3+2' : '5+0', rated: rnd() < 0.8,
                    rating, provisional: rnd() < 0.3, shard: 0, connId: uid++,
                    joinedAt: now - Math.floor(rnd() * 3000),
                    recentOpponents: rnd() < 0.3 ? [userId + 1 + Math.floor(rnd() * 3)] : undefined,
                };
                assert.deepEqual(m.join(e), { ok: true });
                ref.join(e);
                queued.add(userId);
            }
            if (rnd() < 0.2 && queued.size) {
                const victim = [...queued][Math.floor(rnd() * queued.size)];
                assert.equal(m.leave(victim), true);
                ref.leave(victim);
                queued.delete(victim);
            }
            const got = new Map();
            for (const p of m.tick(now)) {
                if (p.rated) m.recordPairing(p.white, p.black, now);     // the primary, once the game exists
                const key = p.category + (p.rated ? '|r' : '|c');
                if (!got.has(key)) got.set(key, []);
                got.get(key).push(ids(p));
                queued.delete(p.white.userId);
                queued.delete(p.black.userId);
            }
            const want = ref.tick(now);
            assert.deepEqual(new Map([...got].sort()), new Map([...want].sort()), `seed ${seed} step ${step}`);
        }
        for (const k of Object.keys(totals)) totals[k] += ref[k];
    }
    // The simulation exercised every rule.
    assert.ok(totals.pairs > 1000, JSON.stringify(totals));
    assert.ok(totals.repeatSkips > 0 && totals.recentSkips > 0 && totals.mutualSkips > 0, JSON.stringify(totals));
});

test('matchmaker: 100k waiting players, one tick stays fast and every pair is valid', () => {
    const rnd = lcg(42);
    const m = mm({ random: rnd });
    const cats = cfg.categories.map((c) => c.id);
    const N = 100000;
    for (let i = 1; i <= N; i++) {
        // Roughly normal ratings around 1500.
        const r = Math.round(1500 + (rnd() + rnd() + rnd() + rnd() - 2) * 500);
        m.join({ userId: i, username: 'u' + i, category: cats[i % cats.length], rated: (i >> 4) % 2 === 0,
            rating: r, provisional: rnd() < 0.2, shard: i % 8, connId: i, joinedAt: Math.floor(i / 100) });
    }
    assert.equal(m.size, N);
    const t0 = performance.now();
    const pairs = m.tick(2000);
    const ms = performance.now() - t0;
    const seen = new Set();
    for (const p of pairs) {
        const d = Math.abs(p.white.rating - p.black.rating);
        assert.ok(d <= searchWindow(p.white.waitMs, p.white.provisional, cfg));
        assert.ok(d <= searchWindow(p.black.waitMs, p.black.provisional, cfg));
        assert.equal(p.white.category, p.black.category);
        assert.ok(!seen.has(p.white.userId) && !seen.has(p.black.userId));
        seen.add(p.white.userId); seen.add(p.black.userId);
    }
    assert.equal(m.size + seen.size, N);
    assert.ok(pairs.length > N / 2 - 200, `${pairs.length} pairs`);
    // A generous bound (a few tens of ms are expected): catches accidental O(n^2) behaviour.
    assert.ok(ms < 1500, `tick took ${ms.toFixed(1)} ms`);
    // A second tick over the few leftovers is immediate.
    const t1 = performance.now();
    m.tick(2250);
    assert.ok(performance.now() - t1 < 100);
});
