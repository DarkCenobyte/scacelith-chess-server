import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { ControlPlane } from '../../src/cluster/control-plane.js';
import { Ipc, channelPair } from '../../src/cluster/ipc.js';
import { OnceStore, SlidingWindowLimiter } from '../../src/cluster/limits.js';
import { Presence } from '../../src/cluster/presence.js';
import { testConfig } from '../../src/config.js';
import { Challenges } from '../../src/match/challenges.js';
import { Registry } from '../../src/metrics.js';
import { decode, enums, messageName } from '../../src/protocol/index.js';
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
    async request(shard, type, payload) {
        this.requests.push({ shard, type, payload });
        if (type !== 'game.create') return null;
        if (this.create) return this.create(shard, payload);
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

function setup({ config = cfg, activeBan = null, conduct = null, ratingOf = null, acceptsChallenges = null, live } = {}) {
    let t = Date.UTC(2026, 5, 1, 12);
    const clock = { now: () => t, advance: (ms) => { t += ms; } };
    const shards = new FakeShards(live);
    const mm = new FakeMatchmaker();
    const ch = new Challenges({ config, now: clock.now, randomInt: () => 0 });
    const presence = new Presence({ maxConnections: config.maxConnections, maxPerIp: config.maxConnectionsPerIp });
    const cp = new ControlPlane({
        config, presence, matchmaker: mm, challenges: ch, conduct, limiter: new SlidingWindowLimiter({ now: clock.now }),
        once: new OnceStore({ now: clock.now }), shards, activeBan, ratingOf, acceptsChallenges, now: clock.now,
        random: () => 0.1, registry: new Registry(),
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
        assert.deepEqual([kick.shard, kick.payload.connId, kick.payload.closeCode, kick.payload.code], [0, 10, 4004, E.Banned]);
        assert.ok(shards.frames().some((f) => f.name === 'Notice' && f.msg.code === N.Banned && f.msg.arg === until));
        assert.deepEqual(shards.of('game.forfeit').map((x) => [x.shard, x.payload.userId, x.payload.gameId]), [[1, 1, gameId]]);
        assert.deepEqual(cp.presenceClaim({ userId: 1, username: 'alice', shard: 0, connId: 11 }, 0), { error: E.Banned, until });
        clock.advance(3600001);
        cp.sweep();
        assert.equal(cp.presenceClaim({ userId: 1, username: 'alice', shard: 0, connId: 11 }, 0).ok, true);
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
        assert.deepEqual(shards.of('auth.invalidate'), [{ shard: '*', type: 'auth.invalidate', payload: { userId: 1, tokenHashes: ['ab'] } }]);
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
});
