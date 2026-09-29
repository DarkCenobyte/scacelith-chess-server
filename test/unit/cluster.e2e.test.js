// Multi-process test of the cluster wiring: the real primary (supervisor, control plane,
// presence, real Matchmaker and Challenges, metrics endpoint) forks two real shard processes
// (startShard: bus over Unix sockets, router, WebSocket server, listeners) whose GameHost and
// auth service are stubs. Covers: hello, queue, pairing, game placement, attach across the bus,
// game messages relayed to the host shard, replacement of a connection on another shard, a
// shard crash and restart (the player gets their game back), metrics aggregation and the
// graceful shutdown.

import assert from 'node:assert/strict';
import { fork } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { after, before, describe, it } from 'node:test';
import { startPrimary } from '../../src/cluster/primary.js';
import { testConfig } from '../../src/config.js';
import { Challenges } from '../../src/match/challenges.js';
import { Matchmaker } from '../../src/match/matchmaker.js';
import { Registry } from '../../src/metrics.js';
import { connectWs } from '../../src/net/ws-raw-client.js';
import { PROTOCOL_VERSION, SCHEMA_HASH, decode, encode, enums, messageName } from '../../src/protocol/index.js';

const E = enums.ErrorCode, N = enums.NoticeCode, QS = enums.QueueState;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const url = (p) => new URL(p, import.meta.url).href;
const silent = { child: () => silent, debug() {}, info() {}, warn() {}, error() {}, security() {} };

async function freePort() {
    return new Promise((resolve) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); }); });
}
async function waitFor(pred, ms = 5000) {
    const t0 = Date.now();
    while (!(await pred())) {
        if (Date.now() - t0 > ms) throw new Error('condition not reached');
        await sleep(10);
    }
}

// The shard process: startShard with a stub host (attach answers Notice{Motd, gameId}, every game
// message is answered to all attached players with Notice{Motd, seq}) and a stub auth service
// (token "user-<id>-..." is user <id>).
const WORKER = `
import { Ipc } from ${JSON.stringify(url('../../src/cluster/ipc.js'))};
import { startShard } from ${JSON.stringify(url('../../src/cluster/shard.js'))};
import { testConfig } from ${JSON.stringify(url('../../src/config.js'))};
import { encode, enums } from ${JSON.stringify(url('../../src/protocol/index.js'))};
import { GameIdAllocator } from ${JSON.stringify(url('../../src/util/ids.js'))};
import { Registry } from ${JSON.stringify(url('../../src/metrics.js'))};
const shard = Number(process.env.SHARD);
const config = testConfig(JSON.parse(process.env.E2E_ENV));
const silent = { child: () => silent, debug() {}, info() {}, warn() {}, error() {}, security() {} };
const primary = new Ipc(process);
const alloc = new GameIdAllocator(shard);
const games = new Map();
const motd = (arg) => encode.Notice({ code: enums.NoticeCode.Motd, arg });
const host = {
    createGame(spec) { const id = alloc.next(); games.set(id, { spec, eps: new Map() }); return id; },
    attach(gameId, userId, ep) { const g = games.get(gameId); if (!g) return; g.eps.set(userId, ep); ep.send(motd(gameId)); },
    detach(gameId, userId, ep) { const g = games.get(gameId); if (g && g.eps.get(userId) === ep) g.eps.delete(userId); },
    onClientMessage(gameId, userId, msg) { const g = games.get(gameId); if (!g) return; for (const ep of g.eps.values()) ep.send(motd(msg.seq)); },
    onRtt() {}, forfeitUser() { return false; }, stats() { return { games: games.size }; }, async shutdown() {},
};
const auth = {
    async validateToken(t) { const m = /^user-(\\d+)-/.exec(t); return m ? { userId: +m[1], username: 'user' + m[1], emailVerified: true } : null; },
};
await startShard({
    config, shard, serverId: process.env.SCACELITH_SERVER_ID, primary, host, auth, log: silent, registry: new Registry(),
    onStopped: () => { primary.flush(); setTimeout(() => process.exit(0), 20); },
});
process.on('disconnect', () => process.exit(0));
`;

class Client {
    constructor(ws) { this.ws = ws; this.seq = 1; }
    static async open(port, userId) {
        const c = new Client(await connectWs({ port }));
        c.ws.send(encode.Hello({ seq: 1, proto: PROTOCOL_VERSION, schema: SCHEMA_HASH, client: 'e2e', token: `user-${userId}-${'x'.repeat(20)}` }));
        c.welcome = await c.until('Welcome');
        return c;
    }
    send(name, fields = {}) { this.ws.send(encode[name]({ seq: ++this.seq, ...fields })); return this.seq; }
    async recv(ms = 5000) { const b = await this.ws.next(ms); return { name: messageName(b[0]), ...decode(b) }; }
    async until(name, pred = () => true, ms = 5000) { for (;;) { const m = await this.recv(ms); if (m.name === name && pred(m)) return m; } }
}

function get(port, p) {
    return new Promise((resolve, reject) => {
        http.get({ host: '127.0.0.1', port, path: p }, (res) => { let body = ''; res.on('data', (c) => { body += c; }); res.on('end', () => resolve({ status: res.statusCode, body })); }).on('error', reject);
    });
}

describe('cluster end to end (primary + 2 shard processes)', { timeout: 60000 }, () => {
    let dir, worker, primary, ports, metricsPort, children;
    before(async () => {
        dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-e2e-'));
        worker = path.join(dir, 'worker.mjs');
        fs.writeFileSync(worker, WORKER);
        ports = [await freePort(), await freePort()];
        metricsPort = await freePort();
        const base = { DATA_DIR: dir, WORKERS: '2', BIND_ADDRESS: '127.0.0.1', REQUIRE_EMAIL_VERIFICATION: 'false', MATCH_TICK_MS: '50', SHUTDOWN_GRACE_MS: '100' };
        const config = testConfig({ ...base, METRICS_PORT: String(metricsPort), METRICS_BIND: '127.0.0.1' });
        fs.mkdirSync(config.runDir, { recursive: true, mode: 0o700 });
        children = new Map();
        primary = await startPrimary({
            config, log: silent, registry: new Registry(),
            matchmaker: new Matchmaker({ config }), challenges: new Challenges({ config }),
            fork: (shard) => {
                const env = { ...base, API_PORT: String(ports[shard]), WS_PORT: String(ports[shard]) };
                const child = fork(worker, [], {
                    serialization: 'advanced', stdio: ['ignore', 'inherit', 'inherit', 'ipc'],
                    env: { ...process.env, SHARD: String(shard), SCACELITH_SERVER_ID: 'e2e', E2E_ENV: JSON.stringify(env) },
                });
                children.set(shard, child);
                return child;
            },
        });
        await waitFor(() => primary.ready(), 15000);
    });
    after(async () => {
        for (const c of children.values()) if (c.exitCode === null) c.kill('SIGKILL');
        fs.rmSync(dir, { recursive: true, force: true });
    });

    let a, b, gameId;

    it('pairs two players of different shards and hosts the game on the older searcher shard', async () => {
        a = await Client.open(ports[0], 1);
        b = await Client.open(ports[1], 2);
        assert.deepEqual([a.welcome.userId, b.welcome.userId, a.welcome.activeGame], [1, 2, 0]);
        // Every shard names the server in its 101 response (the /info serverId).
        assert.deepEqual([a.ws.headers['scacelith-server-id'], b.ws.headers['scacelith-server-id']], ['e2e', 'e2e']);
        const s1 = a.send('QueueJoin', { category: '5+0', rated: true });
        assert.equal((await a.until('Ack')).ref, s1);
        assert.equal((await a.until('QueueStatus')).state, QS.Searching);
        await sleep(20);
        b.send('QueueJoin', { category: '5+0', rated: true });
        await b.until('QueueStatus', (m) => m.state === QS.Matched);
        await a.until('QueueStatus', (m) => m.state === QS.Matched);
        const na = await a.until('Notice', (m) => m.code === N.Motd);        // host attach (local)
        const nb = await b.until('Notice', (m) => m.code === N.Motd);        // host attach (over the bus)
        gameId = na.arg;
        assert.equal(nb.arg, gameId);
        assert.equal(Math.floor(gameId / 64) % 64, 0);                       // shard of the older searcher
    });

    it('relays a remote player\'s game message to the host shard and the answers back', async () => {
        const seq = b.send('Resign', { game: gameId });
        assert.equal((await b.until('Notice', (m) => m.code === N.Motd)).arg, seq);
        assert.equal((await a.until('Notice', (m) => m.code === N.Motd)).arg, seq);
    });

    it('replaces a connection from another shard (4007)', async () => {
        const a2 = await Client.open(ports[1], 1);
        assert.equal(a2.welcome.activeGame, gameId);
        assert.equal((await a.until('Error')).code, E.Replaced);
        assert.equal((await a.until('Notice')).code, N.ReplacedByNewConnection);
        assert.equal((await a.ws.closed).code, 4007);
        await a2.until('Notice', (m) => m.code === N.Motd && m.arg === gameId);     // re-attached from shard 1
        a = a2;
    });

    it('aggregates the metrics of every process', async () => {
        const r = await get(metricsPort, '/metrics');
        assert.equal(r.status, 200);
        assert.match(r.body, /scacelith_ws_connections\{shard="0"\} \d+/);
        assert.match(r.body, /scacelith_ws_connections\{shard="1"\} 2/);
        assert.match(r.body, /scacelith_presence_online 2/);
        assert.equal((await get(metricsPort, '/readyz')).status, 200);
    });

    it('restarts a crashed shard; its players reconnect and get their game back', async () => {
        const old = children.get(1);
        old.kill('SIGKILL');
        assert.equal((await b.ws.closed).code, 1006);
        await waitFor(() => children.get(1) !== old && primary.ready(), 10000);
        assert.equal(primary.presence.get(2), undefined);
        const b2 = await Client.open(ports[1], 2);
        assert.equal(b2.welcome.activeGame, gameId);
        assert.equal((await b2.until('Notice', (m) => m.code === N.Motd)).arg, gameId);
        const seq = b2.send('Resign', { game: gameId });
        assert.equal((await b2.until('Notice', (m) => m.code === N.Motd && m.arg === seq)).arg, seq);
        b = b2;
    });

    it('shuts down gracefully: ServerShutdown, then ShuttingDown and 4008, then the shards exit', async () => {
        const stopping = primary.stop(100);
        const n = await b.until('Notice', (m) => m.code === N.ServerShutdown);
        assert.equal(n.arg, 100);
        assert.equal((await b.until('Error')).code, E.ShuttingDown);
        assert.equal((await b.ws.closed).code, 4008);
        await stopping;
        await waitFor(() => [...children.values()].every((c) => c.exitCode !== null || c.signalCode !== null), 5000);
    });
});
