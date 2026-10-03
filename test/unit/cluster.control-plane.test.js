import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { describe, it } from 'node:test';
import { AbuseTracker } from '../../src/cluster/abuse.js';
import { ControlPlane, PAIR_RETRY_DELAY_MS } from '../../src/cluster/control-plane.js';
import { Ipc, IpcTimeoutError, channelPair } from '../../src/cluster/ipc.js';
import { OnceStore, SlidingWindowLimiter } from '../../src/cluster/limits.js';
import { Presence } from '../../src/cluster/presence.js';
import { startPrimary } from '../../src/cluster/primary.js';
import { stopPrimary } from '../../src/cluster/primary-main.js';
import { describe as describeConfig, loadConfig, primaryConfig, testConfig } from '../../src/config.js';
import { Challenges } from '../../src/match/challenges.js';
import { Matchmaker } from '../../src/match/matchmaker.js';
import { Registry } from '../../src/metrics.js';
import { TicketKeys } from '../../src/net/ticket-keys.js';
import { CloseCode, decode, enums, messageName } from '../../src/protocol/index.js';
import { GameIdAllocator, shardOfGameId } from '../../src/util/ids.js';

const E = enums.ErrorCode, N = enums.NoticeCode, QS = enums.QueueState, CS = enums.ChallengeState;
const cfg = testConfig();
const tick = () => new Promise((r) => setImmediate(r));

class FakeMatchmaker {
    constructor() { this.q = new Map(); this.pairs = []; }
    join(e) { if (this.q.has(e.userId)) return { error: E.QueueNotAllowed }; this.q.set(e.userId, e); return { ok: true }; }
    leave(u) { return this.q.delete(u); }
    has(u) { return this.q.has(u); }
    statusOf(u, now) { const e = this.q.get(u); return e && { category: e.category, rated: e.rated, state: QS.Searching, waitMs: now - e.joinedAt, window: 50, queued: this.q.size }; }
    tick() { const p = this.pairs; this.pairs = []; return p; }
}

class FakeShards {
    constructor(live = [0, 1]) {
        this.live = live;
        this.sent = [];
        this.requests = [];
        this.alloc = new Map();
        this.create = null;                                     // override: (shard, payload) => reply
    }
    notify(shard, type, payload) { this.sent.push({ shard, type, payload }); }
    broadcast(type, payload) { this.sent.push({ shard: '*', type, payload }); }
    list() { return this.live; }
    async request(shard, type, payload, opts) {
        this.requests.push({ shard, type, payload });
        if (type !== 'game.create') return null;
        if (this.create) return this.create(shard, payload, opts);
        if (!this.alloc.has(shard)) this.alloc.set(shard, new GameIdAllocator(shard));
        return { ok: true, gameId: this.alloc.get(shard).next() };
    }
    /** Messages written to users: [{ shard, connId, name, msg }]. */
    frames() {
        const out = [];
        for (const s of this.sent) {
            if (s.type !== 'conn.send' && s.type !== 'conn.kick') continue;
            for (const f of s.payload.frames || []) { const m = decode(f); out.push({ shard: s.shard, connId: s.payload.connId, kind: s.type, name: messageName(m.type), msg: m }); }
        }
        return out;
    }
    of(type) { return this.sent.filter((s) => s.type === type); }
    clear() { this.sent.length = 0; this.requests.length = 0; }
}

function setup({ config = cfg, activeBan = null, conduct = null, ratingOf = null, acceptsChallenges = null, live, abuse = false, realMatchmaker = false } = {}) {
    let t = Date.UTC(2026, 5, 1, 12);
    const clock = { now: () => t, advance: (ms) => { t += ms; } };
    const shards = new FakeShards(live);
    const mm = realMatchmaker ? new Matchmaker({ config, now: clock.now, random: () => 0.1 }) : new FakeMatchmaker();
    const ch = new Challenges({ config, now: clock.now, randomInt: () => 0 });
    const presence = new Presence({ maxConnections: config.maxConnections, maxPerIp: config.maxConnectionsPerIp });
    const cp = new ControlPlane({
        config, presence, matchmaker: mm, challenges: ch, conduct, limiter: new SlidingWindowLimiter({ now: clock.now }),
        once: new OnceStore({ now: clock.now }), shards, activeBan, ratingOf, acceptsChallenges, now: clock.now,
        random: () => 0.1, registry: new Registry(),
        // abuse: a tracker on the test clock that broadcasts like the default one.
        abuse: abuse ? new AbuseTracker({ config, now: clock.now, registry: new Registry(), broadcast: (blocks) => shards.broadcast('abuse.block', { blocks }) }) : null,
    });
    const online = (userId, username, shard, connId = userId * 10) => {
        const r = cp.presenceClaim({ userId, username, shard, connId, ip: `10.0.0.${userId}` }, shard);
        assert.equal(r.ok, true);
        return { userId, username, shard, connId };
    };
    return { cp, shards, mm, ch, presence, clock, online };
}

describe('control plane: presence', () => {
    it('replaces an older connection of the same user (kick 4007) and releases only the live one', () => {
        const { cp, shards, mm, online } = setup();
        online(1, 'alice', 0, 10);
        mm.q.set(1, { userId: 1 });
        cp.queued.set(1, { category: '5+0', rated: true });
        const r = cp.presenceClaim({ userId: 1, username: 'alice', shard: 1, connId: 20 }, 1);
        assert.deepEqual(r, { ok: true, activeGame: 0, kicked: true });
        const [kick] = shards.of('conn.kick');
        assert.equal(kick.shard, 0);
        assert.equal(kick.payload.connId, 10);
        assert.equal(kick.payload.closeCode, 4007);
        assert.deepEqual(shards.frames().map((f) => f.name), ['Error', 'Notice']);
        assert.equal(shards.frames()[1].msg.code, N.ReplacedByNewConnection);
        assert.equal(mm.q.has(1), false);                       // the replaced connection's search ends
        assert.deepEqual(cp.presenceRelease({ userId: 1, connId: 10 }, 0), { ok: true, current: false });
        assert.equal(cp.presence.get(1).connId, 20);
        assert.deepEqual(cp.presenceRelease({ userId: 1, connId: 20 }, 1), { ok: true, current: true });
        assert.equal(cp.presence.get(1), undefined);
    });

    it('refuses banned users and a full server', () => {
        const { cp, clock } = setup({ activeBan: (u, now) => (u === 7 ? { endsAt: now + 60000 } : null), config: { ...cfg, maxConnections: 1 } });
        const r = cp.presenceClaim({ userId: 7, username: 'bad', shard: 0, connId: 1 }, 0);
        assert.equal(r.error, E.Banned);
        assert.equal(r.until, clock.now() + 60000);
        assert.equal(cp.presenceClaim({ userId: 1, username: 'a', shard: 0, connId: 2 }, 0).ok, true);
        assert.deepEqual(cp.presenceClaim({ userId: 2, username: 'b', shard: 0, connId: 3 }, 0), { error: E.ServerFull });
        assert.equal(cp.presenceClaim({ userId: 1, username: 'a', shard: 1, connId: 4 }, 1).ok, true);   // a reconnect is not a new user
    });

    it('admits a player whose game is in progress on a full server; newcomers in the reserve get ServerFull', () => {
        const max = 3;
        const { cp, presence } = setup({ config: { ...cfg, maxConnections: max, maxConnectionsPerIp: 100 } });
        const gameId = new GameIdAllocator(1).next();
        // A full server: every slot taken by a player, and one of the players of gameId just lost
        // the connection (the socket closed: its count and its presence are released).
        for (let u = 1; u <= max; u++) {
            assert.equal(presence.ipAcquire(`10.0.0.${u}`, 0).ok, true);
            assert.equal(cp.presenceClaim({ userId: u, username: `u${u}`, shard: 0, connId: u }, 0).ok, true);
        }
        cp.gameActive({ gameId, whiteId: 3, blackId: 9 });
        presence.ipRelease('10.0.0.3', 0);
        cp.presenceRelease({ userId: 3, connId: 3 }, 0);
        // A newcomer takes the freed slot first.
        assert.equal(presence.ipAcquire('10.0.0.4', 0).ok, true);
        assert.equal(cp.presenceClaim({ userId: 4, username: 'u4', shard: 0, connId: 4 }, 0).ok, true);
        // The upgrade of the returning player passes on the reserve, and Hello admits them with their game.
        assert.equal(presence.ipAcquire('10.0.0.3', 1).ok, true);
        assert.deepEqual(cp.presenceClaim({ userId: 3, username: 'u3', shard: 1, connId: 30 }, 1), { ok: true, activeGame: gameId, kicked: false });
        assert.equal(presence.size, max + 1);
        // So does their opponent, never seen before on this server.
        assert.equal(presence.ipAcquire('10.0.0.9', 1).ok, true);
        assert.equal(cp.presenceClaim({ userId: 9, username: 'u9', shard: 1, connId: 90 }, 1).activeGame, gameId);
        // Another newcomer passes the upgrade on the reserve too, but not Hello.
        assert.equal(presence.ipAcquire('10.0.0.5', 0).ok, true);
        assert.deepEqual(cp.presenceClaim({ userId: 5, username: 'u5', shard: 0, connId: 5 }, 0), { error: E.ServerFull });
        // The reserve has an end: max(16, 2 %) upgrades beyond MAX_CONNECTIONS.
        while (presence.connections < max + 16) assert.equal(presence.ipAcquire('10.0.2.1', 0).ok, true);
        assert.deepEqual(presence.ipAcquire('10.0.0.3', 0), { ok: false, reason: 'global' });
    });

    it('serves requests over a bound IPC channel with the caller shard', async () => {
        const { cp } = setup();
        const [a, b] = channelPair();
        cp.bind(3, new Ipc(a));
        const shardSide = new Ipc(b);
        assert.deepEqual(await shardSide.request('presence.claim', { userId: 5, username: 'eve', connId: 9 }), { ok: true, activeGame: 0, kicked: false });
        assert.equal(cp.presence.get(5).shard, 3);
        assert.equal((await shardSide.request('ratelimit.take', { key: 'login:1.2.3.4', limit: 1, windowMs: 1000 })).allowed, true);
        assert.equal((await shardSide.request('ratelimit.take', { key: 'login:1.2.3.4', limit: 1, windowMs: 1000 })).allowed, false);
        assert.deepEqual(await shardSide.request('ratelimit.refund', { key: 'login:1.2.3.4', windowMs: 1000, cost: 1, ageMs: 0 }), { refunded: true });
        assert.equal((await shardSide.request('ratelimit.take', { key: 'login:1.2.3.4', limit: 1, windowMs: 1000 })).allowed, true);
        assert.equal((await shardSide.request('once.consume', { key: 'totp:5:123456', ttlMs: 90000 })).fresh, true);
        assert.equal((await shardSide.request('once.consume', { key: 'totp:5:123456', ttlMs: 90000 })).fresh, false);
        assert.deepEqual(await shardSide.request('conn.ipAcquire', { ip: '192.0.2.9' }), { ok: true });
        assert.deepEqual(await shardSide.request('conn.ipRelease', { ip: '192.0.2.9' }), { ok: true });
    });
});

describe('control plane: matchmaking', () => {
    it('joins the queue (QueueStatus Searching) and refuses stale, busy, unknown and cooling-down players', async () => {
        const until = Date.UTC(2026, 5, 1, 13);
        const conduct = { cooldownUntil: (u) => (u === 3 ? until : 0), record() {} };
        const { cp, shards, online } = setup({ conduct });
        const a = online(1, 'alice', 0);
        online(3, 'carol', 1);
        assert.deepEqual(cp.mmJoin({ ...a, category: '5+0', rated: true, rating: 1500 }, 0), { ok: true });
        await tick();
        const qs = shards.frames().find((f) => f.name === 'QueueStatus');
        assert.equal(qs.connId, a.connId);
        assert.equal(qs.msg.state, QS.Searching);
        assert.equal(qs.msg.category, '5+0');
        assert.deepEqual(cp.mmJoin({ ...a, connId: 999, category: '5+0', rated: true }, 0), { error: E.QueueNotAllowed });
        assert.deepEqual(cp.mmJoin({ ...a, category: '7+7', rated: true }, 0), { error: E.InvalidCategory });
        assert.deepEqual(cp.mmJoin({ userId: 3, username: 'carol', shard: 1, connId: 30, category: '5+0', rated: true }, 1), { error: E.MatchmakingCooldown });
        assert.ok(shards.frames().some((f) => f.name === 'Notice' && f.msg.code === N.MatchmakingCooldown && f.msg.arg === until));
        assert.deepEqual(cp.mmJoin({ userId: 3, username: 'carol', shard: 1, connId: 30, category: '5+0', rated: false }, 1), { ok: true });
        cp.gameActive({ gameId: new GameIdAllocator(0).next(), whiteId: 1, blackId: 9 });
        assert.deepEqual(cp.mmJoin({ ...a, category: '5+0', rated: true }, 0), { error: E.AlreadyInGame });
        shards.clear();
        assert.deepEqual(cp.mmLeave({ userId: 3 }), { ok: true });
        assert.equal(shards.frames()[0].msg.state, QS.Left);
    });

    it('pairs: Matched to both, the older searcher hosts, both connections attached', async () => {
        const { cp, shards, mm, clock, online } = setup();
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1);
        cp.mmJoin({ ...a, category: '5+0', rated: true, rating: 1510 }, 0);
        clock.advance(4000);
        cp.mmJoin({ ...b, category: '5+0', rated: true, rating: 1490 }, 1);
        await tick();
        shards.clear();
        mm.pairs.push({ category: '5+0', rated: true, white: { ...mm.q.get(2) }, black: { ...mm.q.get(1) } });
        mm.q.clear();
        cp.matchTick();
        await tick();
        const matched = shards.frames().filter((f) => f.name === 'QueueStatus');
        assert.deepEqual(matched.map((f) => [f.connId, f.msg.state]), [[20, QS.Matched], [10, QS.Matched]]);
        assert.equal(matched[1].msg.waitMs, 4000);
        const [create] = shards.requests;
        assert.equal(create.shard, 0);                                  // alice waited longer
        const spec = create.payload.spec;
        assert.deepEqual([spec.white.userId, spec.black.userId, spec.baseMs, spec.incMs, spec.rated], [2, 1, 300000, 0, true]);
        assert.deepEqual(spec.white, { userId: 2, name: 'bob', rating: 1490, provisional: false });
        const attaches = shards.of('game.attach');
        assert.deepEqual(attaches.map((x) => [x.shard, x.payload.userId, x.payload.connId]), [[1, 2, 20], [0, 1, 10]]);
        const gameId = attaches[0].payload.gameId;
        assert.equal(shardOfGameId(gameId), 0);
        assert.equal(cp.presenceClaim({ userId: 1, username: 'alice', shard: 1, connId: 11 }, 1).activeGame, gameId);
        assert.deepEqual(cp.gameEnded({ gameId, whiteId: 2, blackId: 1 }), { ok: true });
        assert.equal(cp.activeGames.size, 0);
    });

    it('places a game elsewhere when the preferred shard is overloaded or silent', async () => {
        const { cp, shards, clock } = setup({ live: [0, 1, 2] });
        cp.handlers['shard.load']({ conns: 100, games: 50, lagP99: 80, overloaded: true }, 0);
        cp.handlers['shard.load']({ conns: 10, games: 5, overloaded: false }, 1);
        cp.handlers['shard.load']({ conns: 3, games: 2, overloaded: false }, 2);
        assert.equal(cp._chooseShard(0), 2);
        assert.equal(cp._chooseShard(1), 1);
        clock.advance(11000);                                           // reports went stale
        cp.handlers['shard.load']({ conns: 10, games: 5, overloaded: false }, 1);
        assert.equal(cp._chooseShard(2), 1);
        const white = { userId: 1, name: 'a', rating: 1500, provisional: false }, black = { userId: 2, name: 'b', rating: 1500, provisional: false };
        const r = await cp.createGame({ category: '5+0', baseMs: 300000, incMs: 0, rated: true, white, black }, 0, 'queue');
        assert.equal(r.ok, true);
        assert.equal(shards.requests[0].shard, 1);
    });

    it('colours alternate over queue games, a failed creation gives them back, challenges and rematches do not count', async () => {
        const { cp, shards, mm, clock, online } = setup({ realMatchmaker: true });
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1);
        const whites = [];
        const playQueueGame = async () => {
            cp.mmJoin({ ...a, category: '5+0', rated: false, rating: 1500 }, 0);
            cp.mmJoin({ ...b, category: '5+0', rated: false, rating: 1500 }, 1);
            clock.advance(250);
            shards.clear();
            cp.matchTick();
            await tick();
            const gameId = shards.of('game.attach')[0]?.payload.gameId;
            if (gameId) {
                whites.push(shards.requests[0].payload.spec.white.userId);
                cp.gameEnded({ gameId, whiteId: 1, blackId: 2 });
            }
            return gameId;
        };
        for (let i = 0; i < 4; i++) await playQueueGame();
        assert.deepEqual(whites, [1, 2, 1, 2]);
        assert.deepEqual([mm.colorBalanceOf(1), mm.colorBalanceOf(2)], [0, 0]);
        shards.create = () => ({ error: E.Internal });
        assert.equal(await playQueueGame(), undefined);
        assert.deepEqual([mm.colorBalanceOf(1), mm.colorBalanceOf(2)], [0, 0], 'the failed game counts for nobody');
        cp.mmLeave({ userId: 1 }); cp.mmLeave({ userId: 2 });
        shards.create = null;
        clock.advance(PAIR_RETRY_DELAY_MS);
        // Alice White in a challenge: the balances do not move, the next queue game is drawn.
        const c = cp.challengeCreate({ from: a, target: 'bob', baseSec: 300, incSec: 0, rated: false, color: enums.ColorPref.White });
        const acc = await cp.challengeAccept({ id: c.id, by: b });
        assert.equal(acc.ok, true);
        assert.deepEqual([mm.colorBalanceOf(1), mm.colorBalanceOf(2)], [0, 0]);
        cp.gameEnded({ gameId: acc.gameId, whiteId: 1, blackId: 2 });
        await playQueueGame();
        assert.equal(whites.at(-1), 1);
        // Alice Black in a rematch: no count either, the next queue game gives her Black after her White.
        const rm = await cp.gameRematch({ gameId: acc.gameId, white: 2, black: 1, category: '5+0', baseMs: 300000, incMs: 0, rated: false });
        assert.equal(rm.ok, true);
        assert.deepEqual([mm.colorBalanceOf(1), mm.colorBalanceOf(2)], [1, -1]);
        cp.gameEnded({ gameId: rm.gameId, whiteId: 2, blackId: 1 });
        await playQueueGame();
        assert.equal(whites.at(-1), 2);
    });

    it('a player who took White in many challenges still alternates colours in the queue', async () => {
        const { cp, shards, clock, online } = setup({ realMatchmaker: true });
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1);
        for (let i = 0; i < 5; i++) {
            const c = cp.challengeCreate({ from: a, target: 'bob', baseSec: 300, incSec: 0, rated: false, color: enums.ColorPref.White });
            const acc = await cp.challengeAccept({ id: c.id, by: b });
            assert.equal(acc.ok, true);
            cp.gameEnded({ gameId: acc.gameId, whiteId: 1, blackId: 2 });
        }
        // Fresh opponents each time (balance 0): a draw, then the other colour, and so on.
        let colours = '';
        for (let i = 0; i < 6; i++) {
            const fresh = online(10 + i, `fresh${i}`, 1);
            cp.mmJoin({ ...a, category: '5+0', rated: false, rating: 1500 }, 0);
            cp.mmJoin({ ...fresh, category: '5+0', rated: false, rating: 1500 }, 1);
            clock.advance(250);
            shards.clear();
            cp.matchTick();
            await tick();
            const { white, black } = shards.requests[0].payload.spec;
            colours += white.userId === 1 ? 'W' : 'B';
            cp.gameEnded({ gameId: shards.of('game.attach')[0].payload.gameId, whiteId: white.userId, blackId: black.userId });
        }
        assert.equal(colours, 'WBWBWB');
    });

    it('a rated pairing counts toward MATCH_REPEAT_LIMIT only once its game exists', async () => {
        const { cp, shards, mm, clock, online } = setup({ realMatchmaker: true });
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1);
        cp.mmJoin({ ...a, category: '5+0', rated: true, rating: 1500 }, 0);
        cp.mmJoin({ ...b, category: '5+0', rated: true, rating: 1500 }, 1);
        shards.create = () => ({ error: E.Internal });
        for (let i = 0; i <= cfg.matchRepeatLimit; i++) {
            clock.advance(PAIR_RETRY_DELAY_MS);
            shards.clear();
            cp.matchTick();
            await tick();
            assert.equal(shards.requests.length, 1, `pairing ${i} tried`);
            assert.equal(mm.repeatCount(1, 2), 0);
        }
        shards.create = null;
        clock.advance(PAIR_RETRY_DELAY_MS);
        cp.matchTick();
        await tick();
        assert.equal(cp.activeGames.size, 2);
        assert.equal(mm.repeatCount(1, 2), 1);
    });

    it('a pairing whose game could not be created is not made again for PAIR_RETRY_DELAY_MS; other pairings go on', async () => {
        const { cp, shards, clock, online } = setup({ realMatchmaker: true });
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1), c = online(3, 'carl', 1);
        const players = (r) => [r.payload.spec.white.userId, r.payload.spec.black.userId].sort();
        const round = async (ms) => {
            clock.advance(ms);
            shards.clear();
            cp.matchTick();
            await tick();
            return shards.requests.filter((r) => r.type === 'game.create').map(players);
        };
        cp.mmJoin({ ...a, category: '5+0', rated: true, rating: 1500 }, 0);
        cp.mmJoin({ ...b, category: '5+0', rated: true, rating: 1500 }, 1);
        shards.create = () => ({ error: E.Internal });
        assert.deepEqual(await round(250), [[1, 2]]);
        assert.deepEqual([...cp.queued.keys()].sort(), [1, 2], 'both back in the queue');
        assert.deepEqual(await round(250), [], 'not at the next tick');
        assert.deepEqual(await round(PAIR_RETRY_DELAY_MS - 500), []);
        assert.deepEqual(await round(250), [[1, 2]], 'again once the delay is over');
        // Held, either of them takes another opponent at once.
        cp.mmJoin({ ...c, category: '5+0', rated: true, rating: 1500 }, 1);
        shards.create = null;
        const [pair] = await round(250);
        assert.equal(pair.length, 2);
        assert.ok(pair.includes(3), `paired with carl: ${pair}`);
        assert.equal(cp.activeGames.size, 2);
    });

    it('MATCH_REPEAT_LIMIT counts rated challenges, private games and rematches, and refuses them past it; unrated games stay free', async () => {
        const { cp, ch, shards, mm, clock, online } = setup({ realMatchmaker: true });
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1), c = online(3, 'carl', 1);
        const tc = { baseSec: 300, incSec: 0 }, rated = { ...tc, rated: true };
        const end = (gameId) => cp.gameEnded({ gameId, whiteId: 1, blackId: 2 });
        const rematch = (gameId, isRated) => cp.gameRematch({ gameId, white: 2, black: 1, category: '5+0', baseMs: 300000, incMs: 0, rated: isRated });
        const created = () => shards.requests.filter((r) => r.type === 'game.create').length;
        // Three rated games: a direct challenge, a private game, a rematch.
        const c1 = cp.challengeCreate({ from: a, target: 'bob', ...rated });
        const g1 = await cp.challengeAccept({ id: c1.id, by: b });
        assert.equal(g1.ok, true);
        assert.equal(mm.repeatCount(1, 2), 1);
        end(g1.gameId);
        const p2 = cp.challengeCreate({ from: a, target: '', ...rated });
        const g2 = await cp.challengeJoinCode({ code: p2.code, by: b });
        assert.equal(g2.ok, true);
        assert.equal(mm.repeatCount(1, 2), 2);
        end(g2.gameId);
        const pending = cp.challengeCreate({ from: b, target: 'alice', ...rated });
        assert.equal(pending.ok, true, 'two rated games: a third may be offered');
        const g3 = await rematch(g2.gameId, true);
        assert.equal(g3.ok, true);
        assert.equal(mm.repeatCount(1, 2), 3);
        end(g3.gameId);
        // The limit is reached: no more rated games between them, however made.
        shards.clear();
        assert.deepEqual(await cp.challengeAccept({ id: pending.id, by: a }), { error: E.RatedRepeatLimit }, 'offered before, accepted after');
        assert.deepEqual(shards.frames().map((f) => [f.connId, f.name, f.msg.state]), [[20, 'ChallengeStatus', CS.Unavailable]]);
        shards.clear();
        assert.deepEqual(cp.challengeCreate({ from: a, target: 'bob', ...rated }), { error: E.RatedRepeatLimit });
        assert.deepEqual(shards.frames(), [], 'nothing reaches the target');
        // A wrong time control keeps its own error.
        assert.deepEqual(cp.challengeCreate({ from: a, target: 'bob', baseSec: 420, incSec: 1, rated: true }), { error: E.RatedRequiresOfficialTc });
        assert.deepEqual(cp.challengeCreate({ from: a, target: 'bob', baseSec: 5, incSec: 0, rated: true }), { error: E.InvalidTimeControl });
        const p4 = cp.challengeCreate({ from: b, target: '', ...rated });
        shards.clear();
        assert.deepEqual(await cp.challengeJoinCode({ code: p4.code, by: a }), { error: E.RatedRepeatLimit });
        assert.equal(ch.getCode(p4.code)?.id, p4.id, 'the private game stays pending');
        assert.deepEqual(shards.frames(), [], 'and its creator is told nothing');
        assert.equal(cp.limiter.peek('joincode:u1'), 0, 'not a wrong code');
        // The code stays valid for anyone else.
        const g4 = await cp.challengeJoinCode({ code: p4.code, by: c });
        assert.equal(g4.ok, true);
        cp.gameEnded({ gameId: g4.gameId, whiteId: 2, blackId: 3 });
        shards.clear();
        assert.deepEqual(await rematch(g3.gameId, true), { error: E.RematchUnavailable });
        cp.mmJoin({ ...a, category: '5+0', rated: true, rating: 1500 }, 0);
        cp.mmJoin({ ...b, category: '5+0', rated: true, rating: 1500 }, 1);
        assert.deepEqual([...cp.queued.keys()].sort(), [1, 2]);
        clock.advance(250);
        cp.matchTick();
        await tick();
        assert.deepEqual([...cp.queued.keys()].sort(), [1, 2], 'both still waiting');
        cp.mmLeave({ userId: 1 }); cp.mmLeave({ userId: 2 });
        assert.equal(created(), 0, 'no game was created');
        // Unrated games stay free, and other opponents are not concerned.
        const g5 = await cp.challengeAccept({ id: cp.challengeCreate({ from: a, target: 'bob', ...tc, rated: false }).id, by: b });
        assert.equal(g5.ok, true);
        end(g5.gameId);
        const g6 = await rematch(g5.gameId, false);
        assert.equal(g6.ok, true);
        end(g6.gameId);
        const g8 = await cp.challengeJoinCode({ code: cp.challengeCreate({ from: a, target: '', ...tc, rated: false }).code, by: b });
        assert.equal(g8.ok, true);
        end(g8.gameId);
        assert.equal(cp.challengeCreate({ from: a, target: 'carl', ...rated }).ok, true);
        assert.equal(mm.repeatCount(1, 2), 3);
        // The games leave the count after MATCH_REPEAT_WINDOW_MS.
        clock.advance(cfg.matchRepeatWindowMs);
        const g7 = await cp.challengeAccept({ id: cp.challengeCreate({ from: b, target: 'alice', ...rated }).id, by: a });
        assert.equal(g7.ok, true);
        assert.equal(mm.repeatCount(1, 2), 1);
    });

    it('requeues both players when the host refuses the game', async () => {
        const { cp, shards, mm, online } = setup();
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1);
        shards.create = () => ({ error: E.Internal });
        const ea = { ...a, category: '3+2', rated: false, rating: 1500, joinedAt: 1 }, eb = { ...b, category: '3+2', rated: false, rating: 1500, joinedAt: 2 };
        mm.pairs.push({ category: '3+2', rated: false, white: ea, black: eb });
        cp.matchTick();
        await tick();
        await tick();
        assert.deepEqual([...mm.q.keys()].sort(), [1, 2]);
        assert.equal(cp.queued.size, 2);
        assert.equal(cp.activeGames.size, 0);
        assert.equal(cp.starting.size, 0);
    });
});

describe('control plane: challenges', () => {
    it('direct challenge: Pending to the creator, Received to the target, accept starts the game on the creator shard', async () => {
        const { cp, shards, online } = setup();
        const a = online(1, 'alice', 1), b = online(2, 'bob', 0);
        const r = cp.challengeCreate({ from: a, target: 'Bob', baseSec: 300, incSec: 0, rated: true, color: enums.ColorPref.White });
        assert.equal(r.ok, true);
        const f = shards.frames();
        assert.deepEqual(f.map((x) => [x.connId, x.name]), [[10, 'ChallengeStatus'], [20, 'ChallengeReceived']]);
        assert.equal(f[0].msg.state, CS.Pending);
        assert.equal(f[1].msg.from.name, 'alice');
        assert.equal(f[1].msg.yourColor, enums.ColorPref.Black);
        assert.equal(f[1].msg.expiresMs, cfg.challengeTtlMs);
        shards.clear();
        const acc = await cp.challengeAccept({ id: r.id, by: b });
        assert.equal(acc.ok, true);
        assert.equal(shards.requests[0].shard, 1);
        const spec = shards.requests[0].payload.spec;
        assert.deepEqual([spec.white.userId, spec.black.userId, spec.category, spec.rated], [1, 2, '5+0', true]);
        assert.ok(shards.frames().some((x) => x.connId === 10 && x.name === 'ChallengeStatus' && x.msg.state === CS.Accepted));
        assert.equal(cp.activeGames.get(2), acc.gameId);
        const again = cp.challengeCreate({ from: a, target: 'bob', baseSec: 300, incSec: 0, rated: false });
        assert.equal(again.ok, true);
        assert.deepEqual(await cp.challengeAccept({ id: again.id, by: b }), { error: E.AlreadyInGame });
    });

    it('a busy creator is told to the challenge\'s target only; another player learns nothing', async () => {
        const { cp, ch, online } = setup();
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1), c = online(3, 'carl', 1);
        const direct = cp.challengeCreate({ from: a, target: 'bob', baseSec: 300, incSec: 0, rated: false });
        const priv = cp.challengeCreate({ from: a, target: '', baseSec: 300, incSec: 0, rated: false });
        cp.gameActive({ gameId: new GameIdAllocator(0).next(), whiteId: 1, blackId: 9 });
        for (const id of [direct.id, priv.id]) assert.deepEqual(await cp.challengeAccept({ id, by: c }), { error: E.ChallengeNotFound });
        assert.deepEqual(await cp.challengeAccept({ id: direct.id, by: b }), { error: E.AlreadyInGame });
        assert.equal(ch.size, 2, 'nothing consumed');
    });

    it('decline, cancel, expiry and a creator going offline notify the other side', () => {
        const { cp, shards, clock, online } = setup({ acceptsChallenges: (u) => u !== 4 });
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1);
        online(4, 'dora', 1);
        assert.deepEqual(cp.challengeCreate({ from: a, target: 'dora', baseSec: 60, incSec: 0, rated: false }), { error: E.UserUnavailable });
        assert.deepEqual(cp.challengeCreate({ from: a, target: 'nobody', baseSec: 60, incSec: 0, rated: false }), { error: E.UserUnavailable });
        const states = () => shards.frames().filter((x) => x.name === 'ChallengeStatus').map((x) => [x.connId, x.msg.state]);
        let c = cp.challengeCreate({ from: a, target: 'bob', baseSec: 60, incSec: 0, rated: false });
        shards.clear();
        assert.deepEqual(cp.challengeDecline({ id: c.id, userId: 2 }), { ok: true });
        assert.deepEqual(states(), [[10, CS.Declined]]);
        c = cp.challengeCreate({ from: a, target: 'bob', baseSec: 60, incSec: 0, rated: false });
        shards.clear();
        assert.deepEqual(cp.challengeCancel({ id: c.id, userId: 1 }), { ok: true });
        assert.deepEqual(states(), [[20, CS.Cancelled]]);
        assert.deepEqual(cp.challengeCancel({ id: c.id, userId: 1 }), { error: E.ChallengeNotFound });
        cp.challengeCreate({ from: a, target: 'bob', baseSec: 60, incSec: 0, rated: false });
        shards.clear();
        clock.advance(cfg.challengeTtlMs + 1);
        cp.expireChallenges();
        assert.deepEqual(states(), [[10, CS.Expired], [20, CS.Expired]]);
        cp.challengeCreate({ from: b, target: 'alice', baseSec: 60, incSec: 0, rated: false });
        shards.clear();
        cp.presenceRelease({ userId: 2, connId: 20 }, 1);
        assert.deepEqual(states(), [[10, CS.Cancelled]]);
    });

    it('direct challenges: five withdrawn or declined a minute per creator, so create/cancel cycles cannot flood a target', async () => {
        const { cp, shards, clock, online } = setup();
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1);
        online(3, 'carl', 1);
        clock.advance(1000);
        // Accepted challenges (games) and refused attempts are not counted.
        for (let i = 0; i < 6; i++) {
            const c = cp.challengeCreate({ from: a, target: 'bob', baseSec: 60, incSec: 0, rated: false });
            const acc = await cp.challengeAccept({ id: c.id, by: b });
            assert.equal(acc.ok, true, `game ${i}`);
            cp.gameEnded({ gameId: acc.gameId, whiteId: 1, blackId: 2 });
        }
        assert.deepEqual(cp.challengeCreate({ from: a, target: 'nobody', baseSec: 60, incSec: 0, rated: false }), { error: E.UserUnavailable });
        for (let i = 0; i < 5; i++) {
            const c = cp.challengeCreate({ from: a, target: i & 1 ? 'carl' : 'bob', baseSec: 60, incSec: 0, rated: false });
            assert.equal(c.ok, true, `challenge ${i}`);
            if (i === 4) assert.deepEqual(cp.challengeDecline({ id: c.id, userId: 2 }), { ok: true });
            else assert.deepEqual(cp.challengeCancel({ id: c.id, userId: 1 }), { ok: true });
        }
        shards.clear();
        assert.deepEqual(cp.challengeCreate({ from: a, target: 'bob', baseSec: 60, incSec: 0, rated: false }), { error: E.ChallengeLimit });
        assert.deepEqual(shards.frames(), [], 'nothing reaches the target');
        assert.equal(cp.challengeCreate({ from: a, target: '', baseSec: 60, incSec: 0, rated: false }).ok, true, 'private games are not counted');
        clock.advance(120000);
        assert.equal(cp.challengeCreate({ from: a, target: 'bob', baseSec: 60, incSec: 0, rated: false }).ok, true);
    });

    it('private codes: ten wrong ones a minute per player, then RateLimited; a code that works costs nothing', async () => {
        const { cp, clock, online } = setup();
        const a = online(1, 'alice', 0), c = online(3, 'carl', 1), d = online(4, 'dora', 1);
        clock.advance(1000);
        const r = cp.challengeCreate({ from: a, target: '', baseSec: 180, incSec: 2, rated: false });
        for (let i = 0; i < 9; i++) assert.deepEqual(await cp.challengeJoinCode({ code: 'XXXXXX', by: c }), { error: E.CodeInvalid });
        assert.deepEqual(await cp.challengeJoinCode({ code: r.code, by: a }), { error: E.CannotChallengeSelf });
        assert.equal((await cp.challengeJoinCode({ code: r.code, by: c })).ok, true);
        const r2 = cp.challengeCreate({ from: d, target: '', baseSec: 180, incSec: 2, rated: false });
        const e = online(5, 'emil', 0);
        for (let i = 0; i < 10; i++) assert.deepEqual(await cp.challengeJoinCode({ code: 'XXXXXX', by: e }), { error: E.CodeInvalid });
        assert.deepEqual(await cp.challengeJoinCode({ code: r2.code, by: e }), { error: E.RateLimited });
        clock.advance(120000);
        assert.equal((await cp.challengeJoinCode({ code: r2.code, by: e })).ok, true);
    });

    it('both limits are settings: CHALLENGE_UNPLAYED_PER_MIN (5) and PRIVATE_CODE_FAILURES_PER_MIN (10), at least 1', async () => {
        assert.deepEqual([cfg.challengeUnplayedPerMin, cfg.privateCodeFailuresPerMin], [5, 10]);
        for (const k of ['CHALLENGE_UNPLAYED_PER_MIN', 'PRIVATE_CODE_FAILURES_PER_MIN']) {
            assert.throws(() => testConfig({ [k]: '0' }), new RegExp(`${k}: at least 1`));
            assert.throws(() => testConfig({ [k]: 'many' }), new RegExp(`${k}: integer expected`));
        }
        const { cp, online } = setup({ config: testConfig({ CHALLENGE_UNPLAYED_PER_MIN: '2', PRIVATE_CODE_FAILURES_PER_MIN: '3' }) });
        const a = online(1, 'alice', 0), b = online(2, 'bob', 1);
        for (let i = 0; i < 2; i++) {
            const c = cp.challengeCreate({ from: a, target: 'bob', baseSec: 60, incSec: 0, rated: false });
            assert.deepEqual(cp.challengeCancel({ id: c.id, userId: 1 }), { ok: true });
        }
        assert.deepEqual(cp.challengeCreate({ from: a, target: 'bob', baseSec: 60, incSec: 0, rated: false }), { error: E.ChallengeLimit });
        for (let i = 0; i < 3; i++) assert.deepEqual(await cp.challengeJoinCode({ code: 'XXXXXX', by: b }), { error: E.CodeInvalid });
        assert.deepEqual(await cp.challengeJoinCode({ code: 'XXXXXX', by: b }), { error: E.RateLimited });
    });

    it('private game: a code, joined by anyone with it', async () => {
        const { cp, shards, online } = setup();
        const a = online(1, 'alice', 0), c = online(3, 'carl', 1);
        const r = cp.challengeCreate({ from: a, target: '', baseSec: 180, incSec: 2, rated: false, color: enums.ColorPref.Black });
        assert.match(r.code, /^[2-9A-Z]{6}$/);
        assert.deepEqual(await cp.challengeJoinCode({ code: 'XXXXXX', by: c }), { error: E.CodeInvalid });
        const j = await cp.challengeJoinCode({ code: r.code.toLowerCase(), by: c });
        assert.equal(j.ok, true);
        const spec = shards.requests.at(-1).payload.spec;
        assert.deepEqual([spec.white.userId, spec.black.userId, spec.baseMs, spec.incMs, spec.category], [3, 1, 180000, 2000, '3+2']);
    });
});

describe('control plane: clock press', () => {
    it('gives every new game AUTO_PRESS_CLOCK (queue, challenge, private code); a rematch keeps the finished game\'s', async () => {
        for (const auto of [true, false]) {
            const { cp, shards, mm, online } = setup({ config: testConfig({ AUTO_PRESS_CLOCK: String(auto) }) });
            const u = [1, 2, 3, 4, 5, 6].map((id) => online(id, `user${id}`, id & 1));
            const entry = (p, joinedAt) => ({ ...p, category: '5+0', rated: false, rating: 1500, joinedAt });
            mm.pairs.push({ category: '5+0', rated: false, white: entry(u[0], 1), black: entry(u[1], 2) });
            cp.matchTick();
            await tick();
            const ch = cp.challengeCreate({ from: u[2], target: 'user4', baseSec: 180, incSec: 2, rated: false });
            assert.equal((await cp.challengeAccept({ id: ch.id, by: u[3] })).ok, true);
            const priv = cp.challengeCreate({ from: u[4], target: '', baseSec: 60, incSec: 0, rated: false });
            assert.equal((await cp.challengeJoinCode({ code: priv.code, by: u[5] })).ok, true);
            const specs = () => shards.requests.filter((r) => r.type === 'game.create').map((r) => r.payload.spec);
            assert.deepEqual(specs().map((s) => s.autoPress), [auto, auto, auto]);
            // A rematch: the finished game's setting, whatever the configuration says now.
            const first = cp.activeGames.get(1);
            const r = await cp.gameRematch({ gameId: first, white: 2, black: 1, baseMs: 300000, incMs: 0, rated: false, autoPress: !auto });
            assert.equal(r.ok, true);
            assert.equal(specs().at(-1).autoPress, !auto);
            cp.gameEnded({ gameId: r.gameId, whiteId: 2, blackId: 1 });
            const r2 = await cp.gameRematch({ gameId: r.gameId, white: 1, black: 2, baseMs: 300000, incMs: 0, rated: false });
            assert.equal(r2.ok, true);
            assert.equal(specs().at(-1).autoPress, auto, 'AUTO_PRESS_CLOCK when the request has none');
        }
    });
});

describe('control plane: games, sanctions, shards', () => {
    it('rematch: the old host shard, colours as given, refused when a player left', async () => {
        const { cp, shards, online } = setup();
        online(1, 'alice', 0); online(2, 'bob', 1);
        const oldId = new GameIdAllocator(1).next();
        cp.gameActive({ gameId: oldId, whiteId: 1, blackId: 2 });
        const r = await cp.gameRematch({ gameId: oldId, white: { userId: 2, name: 'bob' }, black: { userId: 1, name: 'alice' }, baseMs: 180000, incMs: 2000, rated: true });
        assert.equal(r.ok, true);
        const req = shards.requests.at(-1);
        assert.equal(req.shard, 1);
        assert.deepEqual([req.payload.spec.white.userId, req.payload.spec.rematchOf, req.payload.spec.category], [2, oldId, '3+2']);
        assert.equal(cp.activeGames.get(1), r.gameId);
        cp.gameEnded({ gameId: r.gameId, whiteId: 2, blackId: 1 });
        cp.presenceRelease({ userId: 2, connId: 20 }, 1);
        assert.deepEqual(await cp.gameRematch({ gameId: r.gameId, white: 1, black: 2, baseMs: 180000, incMs: 2000, rated: true }), { error: E.RematchUnavailable });
    });

    it('sanction: kick 4004 with Banned, forfeit on the host shard, no reconnection', () => {
        const { cp, shards, clock, online } = setup();
        online(1, 'alice', 0); online(2, 'bob', 1);
        const gameId = new GameIdAllocator(1).next();
        cp.gameActive({ gameId, whiteId: 1, blackId: 2 });
        const until = clock.now() + 3600000;
        cp.sanctionApplied({ userId: 1, until, reason: 'engine' });
        const [kick] = shards.of('conn.kick');
        assert.deepEqual([kick.shard, kick.payload.connId, kick.payload.closeCode], [0, 10, 4004]);
        assert.equal(shards.frames().find((f) => f.name === 'Error').msg.code, E.Banned);
        assert.ok(shards.frames().some((f) => f.name === 'Notice' && f.msg.code === N.Banned && f.msg.arg === until));
        assert.deepEqual(shards.of('game.forfeit').map((x) => [x.shard, x.payload.userId, x.payload.gameId]), [[1, 1, gameId]]);
        assert.deepEqual(cp.presenceClaim({ userId: 1, username: 'alice', shard: 0, connId: 11 }, 0), { error: E.Banned, until });
        clock.advance(3600001);
        cp.sweep();
        assert.equal(cp.presenceClaim({ userId: 1, username: 'alice', shard: 0, connId: 11 }, 0).ok, true);
    });

    it('a ban only in the database (the admin CLI) stops the next game: queue, challenge and rematch refused, the player kicked', async () => {
        const banned = new Map();
        const { cp, shards, mm, clock, online } = setup({ activeBan: (u, now) => (banned.get(u) > now ? { until: banned.get(u) } : null) });
        const [a, b, c, d, e, f] = [[1, 'alice', 0], [2, 'bob', 1], [3, 'carol', 0], [4, 'dave', 1], [5, 'erin', 0], [6, 'frank', 1]]
            .map(([id, name, shard]) => online(id, name, shard));
        const until = clock.now() + 3600000;
        const kicked = () => shards.of('conn.kick').map((k) => [k.payload.connId, k.payload.closeCode]);
        const creates = () => shards.requests.filter((r) => r.type === 'game.create').length;

        // Alice is banned while she searches and has challenged Bob: paired with Carol, no game;
        // Alice is kicked, her challenge withdrawn, Carol searches again.
        assert.deepEqual(cp.mmJoin({ ...a, category: '5+0', rated: true, rating: 1500, joinedAt: clock.now() }, 0), { ok: true });
        assert.equal(cp.mmJoin({ ...c, category: '5+0', rated: true, rating: 1500, joinedAt: clock.now() }, 0).ok, true);
        const toBob = cp.challengeCreate({ from: a, target: 'bob', baseSec: 300, incSec: 0, rated: true });
        banned.set(1, until);
        mm.pairs.push({ category: '5+0', rated: true, white: { ...mm.q.get(1) }, black: { ...mm.q.get(3) } });
        mm.q.clear();
        shards.clear();
        cp.matchTick();
        await tick();
        await tick();
        assert.equal(creates(), 0);
        assert.deepEqual(kicked(), [[a.connId, CloseCode.Banned]]);
        assert.ok(shards.frames().some((x) => x.name === 'Notice' && x.msg.code === N.Banned && x.msg.arg === until));
        assert.deepEqual([...mm.q.keys()], [3]);
        assert.deepEqual(await cp.challengeAccept({ id: toBob.id, by: b }), { error: E.ChallengeNotFound });

        // Dave, banned while idle: his next queue join or challenge is refused.
        banned.set(4, until);
        shards.clear();
        assert.deepEqual(cp.mmJoin({ ...d, category: '5+0', rated: true, rating: 1500 }, 1), { error: E.Banned });
        assert.deepEqual(kicked(), [[d.connId, CloseCode.Banned]]);
        assert.equal(mm.q.has(4), false);
        shards.clear();
        assert.deepEqual(cp.challengeCreate({ from: d, target: 'bob', baseSec: 300, incSec: 0, rated: true }), { error: E.Banned });
        assert.deepEqual(kicked(), [[d.connId, CloseCode.Banned]]);
        assert.equal(cp.ch.size, 0);

        // Erin, banned before she accepts Bob's challenge: no game, Bob is told she is unavailable.
        const toErin = cp.challengeCreate({ from: b, target: 'erin', baseSec: 300, incSec: 0, rated: true });
        banned.set(5, until);
        shards.clear();
        assert.deepEqual(await cp.challengeAccept({ id: toErin.id, by: e }), { error: E.UserUnavailable });
        assert.equal(creates(), 0);
        assert.deepEqual(kicked(), [[e.connId, CloseCode.Banned]]);
        assert.ok(shards.frames().some((x) => x.connId === b.connId && x.name === 'ChallengeStatus' && x.msg.state === CS.Unavailable));

        // Frank, banned after a game: no rematch.
        const gameId = new GameIdAllocator(1).next();
        cp.gameActive({ gameId, whiteId: 2, blackId: 6 });
        cp.gameEnded({ gameId, whiteId: 2, blackId: 6 });
        banned.set(6, until);
        shards.clear();
        assert.deepEqual(await cp.gameRematch({ gameId, white: 6, black: 2, baseMs: 300000, incMs: 0, rated: true }), { error: E.RematchUnavailable });
        assert.equal(creates(), 0);
        assert.deepEqual(kicked(), [[f.connId, CloseCode.Banned]]);
        assert.deepEqual(cp.presenceClaim({ userId: 6, username: 'frank', shard: 1, connId: 61 }, 1), { error: E.Banned, until });
        // Not cached: an unban in the database counts at once.
        banned.delete(6);
        assert.equal(cp.presenceClaim({ userId: 6, username: 'frank', shard: 1, connId: 61 }, 1).ok, true);
    });

    it('conduct records push the cooldown notice; session revocations are broadcast', () => {
        const until = Date.UTC(2026, 5, 2);
        const recorded = [];
        const { cp, shards, online } = setup({ conduct: { record: (u, k) => recorded.push([u, k]), cooldownUntil: () => until } });
        online(1, 'alice', 0);
        cp.conductRecord({ userId: 1, kind: 'abandon' });
        assert.deepEqual(recorded, [[1, 'abandon']]);
        assert.equal(shards.frames()[0].msg.code, N.MatchmakingCooldown);
        cp.sessionRevoked({ userId: 1, tokenHashes: ['ab'] });
        cp.sessionRevoked({ userId: 1, tokenHashes: null });
        assert.deepEqual(shards.of('auth.invalidate'), [{ shard: '*', type: 'auth.invalidate', payload: { userId: 1, tokenHashes: ['ab'] } },
            { shard: '*', type: 'auth.invalidate', payload: { userId: 1, tokenHashes: null } }]);
    });

    it('shard ready re-attaches its games; shard down forgets its users and tells the others', () => {
        const { cp, shards, online } = setup({ live: [0, 1, 2] });
        online(1, 'alice', 0); online(2, 'bob', 2); online(3, 'carl', 1);
        const gameId = new GameIdAllocator(1).next();
        cp.handlers['game.recovered']({ gameId, whiteId: 1, blackId: 2, shard: 1 });
        assert.deepEqual(cp.shardReady(1), { ok: true, reattached: 2 });
        assert.deepEqual(shards.of('game.attach').map((x) => [x.shard, x.payload.userId, x.payload.gameId]), [[0, 1, gameId], [2, 2, gameId]]);
        assert.ok(cp.readyShards.has(1));
        shards.clear();
        assert.equal(cp.shardDown(1), 1);
        assert.equal(cp.presence.get(3), undefined);
        assert.ok(!cp.readyShards.has(1));
        assert.deepEqual(shards.of('shard.down').map((x) => x.shard), [0, 2]);
    });

    it('a restarted shard that does not replay a game frees its players at its shard.ready', async () => {
        const { cp, shards, online } = setup({ live: [0, 1] });
        online(1, 'alice', 0); online(2, 'bob', 0); online(3, 'carl', 0); online(4, 'dan', 0);
        const spec = (w, b) => ({ white: { userId: w, username: 'w' }, black: { userId: b, username: 'b' }, baseMs: 300000, incMs: 0, rated: true });
        const lost = (await cp.createGame(spec(1, 2), 1, 'challenge')).gameId;
        const kept = (await cp.createGame(spec(3, 4), 1, 'challenge')).gameId;
        cp.shardDown(1);                                        // crashed before the journal had the first game
        cp.handlers['game.recovered']({ gameId: kept, whiteId: 3, blackId: 4, shard: 1 }, 1);
        cp.handlers['game.recovered']({ gameId: lost, whiteId: 1, blackId: 2, shard: 0 }, 0);   // not its host: ignored
        shards.clear();
        assert.deepEqual(cp.shardReady(1), { ok: true, reattached: 2 });
        assert.deepEqual(shards.of('game.attach').map((x) => [x.payload.userId, x.payload.gameId]), [[3, kept], [4, kept]]);
        assert.deepEqual([...cp.activeGames], [[3, kept], [4, kept]]);
        assert.deepEqual(cp.mmJoin({ userId: 1, username: 'alice', category: '5+0', rated: true, rating: 1500, shard: 0, connId: 10 }, 0), { ok: true });
        assert.equal(cp.presenceClaim({ userId: 2, username: 'bob', shard: 0, connId: 21 }, 0).activeGame, 0);
        assert.equal(cp.presenceClaim({ userId: 3, username: 'carl', shard: 0, connId: 31 }, 0).activeGame, kept);
        shards.clear();
        cp.shardReady(1);                                       // a later shard.ready without a crash forgets nothing
        assert.equal(cp.activeGames.size, 2);
    });

    it('abuse.report leads to an abuse.block broadcast; a shard that becomes ready gets the running blocks; sweep ends them', () => {
        const { cp, shards, clock } = setup({ abuse: true, config: testConfig({ ABUSE_BLOCK_REFUSALS_PER_MIN: '50' }) });
        cp.handlers['abuse.report']({ entries: [['198.51.100.7', null, 30]] }, 0);
        assert.deepEqual(shards.of('abuse.block'), []);
        assert.equal(cp.handlers['abuse.report']({ entries: [['198.51.100.7', null, 20], ['198.51.100.8', null, 1]] }, 1), null);
        assert.deepEqual(shards.of('abuse.block'), [{ shard: '*', type: 'abuse.block', payload: { blocks: [['198.51.100.7', 60000, 1]] } }]);
        cp.handlers['abuse.report']({}, 0);                    // malformed: ignored
        shards.clear();
        clock.advance(15000);
        cp.shardReady(1);
        assert.deepEqual(shards.of('abuse.block'), [{ shard: 1, type: 'abuse.block', payload: { blocks: [['198.51.100.7', 45000, 1]] } }]);
        clock.advance(45000);
        cp.sweep();
        assert.equal(cp.abuse.size, 0);
        shards.clear();
        cp.shardReady(0);
        assert.deepEqual(shards.of('abuse.block'), [], 'nothing to give once the blocks ended');
    });

    it('builds its own tracker by default, broadcasting through the shards directory', () => {
        const { cp, shards } = setup({ config: testConfig({ ABUSE_BLOCK_REFUSALS_PER_MIN: '5' }) });
        cp.handlers['abuse.report']({ entries: [['2001:db8::/64', '2001:db8::/48', 5]] }, 0);
        assert.deepEqual(shards.of('abuse.block').map((x) => [x.shard, x.payload.blocks.map(([k, , l]) => [k, l])]), [['*', [['2001:db8::/64', 1]]]]);
    });
});

describe('control plane: game.create after its timeout', () => {
    it('cancels the game a host created after the primary gave up waiting', async () => {
        const { cp, shards } = setup();
        const gameId = new GameIdAllocator(1).next();
        shards.create = (shard, payload, opts) => {
            setImmediate(() => opts.onLate({ ok: true, gameId }));          // the late reply
            throw new IpcTimeoutError('game.create', 5000);
        };
        const spec = { white: { userId: 1, username: 'a' }, black: { userId: 2, username: 'b' }, baseMs: 300000, incMs: 0, rated: true };
        assert.deepEqual(await cp.createGame(spec, 1, 'challenge'), { error: E.Internal });
        await tick();
        assert.deepEqual(shards.of('game.cancel').map((x) => [x.shard, x.payload.gameId]), [[1, gameId]]);
        assert.equal(cp.activeGames.size, 0);
        shards.create = (shard, payload, opts) => {                       // a late refusal created nothing
            setImmediate(() => opts.onLate({ error: E.Internal }));
            throw new IpcTimeoutError('game.create', 5000);
        };
        await cp.createGame(spec, 1, 'challenge');
        await tick();
        assert.equal(shards.of('game.cancel').length, 1);
    });
});

describe('primary assembly', () => {
    it('the global rate limiter runs on a monotonic clock: a step back of the wall clock locks no key out', async (t) => {
        const silent = { child: () => silent, debug() {}, info() {}, warn() {}, error() {}, security() {} };
        const wall = Date.now;
        let step = 0;
        Date.now = () => wall() + step;                     // the system clock, as the primary reads it
        t.after(() => { Date.now = wall; });
        const primary = await startPrimary({
            config: testConfig(), log: silent, fork: () => { throw new Error('no worker in this test'); }, shards: [],
            matchmaker: new FakeMatchmaker(), challenges: new Challenges({ config: cfg }), registry: new Registry(),
        });
        t.after(() => primary.stop(0));
        const limiter = primary.controlPlane.limiter;
        const k = { key: 'auth:ip:192.0.2.1', limit: 2, windowMs: 100 };
        assert.ok(limiter.take(k).allowed && limiter.take(k).allowed);
        assert.equal(limiter.take(k).allowed, false);
        step = -3600000;                                    // the wall clock steps back an hour
        await new Promise((r) => setTimeout(r, 250));       // two windows of real time
        assert.equal(limiter.take(k).allowed, true);
        primary.controlPlane.sweep();
        assert.equal(limiter.take(k).allowed, true);
    });

    it('on SIGTERM, the shards are told to drain before the analysis process and the purge have stopped', async () => {
        const events = [];
        const later = (ms, what, fail) => new Promise((resolve, reject) => setTimeout(() => {
            events.push(what);
            if (fail) reject(new Error(what)); else resolve();
        }, ms));
        const errors = [];
        await stopPrimary({
            config: cfg, log: { error: (msg) => errors.push(msg) },
            primary: { stop: (graceMs) => { events.push(`drain ${graceMs}`); return later(20, 'shards gone'); } },
            retention: { stop: () => { events.push('retention.stop'); return later(10, 'retention stopped'); } },
            analysis: { stop: () => { events.push('analysis.stop'); return later(100, 'analysis stopped', true); } },
            store: { close: () => events.push('store.close') },
        });
        assert.deepEqual(events, [`drain ${cfg.shutdownGraceMs}`, 'retention.stop', 'analysis.stop', 'retention stopped', 'shards gone', 'analysis stopped', 'store.close']);
        assert.deepEqual(errors, ['analysis stop failed']);
    });

    it('a shard started after an edit of .env or of a secret file runs the configuration the primary loaded', async (t) => {
        const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-cfg-'));
        t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
        const envFile = path.join(dir, '.env'), secretFile = path.join(dir, 'secret');
        fs.writeFileSync(envFile, `SERVER_MOTD=before\nSERVER_SECRET_FILE=${secretFile}\n`);
        fs.writeFileSync(secretFile, Buffer.alloc(32, 1).toString('base64'));
        // The server's environment, which its workers inherit.
        const env = { SCACELITH_ENV_FILE: envFile, TLS_MODE: 'off', ALLOW_INSECURE_DEV: '1', WORKERS: '1', MAIL_TRANSPORT: 'none', LOG_LEVEL: 'error', METRICS_PORT: '0' };
        const saved = { ...process.env };
        Object.assign(process.env, env);
        t.after(() => { for (const k of Object.keys(env)) if (saved[k] === undefined) delete process.env[k]; else process.env[k] = saved[k]; });
        const config = loadConfig();
        // A worker: its IPC channel, and an exit when the primary tells it to stop.
        const workers = [];
        const fork = () => {
            const [a, b] = channelPair();
            const w = Object.assign(new EventEmitter(), { send: (...args) => a.send(...args), kill() {} });
            a.on('message', (m) => w.emit('message', m));
            new Ipc(b).on('shutdown', () => setImmediate(() => w.emit('exit', 0, null)));
            workers.push(b);
            return w;
        };
        const silent = { child: () => silent, debug() {}, info() {}, warn() {}, error() {}, security() {} };
        const primary = await startPrimary({
            config, log: silent, fork, shards: [0], matchmaker: new FakeMatchmaker(), challenges: new Challenges({ config: cfg }), registry: new Registry(),
        });
        t.after(() => primary.stop(0));
        // The operator edits .env and rotates the secret, to apply them at the next restart.
        fs.writeFileSync(envFile, `SERVER_MOTD=after\nSERVER_SECRET_FILE=${secretFile}\n`);
        fs.writeFileSync(secretFile, Buffer.alloc(32, 2).toString('base64'));
        assert.equal(loadConfig().serverMotd, 'after');
        const shard = await primaryConfig(new Ipc(workers[0]));
        assert.equal(shard.serverMotd, 'before');
        assert.deepEqual(shard.serverSecret, Buffer.alloc(32, 1));
        assert.deepEqual(describeConfig(shard), describeConfig(config));
        assert.equal(await new Ipc(workers[0]).request('tls.ticketKeys'), null, 'no shared ticket keys without native TLS');
    });

    it('gives every shard, a restarted one included, the current state of the session-ticket keys (native TLS)', async (t) => {
        const wall = Date.now;
        let shift = 0;
        Date.now = () => wall() + shift;
        t.after(() => { Date.now = wall; });
        const workers = [];
        const fork = () => {
            const [a, b] = channelPair();
            const w = Object.assign(new EventEmitter(), { send: (...args) => a.send(...args), kill() {} });
            a.on('message', (m) => w.emit('message', m));
            const ipc = new Ipc(b).on('shutdown', () => setImmediate(() => w.emit('exit', 0, null)));
            workers.push({ w, ipc });
            return w;
        };
        const silent = { child: () => silent, debug() {}, info() {}, warn() {}, error() {}, security() {} };
        const primary = await startPrimary({
            config: { ...cfg, tlsMode: 'native' }, log: silent, fork, shards: [0, 1], matchmaker: new FakeMatchmaker(),
            challenges: new Challenges({ config: cfg }), registry: new Registry(),
        });
        t.after(() => primary.stop(0));
        const first = await workers[0].ipc.request('tls.ticketKeys');
        assert.equal(first.key.length, 32);
        assert.deepEqual(await workers[1].ipc.request('tls.ticketKeys'), first, 'every shard starts from the same state');
        // Shard 1 crashes the next day: the supervisor starts it again, and it gets the state the
        // running shard has moved to, not the one of the primary's start.
        shift = 86400000;
        workers[1].w.emit('exit', 1, null);
        for (let i = 0; i < 100 && workers.length < 3; i++) await new Promise((r) => setTimeout(r, 20));
        const restarted = await workers[2].ipc.request('tls.ticketKeys');
        assert.equal(restarted.day, first.day + 1);
        assert.notDeepEqual(restarted.key, first.key);
        assert.deepEqual(new TicketKeys(restarted).ticketKeys(), new TicketKeys(first).ticketKeys());
    });
});
