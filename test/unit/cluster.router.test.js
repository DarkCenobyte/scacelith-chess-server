import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import net from 'node:net';
import { afterEach, describe, it } from 'node:test';
import { BusKind, BusOp } from '../../src/cluster/bus.js';
import { Ipc, channelPair } from '../../src/cluster/ipc.js';
import { Router } from '../../src/cluster/router.js';
import { testConfig } from '../../src/config.js';
import { now as clockNow } from '../../src/game/clock.js';
import { Registry } from '../../src/metrics.js';
import { WsServer } from '../../src/net/ws.js';
import { connectWs } from '../../src/net/ws-raw-client.js';
import { MSG, PROTOCOL_VERSION, SCHEMA_HASH, decode, encode, enums, messageName } from '../../src/protocol/index.js';
import { GameIdAllocator } from '../../src/util/ids.js';

const E = enums.ErrorCode, N = enums.NoticeCode;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(pred, ms = 2000) {
    const t0 = Date.now();
    while (!pred()) {
        if (Date.now() - t0 > ms) throw new Error('condition not reached');
        await sleep(5);
    }
}

const TOKEN = 'a'.repeat(43);
const SESSIONS = {
    [TOKEN]: { userId: 1, username: 'alice', emailVerified: true, sessionId: 5 },
    ['b'.repeat(43)]: { userId: 2, username: 'bob', emailVerified: true },
    ['c'.repeat(43)]: { userId: 3, username: 'carl', emailVerified: false },
};

class FakeHost {
    constructor() { this.calls = []; this.games = 0; this.stalled = false; }
    createGame(spec) { this.calls.push(['createGame', spec]); return new GameIdAllocator(0).next(); }
    attach(gameId, userId, ep) { this.calls.push(['attach', gameId, userId, ep]); }
    detach(gameId, userId, ep) { this.calls.push(['detach', gameId, userId, ep]); }
    onClientMessage(gameId, userId, msg, ep) { this.calls.push(['msg', gameId, userId, msg, ep]); }
    onRtt(gameId, userId, ms) { this.calls.push(['rtt', gameId, userId, ms]); }
    relayGesture(gameId, userId, frame) { this.calls.push(['gesture', gameId, userId, Buffer.from(frame)]); return true; }
    stallDuring() { return this.stalled; }
    forfeitUser(userId) { this.calls.push(['forfeit', userId]); return true; }
    stats() { return { games: this.games }; }
    of(kind) { return this.calls.filter((c) => c[0] === kind); }
}

class FakeBus {
    constructor() { this.sent = []; this.queued = 0; this.maxQueueBytes = 8 << 20; }
    queuedBytes() { return this.queued; }
    send(peer, kind, connId, userId, gameId, payload) { this.sent.push({ peer, kind, connId, userId, gameId, payload: Buffer.from(payload) }); return true; }
    control(peer, op, connId, userId, gameId, extra = null) { this.sent.push({ peer, kind: BusKind.Control, op, connId, userId, gameId, extra }); return true; }
}

class Client {
    constructor(ws) { this.ws = ws; this.seq = 0; }
    send(name, fields = {}) { this.ws.send(encode[name]({ seq: ++this.seq, ...fields })); }
    hello(token = TOKEN, extra = {}) { this.seq = 1; this.ws.send(encode.Hello({ seq: 1, proto: PROTOCOL_VERSION, schema: SCHEMA_HASH, client: 'test/1', token, ...extra })); }
    async recv(ms = 2000) { const b = await this.ws.next(ms); return { name: messageName(b[0]), ...decode(b) }; }
    /** Next message of that name (others skipped). */
    async until(name, ms = 2000) { for (;;) { const m = await this.recv(ms); if (m.name === name) return m; } }
    async closed() { return (await this.ws.closed).code; }
}

const envs = [];
function setup({ config = {}, claim = null, host = new FakeHost(), bus = null, primaryHandlers = {}, store = null } = {}) {
    const cfg = testConfig({ WS_HELLO_TIMEOUT_MS: '1000', HEARTBEAT_INTERVAL_MS: '1000', HEARTBEAT_TIMEOUT_MS: '3000', REQUIRE_EMAIL_VERIFICATION: 'true', ...config });
    const [a, b] = channelPair();
    const primary = new Ipc(b);
    const seen = [];
    const record = (type, reply) => (p) => { seen.push({ type, p }); return typeof reply === 'function' ? reply(p) : reply; };
    primary.on('conn.ipAcquire', record('conn.ipAcquire', { ok: true }));
    primary.on('conn.ipRelease', record('conn.ipRelease', { ok: true }));
    primary.on('presence.claim', record('presence.claim', claim || { ok: true, activeGame: 0 }));
    primary.on('presence.release', record('presence.release', { ok: true }));
    primary.on('mm.join', record('mm.join', { ok: true }));
    primary.on('mm.leave', record('mm.leave', { ok: true }));
    primary.on('shard.load', record('shard.load', null));
    for (const [t, h] of Object.entries(primaryHandlers)) primary.on(t, record(t, h));
    const anomalies = [], sanctions = [], invalidated = [];
    const anticheat = { recordAnomaly: (x) => { anomalies.push(x); }, sanctionCertain: (x) => { sanctions.push(x); } };
    const auth = { validateToken: async (t) => SESSIONS[t] || null, invalidate: (p) => invalidated.push(p) };
    const registry = new Registry();
    const shardSide = new Ipc(a);
    const router = new Router({
        config: cfg, shard: 0, host, auth, primary: shardSide, bus, anticheat, store, registry, isShard: (s) => s < 4,
    });
    router.bindPrimary(shardSide);
    const wss = new WsServer({ registry, onConnection: router.onConnection, admission: router.admission, closeTimeoutMs: 300 });
    const srv = net.createServer((s) => wss.handleSocket(s));
    const env = { cfg, router, host, bus, primary, seen, anomalies, sanctions, invalidated, registry, wss, srv, port: 0 };
    env.connect = async (opts = {}) => new Client(await connectWs({ port: env.port, ...opts }));
    env.login = async (token = TOKEN) => { const c = await env.connect(); c.hello(token); const w = await c.until('Welcome'); return { c, w }; };
    env.conn = () => [...router.conns.values()].at(-1);
    envs.push(env);
    return new Promise((r) => srv.listen(0, '127.0.0.1', () => { env.port = srv.address().port; r(env); }));
}
afterEach(() => { for (const e of envs.splice(0)) { e.wss.closeAll(); e.srv.close(); e.router.stop(); } });

describe('router: hello', () => {
    it('welcomes a valid token, claims presence, releases on close', async () => {
        const env = await setup();
        const { c, w } = await env.login();
        assert.equal(w.userId, 1);
        assert.equal(w.username, 'alice');
        assert.equal(w.serverName, env.cfg.serverName);
        assert.equal(w.heartbeatMs, 1000);
        assert.equal(w.clientPingMs, 10000);          // CLIENT_PING_INTERVAL_MS default
        assert.equal(w.activeGame, 0);
        const claim = env.seen.find((x) => x.type === 'presence.claim').p;
        assert.deepEqual([claim.userId, claim.username, claim.shard], [1, 'alice', 0]);
        assert.ok(env.seen.some((x) => x.type === 'conn.ipAcquire' && x.p.ip === '127.0.0.1'));
        c.ws.close(1000);
        await waitFor(() => env.seen.some((x) => x.type === 'presence.release'));
        assert.deepEqual(env.seen.find((x) => x.type === 'presence.release').p, { userId: 1, connId: claim.connId });
        await waitFor(() => env.seen.some((x) => x.type === 'conn.ipRelease'));
    });

    it('announces CLIENT_PING_INTERVAL_MS in Welcome (1 s to 60 s)', async () => {
        const env = await setup({ config: { CLIENT_PING_INTERVAL_MS: '25000' } });
        const { c, w } = await env.login();
        assert.equal(w.clientPingMs, 25000);
        assert.equal(w.heartbeatMs, 1000);
        c.ws.close(1000);
        assert.equal(testConfig({ CLIENT_PING_INTERVAL_MS: '1000' }).clientPingIntervalMs, 1000);
        assert.equal(testConfig({ CLIENT_PING_INTERVAL_MS: '60000' }).clientPingIntervalMs, 60000);
        assert.throws(() => testConfig({ CLIENT_PING_INTERVAL_MS: '999' }), /CLIENT_PING_INTERVAL_MS: at least 1000/);
        assert.throws(() => testConfig({ CLIENT_PING_INTERVAL_MS: '60001' }), /CLIENT_PING_INTERVAL_MS: at most 60000/);
    });

    it('refuses an upgrade the primary refuses (429 per IP, 503 when full)', async () => {
        const env = await setup();
        env.primary.on('conn.ipAcquire', () => ({ ok: false, reason: 'per_ip' }));
        await assert.rejects(env.connect(), (e) => e.status === 429);
        env.primary.on('conn.ipAcquire', () => ({ ok: false, reason: 'global' }));
        await assert.rejects(env.connect(), (e) => e.status === 503);
        assert.equal(env.router.conns.size, 0);
    });

    it('replays messages pipelined behind Hello after the Welcome', async () => {
        let release;
        const gate = new Promise((r) => { release = r; });
        const env = await setup({ claim: async () => { await gate; return { ok: true, activeGame: 0 }; } });
        const c = await env.connect();
        c.hello();
        c.send('QueueJoin', { category: '5+0', rated: true });
        c.send('QueueLeave');
        await sleep(30);
        release();
        assert.equal((await c.recv()).name, 'Welcome');
        const acks = [await c.recv(), await c.recv()];
        assert.deepEqual(acks.map((m) => [m.name, m.ref]), [['Ack', 2], ['Ack', 3]]);
    });

    const refusals = [
        ['a first message other than Hello', (c) => c.send('QueueLeave'), E.HelloRequired, 4010],
        ['a Hello with seq 2', (c) => c.ws.send(encode.Hello({ seq: 2, proto: PROTOCOL_VERSION, schema: SCHEMA_HASH, client: 't', token: TOKEN })), E.ProtocolViolation, 4300],
        ['a truncated Hello', (c) => c.ws.send(encode.Hello({ seq: 1, proto: PROTOCOL_VERSION, schema: SCHEMA_HASH, client: 't', token: TOKEN }).subarray(0, 12)), E.Malformed, 4300],
        ['an unsupported protocol version', (c) => c.hello(TOKEN, { proto: PROTOCOL_VERSION + 1 }), E.UnsupportedProtocol, 4002],
        ['a schema mismatch', (c) => c.hello(TOKEN, { schema: (SCHEMA_HASH + 1) >>> 0 }), E.UnsupportedProtocol, 4002],
        ['an unknown token', (c) => c.hello('z'.repeat(43)), E.Unauthorized, 4003],
        ['an unverified e-mail address', (c) => c.hello('c'.repeat(43)), E.EmailUnverified, 4003],
    ];
    for (const [what, act, code, closeCode] of refusals) {
        it(`closes on ${what}`, async () => {
            const env = await setup();
            const c = await env.connect();
            act(c);
            const m = await c.recv();
            assert.deepEqual([m.name, m.code, m.fatal], ['Error', code, true]);
            assert.equal(await c.closed(), closeCode);
            assert.ok(!env.seen.some((x) => x.type === 'presence.claim'));
        });
    }

    it('closes a banned session with the ban end (from the primary)', async () => {
        const env = await setup({ claim: { error: E.Banned, until: Date.UTC(2099, 0, 1) } });
        const c = await env.connect();
        c.hello();
        assert.equal((await c.recv()).code, E.Banned);
        const n = await c.recv();
        assert.deepEqual([n.name, n.code, n.arg], ['Notice', N.Banned, Date.UTC(2099, 0, 1)]);
        assert.equal(await c.closed(), 4004);
    });

    it('closes 1011 and releases the claim when the Welcome cannot be encoded', async () => {
        // A server name over the 64 bytes of Welcome.serverName (loadConfig refuses it since).
        const env = await setup();
        env.router.config = { ...env.cfg, serverName: 'é'.repeat(40) };
        const c = await env.connect();
        c.hello();
        const m = await c.recv();
        assert.deepEqual([m.name, m.code, m.fatal], ['Error', E.Internal, true]);
        assert.equal(await c.closed(), 1011);
        await waitFor(() => env.seen.some((x) => x.type === 'presence.release'));
        assert.equal(env.router.byUser.size, 0);
    });

    it('closes 4006 when the server is full', async () => {
        const env = await setup({ claim: { error: 'ServerFull' } });
        const c = await env.connect();
        c.hello();
        assert.equal((await c.recv()).code, E.ServerFull);
        assert.equal(await c.closed(), 4006);
    });

    it('closes a silent connection at the hello deadline', async () => {
        const env = await setup();
        const c = await env.connect();
        await waitFor(() => env.router.conns.size === 1);
        env.router.sweep(clockNow() + 500);
        await sleep(20);
        assert.equal(env.router.conns.size, 1);
        env.router.sweep(clockNow() + 1100);
        const m = await c.recv();
        assert.deepEqual([m.name, m.code], ['Error', E.HelloRequired]);
        assert.equal(await c.closed(), 4010);
    });

    it('attaches the active game given by the primary', async () => {
        const gameId = new GameIdAllocator(0).next();
        const env = await setup({ claim: { ok: true, activeGame: gameId } });
        const { w } = await env.login();
        assert.equal(w.activeGame, gameId);
        const [, g, u, ep] = env.host.of('attach')[0];
        assert.deepEqual([g, u, ep.connId, ep.shard, ep.local, ep.userId], [gameId, 1, env.conn().id, 0, true, 1]);
    });
});

describe('router: ready connections', () => {
    it('drops a bad seq (anomaly once) and resynchronises on the new one', async () => {
        const env = await setup();
        const { c } = await env.login();
        c.seq = 9;
        c.send('QueueLeave');                    // seq 10 instead of 2: dropped
        c.send('QueueLeave');                    // 11: accepted
        const ack = await c.until('Ack');
        assert.equal(ack.ref, 11);
        c.seq = 20;
        c.send('QueueLeave');
        c.seq = 21;
        c.send('QueueLeave');                    // 22 after 21: accepted
        assert.equal((await c.until('Ack')).ref, 22);
        assert.deepEqual(env.anomalies.map((a) => [a.kind, a.detail.expected, a.detail.got]), [['bad_seq', 2, 10]]);
        assert.equal(env.seen.filter((x) => x.type === 'mm.leave').length, 2);
    });

    it('rate limits (one RateLimited error) and closes a flood 4301', async () => {
        const env = await setup({ config: { WS_MSG_RATE: '1', WS_MSG_BURST: '3' } });
        const { c } = await env.login();
        for (let i = 0; i < 5; i++) c.send('QueueLeave');
        const got = [];
        for (let i = 0; i < 4; i++) got.push(await c.recv());
        assert.deepEqual(got.map((m) => m.name).sort(), ['Ack', 'Ack', 'Ack', 'Error']);
        assert.equal(got.find((m) => m.name === 'Error').code, E.RateLimited);
        for (let i = 0; i < 10; i++) c.send('QueueLeave');
        const m = await c.until('Error');
        assert.deepEqual([m.code, m.fatal], [E.Flood, true]);
        assert.equal(await c.closed(), 4301);
        assert.ok(env.anomalies.some((a) => a.kind === 'flood'));
    });

    it('treats a server-to-client type as a certain cheat: sanction, forfeit, 4302', async () => {
        const gameId = new GameIdAllocator(0).next();
        const env = await setup({ claim: { ok: true, activeGame: gameId } });
        const { c } = await env.login();
        c.ws.send(Buffer.from([MSG.Welcome, 0, 0, 0, 0]));
        const m = await c.until('Error');
        assert.deepEqual([m.code, m.fatal], [E.CheatDetected, true]);
        assert.equal(await c.closed(), 4302);
        assert.deepEqual(env.anomalies.map((a) => [a.kind, a.gameId]), [['forged_type', gameId]]);
        assert.deepEqual(env.sanctions.map((s) => [s.userId, s.gameId, s.kind]), [[1, gameId, 'forged_type']]);
        assert.deepEqual(env.host.of('forfeit'), [['forfeit', 1]]);
    });

    it('closes 4300 on a forged type when automatic sanctions are off, and on malformed messages', async () => {
        const env = await setup({ config: { AUTO_SANCTION_CERTAIN_CHEATS: 'false' } });
        const { c } = await env.login();
        c.ws.send(Buffer.from([MSG.Ack, 1, 0, 0, 0]));
        assert.equal((await c.until('Error')).code, E.ProtocolViolation);
        assert.equal(await c.closed(), 4300);
        assert.equal(env.sanctions.length, 0);
        const { c: c2 } = await env.login('b'.repeat(43));
        c2.ws.send(Buffer.concat([encode.QueueLeave({ seq: 2 }), Buffer.from([0])]));   // trailing byte
        assert.equal((await c2.until('Error')).code, E.Malformed);
        assert.equal(await c2.closed(), 4300);
        assert.ok(env.anomalies.some((a) => a.kind === 'malformed' && a.userId === 2));
    });

    it('answers client pings once a second, pings with the heartbeat and measures the RTT', async () => {
        const gameId = new GameIdAllocator(0).next();
        const env = await setup({ claim: { ok: true, activeGame: gameId } });
        const { c } = await env.login();
        c.send('C_Ping', { nonce: 77 });
        c.send('C_Ping', { nonce: 78 });                         // too soon: dropped
        const pong = await c.recv();
        assert.deepEqual([pong.name, pong.nonce], ['S_Pong', 77]);
        env.router.sweep(clockNow() + 1000);
        const ping = await c.recv();
        assert.equal(ping.name, 'S_Ping');
        await sleep(15);
        c.send('C_Pong', { nonce: ping.nonce });
        await waitFor(() => env.host.of('rtt').length === 1);
        const [, g, u, ms] = env.host.of('rtt')[0];
        assert.deepEqual([g, u], [gameId, 1]);
        assert.ok(ms >= 10 && ms < 1000, `rtt ${ms}`);
        assert.equal(env.host.of('attach')[0][3].rttMs, env.conn().rttMs);
        await assert.rejects(c.recv(100));                       // no second Pong
    });

    it('closes a connection silent past the heartbeat timeout with 1001', async () => {
        const env = await setup();
        const { c } = await env.login();
        env.router.sweep(clockNow() + 3100);
        assert.equal(await c.closed(), 1001);
    });

    it('keeps its heartbeat when the wall clock steps', async () => {
        const env = await setup();
        const { c } = await env.login();
        const wall = Date.now;
        try {
            // Forward by more than HEARTBEAT_TIMEOUT_MS (3 s here): nobody is silent that long.
            Date.now = () => wall() + 3100;
            env.router.sweep();
            await sleep(20);
            assert.equal(env.router.conns.size, 1, 'the connection stays open');
            // Back by a minute (an NTP step, a resumed VM): the pings go on, every half interval.
            Date.now = () => wall() - 60000;
            await sleep(550);
            env.router.sweep();
            assert.equal((await c.until('S_Ping')).name, 'S_Ping');
        } finally {
            Date.now = wall;
        }
    });

    it('queues with the stored rating, closes the rematch window, and maps refusals', async () => {
        const lastGame = new GameIdAllocator(0).next();
        // 3+2: rated after five counted games, then games against unrated opponents only.
        const stored = { '5+0': { rating: 1777, games: 50 }, '3+2': { rating: 1816, games: 58, countedGames: 5, rated: true } };
        const store = { ratings: { get: (u, cat) => stored[cat] ?? null } };
        const env = await setup({ store, claim: { ok: true, activeGame: lastGame } });
        const { c } = await env.login();
        c.send('QueueJoin', { category: '5+0', rated: true });
        assert.equal((await c.until('Ack')).ref, 2);
        const j = env.seen.find((x) => x.type === 'mm.join').p;
        assert.deepEqual([j.userId, j.username, j.category, j.rated, j.rating, j.provisional, j.shard, j.connId], [1, 'alice', '5+0', true, 1777, false, 0, env.conn().id]);
        const decline = env.host.of('msg')[0];
        assert.deepEqual([decline[1], decline[2], decline[3].type, decline[3].accept, decline[4]], [lastGame, 1, MSG.Rematch, false, null]);
        c.send('QueueJoin', { category: '3+2', rated: true });
        assert.equal((await c.until('Ack')).ref, 3);
        const j2 = env.seen.filter((x) => x.type === 'mm.join')[1].p;
        assert.deepEqual([j2.rating, j2.provisional], [1816, true], 'provisional by the counted games, not the games played');
        c.send('QueueJoin', { category: '9+9', rated: true });
        const bad = await c.until('Error');
        assert.deepEqual([bad.ref, bad.code, bad.fatal], [4, E.InvalidCategory, false]);
        env.primary.on('mm.join', () => ({ error: 'AlreadyInGame' }));
        c.send('QueueJoin', { category: '5+0', rated: false });
        const busy = await c.until('Error');
        assert.deepEqual([busy.ref, busy.code], [5, E.AlreadyInGame]);
    });

    it('routes game messages to the local host, over the bus, or refuses them', async () => {
        const bus = new FakeBus();
        const env = await setup({ bus });
        const { c } = await env.login();
        const local = new GameIdAllocator(0).next(), remote = new GameIdAllocator(2).next(), foreign = new GameIdAllocator(9).next();
        c.send('Resign', { game: local });
        await waitFor(() => env.host.of('msg').length === 1);
        const [, g, u, msg, ep] = env.host.of('msg')[0];
        assert.deepEqual([g, u, msg.type, msg.seq, ep.connId], [local, 1, MSG.Resign, 2, env.conn().id]);
        c.send('Move', { game: remote, ply: 0, move: 796, posHash: 1, thinkMs: 5, drawOffer: false });
        await waitFor(() => bus.sent.length === 1);
        const f = bus.sent[0];
        assert.deepEqual([f.peer, f.kind, f.connId, f.userId, f.gameId], [2, BusKind.ToHost, env.conn().id, 1, remote]);
        assert.equal(decode(f.payload).move, 796);
        c.send('Resign', { game: foreign });
        const e = await c.until('Error');
        assert.deepEqual([e.code, e.game], [E.NotInGame, foreign]);
    });
});

describe('router: primary and bus', () => {
    it('serves game.create, conn.send, conn.kick and game.forfeit from the primary', async () => {
        const env = await setup();
        const { c } = await env.login();
        const r = await env.primary.request('game.create', { spec: { category: '5+0' } });
        assert.equal(r.ok, true);
        assert.equal(env.host.of('createGame')[0][1].category, '5+0');
        assert.deepEqual(await env.primary.request('game.forfeit', { userId: 1, gameId: r.gameId }), { ok: true });
        const id = env.conn().id;
        assert.deepEqual(await env.primary.request('conn.send', { connId: id, frames: [encode.Notice({ code: N.Motd, arg: 1 })] }), { ok: true });
        assert.equal((await c.recv()).code, N.Motd);
        assert.deepEqual(await env.primary.request('game.attach', { gameId: r.gameId, userId: 1, connId: id }), { ok: true });
        assert.deepEqual(await env.primary.request('game.attach', { gameId: r.gameId, userId: 2, connId: id }), { ok: false });
        env.primary.notify('conn.kick', { connId: id, closeCode: 4007, frames: [encode.Notice({ code: N.ReplacedByNewConnection, arg: 0 })] });
        assert.equal((await c.recv()).code, N.ReplacedByNewConnection);
        assert.equal(await c.closed(), 4007);
        await waitFor(() => env.host.of('detach').length === 1);
    });

    it('answers conn.send with ok: false when a frame is refused as a slow consumer', async () => {
        const env = await setup();
        const { c } = await env.login();
        const id = env.conn().id;
        env.wss.sendBufferLimit = 4;                                    // any frame now overflows the send buffer
        const notice = encode.Notice({ code: N.RatingRestored, arg: 12 });
        assert.deepEqual(await env.primary.request('conn.send', { connId: id, frames: [notice] }), { ok: false });
        assert.equal(await c.closed(), 4303);
        assert.deepEqual(await env.primary.request('conn.send', { connId: id, frames: [notice] }), { ok: false });
    });

    it('closes the connections of a revoked token only', async () => {
        const env = await setup();
        const { c } = await env.login();
        const other = crypto.createHash('sha256').update('x'.repeat(43)).digest('hex');
        assert.deepEqual(await env.primary.request('auth.invalidate', { userId: 1, tokenHashes: [other] }), { ok: true, closed: 0 });
        const mine = crypto.createHash('sha256').update(TOKEN).digest('base64url');
        assert.deepEqual(await env.primary.request('auth.invalidate', { userId: 1, tokenHashes: [mine] }), { ok: true, closed: 1 });
        assert.equal((await c.recv()).code, N.SessionRevoked);
        assert.equal((await c.recv()).code, E.Unauthorized);
        assert.equal(await c.closed(), 4003);
        assert.equal(env.invalidated.length, 2);
    });

    it('hosts remote players: attach, relay both ways, RTT, close, detach, shard down', async () => {
        const bus = new FakeBus();
        const env = await setup({ bus });
        const gameId = new GameIdAllocator(0).next();
        const rtt = Buffer.alloc(3); rtt[0] = BusOp.Rtt; rtt.writeUInt16LE(123, 1);
        env.router.onBus(BusKind.Control, 2, 55, 7, gameId, Buffer.from([BusOp.Attach]));
        const [, g, u, ep] = env.host.of('attach')[0];
        assert.deepEqual([g, u, ep.shard, ep.connId, ep.local], [gameId, 7, 2, 55, false]);
        env.router.onBus(BusKind.Control, 2, 55, 7, gameId, rtt);
        assert.equal(ep.rttMs, 123);
        assert.deepEqual(env.host.of('rtt')[0].slice(1), [gameId, 7, 123]);
        env.router.onBus(BusKind.ToHost, 2, 55, 7, gameId, encode.Resign({ seq: 9, game: gameId }));
        const m = env.host.of('msg')[0];
        assert.deepEqual([m[1], m[2], m[3].type, m[4]], [gameId, 7, MSG.Resign, ep]);
        ep.send(Buffer.from([1, 2]));
        ep.close(4302);
        assert.deepEqual(bus.sent.map((f) => [f.peer, f.kind, f.connId, f.userId]), [[2, BusKind.ToConn, 55, 7], [2, BusKind.Control, 55, 7]]);
        assert.equal(bus.sent[1].op, BusOp.Close);
        assert.equal(bus.sent[1].extra.readUInt16LE(0), 4302);
        env.router.onBus(BusKind.Control, 2, 55, 7, gameId, Buffer.from([BusOp.Forfeit]));
        assert.deepEqual(env.host.of('forfeit'), [['forfeit', 7]]);
        env.router.onBus(BusKind.Control, 2, 55, 7, gameId, Buffer.from([BusOp.RematchDecline]));
        assert.equal(env.host.of('msg')[1][3].accept, false);
        assert.deepEqual(env.router.forgetShard(2), { ok: true, detached: 1 });
        assert.equal(env.router.remote.size, 0);
    });

    it('delivers bus frames to its own connections and closes them on request', async () => {
        const bus = new FakeBus();
        const env = await setup({ bus });
        const { c } = await env.login();
        const id = env.conn().id;
        env.router.onBus(BusKind.ToConn, 2, id, 1, 0, encode.Notice({ code: N.Motd, arg: 3 }));
        env.router.onBus(BusKind.ToConn, 2, id, 99, 0, encode.Notice({ code: N.Motd, arg: 4 }));   // wrong user: dropped
        assert.equal((await c.recv()).arg, 3);
        const close = Buffer.alloc(3); close[0] = BusOp.Close; close.writeUInt16LE(4302, 1);
        env.router.onBus(BusKind.Control, 2, id, 1, 0, close);
        assert.equal(await c.closed(), 4302);
    });

    it('drains: ServerShutdown notice, then ShuttingDown and 4008', async () => {
        const env = await setup();
        const { c } = await env.login();
        const d = env.router.drain(50);
        const n = await c.recv();
        assert.deepEqual([n.name, n.code, n.arg], ['Notice', N.ServerShutdown, 50]);
        await d;
        assert.equal((await c.recv()).code, E.ShuttingDown);
        assert.equal(await c.closed(), 4008);
        const late = await env.connect();
        assert.equal((await late.recv()).code, E.ShuttingDown);
        assert.equal(await late.closed(), 4008);
    });

    it('reports its load to the primary', async () => {
        const env = await setup();
        env.host.games = 3;
        env.router.lagP99 = () => 300;
        env.router._reportLoad();
        await waitFor(() => env.seen.some((x) => x.type === 'shard.load'));
        const l = env.seen.find((x) => x.type === 'shard.load').p;
        assert.deepEqual([l.shard, l.games, l.lagP99, l.overloaded], [0, 3, 300, true]);
    });
});

describe('router: gestures', () => {
    const G = { ply: 3, touch: 12, aim: 28, placed: 0, flags: 1, yaw: -700, pitch: 250, lean: 40 };
    const value = (env, name, ...labels) => env.registry.metrics.get(name)?.children.get(labels.join('\u0001'))?.value ?? 0;
    const dropped = (env, reason) => value(env, 'scacelith_gestures_dropped_total', reason);
    const inGame = async (config = {}) => {
        const gameId = new GameIdAllocator(0).next();
        const env = await setup({ config, claim: { ok: true, activeGame: gameId } });
        const { c, w } = await env.login();
        return { env, c, w, gameId };
    };

    it('announces the relay in Welcome with the server clock (0/0 when GESTURE_RATE is 0)', async () => {
        const { env, w } = await inGame();
        assert.deepEqual([w.gestureRate, w.gestureBurst], [4, 8]);       // GESTURE_RATE, GESTURE_BURST defaults
        assert.ok(Math.abs(w.serverTime - clockNow()) < 1000, 'Welcome.serverTime is the clock of the game hosts');
        assert.equal(env.cfg.gestureRate, 4);
        const off = await setup({ config: { GESTURE_RATE: '0', GESTURE_BURST: '20' } });
        const { w: w0 } = await off.login();
        assert.deepEqual([w0.gestureRate, w0.gestureBurst], [0, 0]);
        assert.throws(() => testConfig({ GESTURE_RATE: '61' }), /GESTURE_RATE: at most 60/);
        assert.throws(() => testConfig({ GESTURE_BURST: '0' }), /GESTURE_BURST: at least 1/);
    });

    it('relays the raw frame to the local host, drops the excess silently and keeps the seq in step', async () => {
        const { env, c, gameId } = await inGame({ GESTURE_RATE: '1', GESTURE_BURST: '3' });
        const frames = [];
        for (let i = 0; i < 6; i++) { frames.push(encode.C_Gesture({ seq: ++c.seq, game: gameId, ...G, yaw: i })); c.ws.send(frames[i]); }
        c.send('Resign', { game: gameId });
        await waitFor(() => env.host.of('msg').length === 1);
        const got = env.host.of('gesture');
        assert.deepEqual(got.map((x) => [x[1], x[2]]), [[gameId, 1], [gameId, 1], [gameId, 1]]);
        assert.ok(got.every((x, i) => x[3].equals(frames[i])), 'the burst is relayed byte for byte');
        assert.equal(env.host.of('msg')[0][3].seq, 8);                   // accepted after 3 dropped gestures
        assert.equal(dropped(env, 'rate'), 3);
        assert.deepEqual(env.anomalies, []);
        await assert.rejects(c.recv(150));                               // no RateLimited, no Error
        assert.equal(env.conn().state, 'ready');
    });

    it('spends no WS_MSG token: requests stay within their own bucket', async () => {
        const { env, c, gameId } = await inGame({ WS_MSG_RATE: '1', WS_MSG_BURST: '3', GESTURE_RATE: '60', GESTURE_BURST: '120' });
        for (let i = 0; i < 40; i++) c.send('C_Gesture', { game: gameId, ...G });
        for (let i = 0; i < 3; i++) c.send('QueueLeave');
        const acks = [await c.recv(), await c.recv(), await c.recv()];
        assert.deepEqual(acks.map((m) => [m.name, m.ref]), [['Ack', 42], ['Ack', 43], ['Ack', 44]]);
        assert.equal(env.host.of('gesture').length, 40);
        await assert.rejects(c.recv(150));
    });

    it('drops every gesture when GESTURE_RATE is 0, and a gesture for a game not attached to the connection', async () => {
        const { env, c, gameId } = await inGame({ GESTURE_RATE: '0' });
        for (let i = 0; i < 5; i++) c.send('C_Gesture', { game: gameId, ...G });
        c.send('QueueLeave');
        assert.equal((await c.until('Ack')).ref, 7);
        assert.equal(dropped(env, 'rate'), 5);
        const { env: env2, c: c2 } = await inGame();
        c2.send('C_Gesture', { game: new GameIdAllocator(1).next(), ...G });
        c2.send('QueueLeave');
        assert.equal((await c2.until('Ack')).ref, 3);
        assert.equal(dropped(env2, 'not_attached'), 1);
        assert.equal(env.host.of('gesture').length + env2.host.of('gesture').length, 0);
        assert.deepEqual([...env.anomalies, ...env2.anomalies], []);
    });

    it('closes a gross flood 4301 and a malformed gesture 4300', async () => {
        const { env, c, gameId } = await inGame({ GESTURE_RATE: '1', GESTURE_BURST: '2' });
        for (let i = 0; i < 60; i++) c.send('C_Gesture', { game: gameId, ...G });   // max(50, 10 x 2) drops allowed
        const m = await c.until('Error');
        assert.deepEqual([m.code, m.fatal], [E.Flood, true]);
        assert.equal(await c.closed(), 4301);
        assert.deepEqual(env.anomalies.map((a) => [a.kind, a.detail.gestureDrops]), [['flood', 51]]);
        assert.equal(env.host.of('gesture').length, 2);
        const { env: env2, c: c2, gameId: g2 } = await inGame();
        const bad = encode.C_Gesture({ seq: 2, game: g2, ...G });
        bad[bad.length - 10] = 0xff;                                     // flags above the GestureFlag bits
        c2.ws.send(bad);
        assert.equal((await c2.until('Error')).code, E.Malformed);
        assert.equal(await c2.closed(), 4300);
        assert.ok(env2.anomalies.some((a) => a.kind === 'malformed'));
    });

    it('forwards to the host shard over the bus, unless its link holds a backlog', async () => {
        const bus = new FakeBus();
        const env = await setup({ bus });
        const { c } = await env.login();
        const remote = new GameIdAllocator(2).next();
        assert.deepEqual(await env.primary.request('game.attach', { gameId: remote, userId: 1, connId: env.conn().id }), { ok: true });
        const f = encode.C_Gesture({ seq: ++c.seq, game: remote, ...G });
        c.ws.send(f);
        await waitFor(() => bus.sent.some((x) => x.kind === BusKind.ToHost));
        const out = bus.sent.find((x) => x.kind === BusKind.ToHost);
        assert.deepEqual([out.peer, out.connId, out.userId, out.gameId], [2, env.conn().id, 1, remote]);
        assert.ok(out.payload.equals(f), 'the raw client frame');
        bus.queued = bus.maxQueueBytes / 4 + 1;
        c.send('C_Gesture', { game: remote, ...G });
        c.send('QueueLeave');
        await c.until('Ack');
        assert.equal(bus.sent.filter((x) => x.kind === BusKind.ToHost).length, 1);
        assert.equal(dropped(env, 'backlog'), 1);
    });

    it('relays a gesture from another shard without decoding it, and sends gestures as droppable frames', async () => {
        const bus = new FakeBus();
        const env = await setup({ bus });
        const { c } = await env.login();
        const gameId = new GameIdAllocator(0).next();
        const raw = encode.C_Gesture({ seq: 77, game: gameId, ...G });
        env.router.onBus(BusKind.ToHost, 2, 55, 7, gameId, raw);
        assert.equal(env.host.of('msg').length, 0);
        const [, g, u, frame] = env.host.of('gesture')[0];
        assert.deepEqual([g, u], [gameId, 7]);
        assert.ok(frame.equals(raw));
        // Towards a connection of this shard: skipped while it holds a quarter of WS_SEND_BUFFER_LIMIT.
        const conn = env.conn();
        const out = encode.S_Gesture({ game: gameId, ...G });
        assert.equal(conn.ctx.endpoint.sendDroppable(out), true);
        assert.equal((await c.recv()).name, 'S_Gesture');
        Object.defineProperty(conn, 'bufferedBytes', { value: env.cfg.wsSendBufferLimit / 4 + 1, configurable: true });
        assert.equal(conn.ctx.endpoint.sendDroppable(out), false);
        env.router.onBus(BusKind.ToConn, 2, conn.id, 1, 0, out);
        env.router.onBus(BusKind.ToConn, 2, conn.id, 1, 0, encode.Notice({ code: N.Motd, arg: 5 }));
        const next = await c.recv();
        assert.deepEqual([next.name, next.arg], ['Notice', 5]);         // the gesture was dropped, the notice was not
        assert.equal(dropped(env, 'backlog'), 1);
        // Towards another shard: skipped while the bus link holds a quarter of its queue.
        env.router.onBus(BusKind.Control, 2, 55, 7, gameId, Buffer.from([BusOp.Attach]));
        const ep = env.host.of('attach').at(-1)[3];
        assert.equal(ep.sendDroppable(out), true);
        bus.queued = bus.maxQueueBytes / 4 + 1;
        assert.equal(ep.sendDroppable(out), false);
        assert.equal(bus.sent.filter((x) => x.kind === BusKind.ToConn).length, 1);
    });

    it('leaves out of the round-trip average a Pong whose Ping preceded a stall', async () => {
        const gameId = new GameIdAllocator(0).next();
        const env = await setup({ claim: { ok: true, activeGame: gameId } });
        const { c } = await env.login();
        env.host.stalled = true;
        env.router.sweep(clockNow() + 1000);
        const p1 = await c.until('S_Ping');
        assert.ok(Math.abs(p1.serverTime - clockNow()) < 1000);
        c.send('C_Pong', { nonce: p1.nonce });
        c.send('C_Ping', { nonce: 5 });
        const pong = await c.until('S_Pong');
        assert.ok(Math.abs(pong.serverTime - clockNow()) < 1000);
        assert.equal(env.host.of('rtt').length, 0);
        env.host.stalled = false;
        env.router.sweep(clockNow() + 2000);
        const p2 = await c.until('S_Ping');
        c.send('C_Pong', { nonce: p2.nonce });
        await waitFor(() => env.host.of('rtt').length === 1);
    });
});

describe('router: token buckets over time', () => {
    // A Router alone with one fake ready connection in a game of this shard, fed messages at
    // chosen clock.js times (or at the default one), as the socket handler would read them.
    function bucketRig(env = {}) {
        const cfg = testConfig(env);
        const host = new FakeHost(), anomalies = [], requests = [];
        const primary = { request: async (type) => { requests.push(type); return { ok: true }; }, notify() {} };
        const router = new Router({
            config: cfg, shard: 0, host, auth: { validateToken: async () => null }, primary, registry: new Registry(),
            anticheat: { recordAnomaly: (x) => { anomalies.push(x); } },
        });
        const conn = {
            id: 1, state: 'ready', userId: 1, openedAt: clockNow(), lastRecvAt: clockNow(), frames: [], closedWith: 0,
            sendFrame(buf) { this.frames.push(buf); return true; },
            close(code) { this.closedWith = code; this.state = 'closed'; },
        };
        router._onConnection(conn);
        const gameId = new GameIdAllocator(0).next();
        assert.deepEqual(router.attach(gameId, 1, conn.id), { ok: true });
        let seq = 0;
        const send = (name, fields, ...now) => router._onMessage(conn, encode[name]({ seq: ++seq, ...fields }), ...now);
        const gesture = (...now) => send('C_Gesture', { game: gameId, ply: 0, touch: 64, aim: 64, placed: 0, flags: 0, yaw: 0, pitch: 0, lean: 0 }, ...now);
        const relayed = () => host.of('gesture').length;
        const errors = () => conn.frames.filter((f) => f[0] === MSG.Error).map((f) => decode(f).code);
        return { cfg, router, conn, anomalies, requests, send, gesture, relayed, errors };
    }

    // A client pacing its gestures at `rate` for `ms` from `t`, read as they come; returns the end.
    function paced(rig, rate, t, ms) {
        for (let i = 0; i < ms * rate / 1000; i++) { t += 1000 / rate; rig.gesture(t); }
        return t;
    }
    // The same client while nothing is read for `ms`: its gestures all arrive at the end.
    function stalled(rig, rate, t, ms) {
        const n = ms * rate / 1000;
        t += ms;
        for (let i = 0; i < n; i++) rig.gesture(t);
        return t;
    }

    it('lets a client pacing its gestures at the rate live through a stall that delivers them in one burst', () => {
        for (const [rate, stallMs] of [[4, 35000], [20, 5000], [20, 38000], [60, 2500], [60, 40000]]) {
            const rig = bucketRig({ GESTURE_RATE: String(rate) });
            const burst = rig.cfg.gestureBurst;
            let t = paced(rig, rate, clockNow(), 10000);
            assert.equal(rig.relayed(), 10 * rate, `rate ${rate}: nothing dropped while paced`);
            t = stalled(rig, rate, t, stallMs);
            assert.equal(rig.relayed(), 10 * rate + burst, `rate ${rate}: one bucket of the burst relayed`);
            paced(rig, rate, t, 20000);
            const what = `rate ${rate}, ${stallMs} ms stall`;
            assert.deepEqual([rig.conn.closedWith, rig.anomalies], [0, []], what);
            assert.equal(rig.relayed(), 30 * rate + burst, `${what}: the relay goes on at the rate`);
        }
    });

    it('still closes a gross gesture flood 4301, whatever the rate', () => {
        for (const rate of [4, 20, 60]) {
            const rig = bucketRig({ GESTURE_RATE: String(rate) });
            const t = paced(rig, rate, clockNow(), 5000);
            const limit = Math.max(50, 10 * rig.cfg.gestureBurst, Math.ceil(rate * (30000 + 10000 + 250) / 1000));
            for (let i = 0; i < 2 * limit && !rig.conn.closedWith; i++) rig.gesture(t + 100);
            assert.equal(rig.conn.closedWith, 4301, `rate ${rate}`);
            assert.deepEqual(rig.errors(), [E.Flood]);
            assert.deepEqual(rig.anomalies.map((a) => [a.kind, a.detail.gestureDrops]), [['flood', limit + 1]], `rate ${rate}`);
        }
    });

    it('keeps its buckets when the wall clock steps back', () => {
        const rig = bucketRig();
        rig.gesture();
        rig.send('QueueLeave', {});
        const wall = Date.now;
        Date.now = () => wall() - 60000;                                // an NTP step, a resumed VM
        try {
            for (let i = 0; i < 3; i++) rig.gesture();
            rig.send('QueueLeave', {});
        } finally {
            Date.now = wall;
        }
        assert.equal(rig.relayed(), 4, 'the gestures are relayed');
        assert.deepEqual(rig.requests, ['mm.leave', 'mm.leave'], 'the request is not rate limited');
        assert.deepEqual([rig.errors(), rig.conn.closedWith, rig.anomalies], [[], 0, []]);
    });
});

describe('router: heartbeat sweep', () => {
    // A Router alone, driven tick by tick on a simulated clock, with fake ready connections that
    // answer every S_Ping at once (unless `silent`).
    function sweepRig({ n, heartbeatMs, tickMs = 250, silent = () => false }) {
        const cfg = testConfig({ HEARTBEAT_INTERVAL_MS: String(heartbeatMs), HEARTBEAT_TIMEOUT_MS: String(heartbeatMs * 3) });
        const primary = { request: async () => ({ ok: true }), notify() {} };
        const router = new Router({
            config: cfg, shard: 0, host: new FakeHost(), auth: { validateToken: async () => null }, primary, registry: new Registry(), tickMs,
        });
        const t0 = clockNow();
        const conns = [];
        for (let i = 1; i <= n; i++) {
            const conn = {
                id: i, state: 'ready', openedAt: t0, lastRecvAt: t0, pings: [], closedAt: 0, now: t0,
                sendFrame(buf) {
                    if (buf[0] !== MSG.S_Ping) return;
                    this.pings.push(this.now);
                    if (!silent(this)) this.lastRecvAt = this.now;
                },
                close() { this.closedAt = this.now; this.state = 'closed'; router._onClose(this); },
            };
            router._onConnection(conn);
            conn.ctx.pingAt = t0;
            conns.push(conn);
        }
        const run = (ms) => {
            for (let t = t0 + tickMs; t <= t0 + ms; t += tickMs) {
                for (const c of conns) c.now = t;
                router.sweep(t);
            }
        };
        return { conns, run, t0 };
    }

    it('pings and checks the first connection of a worker too', () => {
        for (const [n, hb, ms] of [[11, 1000, 6000], [150, 10000, 45000], [1601, 10000, 45000]]) {
            const { conns, run } = sweepRig({ n, heartbeatMs: hb });
            run(ms);
            for (const c of conns) assert.ok(c.pings.length >= Math.floor(ms / hb) - 1, `n=${n}: connection ${c.id} got ${c.pings.length} pings`);
        }
        // A silent first connection is closed after the heartbeat timeout, not kept forever.
        const { conns, run, t0 } = sweepRig({ n: 11, heartbeatMs: 1000, silent: (c) => c.id === 1 });
        run(6000);
        assert.ok(conns[0].closedAt > 0 && conns[0].closedAt - t0 <= 3000 + 1000 + 250, `closed after ${conns[0].closedAt - t0} ms`);
        assert.ok(conns.slice(1).every((c) => c.closedAt === 0));
    });

    it('spaces the pings of every connection between half an interval and an interval', () => {
        const hb = 10000, tick = 250;
        for (const n of [1, 2, 3, 5, 11, 40, 41, 150, 399, 1000, 1601, 4000]) {
            const { conns, run, t0 } = sweepRig({ n, heartbeatMs: hb, tickMs: tick });
            run(60000);
            for (const c of conns) {
                // The first one: at the first visit half an interval after the opening.
                assert.ok(c.pings.length && c.pings[0] - t0 >= hb / 2 && c.pings[0] - t0 <= hb * 1.5 + tick,
                    `n=${n}: connection ${c.id}, first ping after ${c.pings[0] - t0} ms`);
                for (let i = 1; i < c.pings.length; i++) {
                    const gap = c.pings[i] - c.pings[i - 1];
                    assert.ok(gap >= hb / 2 && gap <= hb + tick, `n=${n}: connection ${c.id}, ${gap} ms before ping ${i + 1}`);
                }
                assert.ok(t0 + 60000 - c.pings.at(-1) <= hb + tick, `n=${n}: connection ${c.id} not pinged since ${c.pings.at(-1) - t0} ms`);
            }
        }
    });
});
