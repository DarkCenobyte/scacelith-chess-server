// Protection per address on the real server (src/net/ipguard.js, src/cluster/abuse.js), with two
// workers. 127.0.0.1 is in ABUSE_EXEMPT (the harness's own requests, and alice); 127.0.0.2 is an
// ordinary address (Linux routes the whole 127.0.0.0/8 to the loopback). bobby plays from
// 127.0.0.2 over a WebSocket opened before the flood, as a player who shares a school's or a
// mobile operator's address with an abuser would.
//
//   - a client of 127.0.0.2 flooding 404s over keep-alive connections is refused (429), then
//     blocked: its new TCP connections get an RST before TLS on both workers within seconds,
//     while 127.0.0.1 keeps working and bobby's game goes on over his open WebSocket;
//   - once the block ends 127.0.0.2 connects again, and a second flood is blocked 4 times longer;
//   - an exempt address is never refused nor blocked.
// Needs the openssl command line (skipped without it).
import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import https from 'node:https';
import tls from 'node:tls';
import { startServer, haveOpenssl } from './helpers/harness.js';
import { account, connect, challengeGame, Table, closeAll } from './helpers/players.js';

const skip = !haveOpenssl() && 'openssl not available';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const FLOODER = '127.0.0.2';
const BASE_SEC = 3;

let srv, alice, bobby, table;

before(async () => {
    if (skip) return;
    srv = await startServer({
        workers: 2,
        env: {
            ABUSE_EXEMPT: '127.0.0.1',
            HTTP_RATE_PER_IP: '60',                   // a burst of 30 per worker
            ABUSE_BLOCK_REFUSALS_PER_MIN: '200',
            ABUSE_BLOCK_BASE_SEC: String(BASE_SEC),
            ABUSE_BLOCK_MAX_SEC: '600',
            LOG_IP: 'full',                           // tell 127.0.0.1 from 127.0.0.2 in the log
        },
    });
    [alice, bobby] = await Promise.all(['alice', 'bobby'].map((n) => account(srv, n)));
    alice.client = await connect(srv, alice.token);
    bobby.client = await connect(srv, bobby.token, { localAddress: FLOODER });
    table = new Table(await challengeGame(alice, bobby, { baseSec: 600, incSec: 5 }));
    await table.playAll(['e2e4', 'e7e5']);
});
after(async () => {
    if (!srv) return;
    await closeAll(alice, bobby);
    await srv.stop();
});

/** Value of one sample of the primary's /metrics (name with its labels, as printed), 0 when absent. */
async function metric(sample) {
    const text = await srv.metrics();
    const line = text.split('\n').find((l) => l.startsWith(`${sample} `));
    return line ? Number(line.slice(sample.length + 1)) : 0;
}

/** One GET from `localAddress` on a new connection: its status, or the error code of the connection. */
function getFrom(localAddress, path = '/api/v1/info') {
    return new Promise((resolve) => {
        const req = https.get({ host: '127.0.0.1', port: srv.apiPort, path, ca: srv.ca, servername: 'localhost', localAddress, agent: false, timeout: 3000 }, (res) => {
            res.resume();
            res.on('end', () => resolve(res.statusCode));
        });
        req.on('error', (e) => resolve(e.code || e.message));
        req.on('timeout', () => req.destroy(new Error('timeout')));
    });
}

/** A TLS connection from `localAddress`: 'tls' when the handshake completed, else the error code. */
function tlsFrom(localAddress) {
    return new Promise((resolve) => {
        const s = tls.connect({ host: '127.0.0.1', port: srv.apiPort, ca: srv.ca, servername: 'localhost', localAddress }, () => { s.destroy(); resolve('tls'); });
        s.on('error', (e) => resolve(e.code || e.message));
    });
}

/**
 * Floods 404s from `localAddress` over `lanes` keep-alive connections until `stop()` is true or
 * `ms` passed; returns the counts by outcome and when the first 429 came.
 */
async function flood(localAddress, { lanes = 8, ms = 8000, stop = () => false } = {}) {
    const agent = new https.Agent({ keepAlive: true, maxSockets: lanes, ca: srv.ca, servername: 'localhost', localAddress });
    const out = { counts: {}, first429: 0 };
    const t0 = Date.now();
    const one = (i) => new Promise((resolve) => {
        const req = https.get({ host: '127.0.0.1', port: srv.apiPort, path: `/api/v1/nothing-${i}`, agent, timeout: 3000 }, (res) => {
            res.resume();
            res.on('end', () => resolve(res.statusCode));
        });
        req.on('error', (e) => resolve(e.code || 'error'));
        req.on('timeout', () => req.destroy(new Error('timeout')));
    });
    let i = 0;
    const lane = async () => {
        while (Date.now() - t0 < ms && !stop()) {
            const r = await one(i++);
            out.counts[r] = (out.counts[r] || 0) + 1;
            if (r === 429 && !out.first429) out.first429 = Date.now();
            if (typeof r === 'string') await sleep(20);
        }
    };
    await Promise.all(Array.from({ length: lanes }, lane));
    agent.destroy();
    return out;
}

/** Resolves with the time when `n` new connections in a row from `localAddress` were all refused before TLS. */
async function blockedEverywhere(localAddress, n = 6, ms = 10000) {
    const t0 = Date.now();
    while (Date.now() - t0 < ms) {
        const r = await Promise.all(Array.from({ length: n }, () => tlsFrom(localAddress)));
        if (r.every((x) => x === 'ECONNRESET')) return Date.now();
        await sleep(100);
    }
    return 0;
}

test('a flooding address is refused, then blocked before TLS on both workers; others keep working', { skip }, async () => {
    const refusedBefore = await metric('scacelith_tls_refused_total{reason="blocked"}');
    let blockedAt = 0;
    const watcher = (async () => {
        while (!blockedAt) {
            await sleep(100);
            const t = await blockedEverywhere(FLOODER, 6, 300);
            if (t) blockedAt = t;
        }
    })();
    const f = await flood(FLOODER, { stop: () => blockedAt > 0, ms: 15000 });
    await watcher;
    assert.ok(f.counts[404] > 0 && f.counts[429] > 0, `refused before being blocked: ${JSON.stringify(f.counts)}`);
    assert.ok(blockedAt > 0, 'blocked');
    const delay = blockedAt - f.first429;
    process.stdout.write(`# flood: ${JSON.stringify(f.counts)}; blocked on every worker ${delay} ms after the first 429\n`);
    assert.ok(delay < 4000, `blocked within seconds (${delay} ms)`);

    // Both workers know the block (a per-shard gauge), and turned the connections away before TLS.
    const text = await srv.metrics();
    const perShard = text.split('\n').filter((l) => l.startsWith('scacelith_abuse_blocked_keys{'));
    assert.equal(perShard.length, 2, perShard.join('\n'));
    for (const l of perShard) assert.ok(Number(l.split(' ').at(-1)) >= 1, l);
    assert.ok(await metric('scacelith_tls_refused_total{reason="blocked"}') - refusedBefore >= 6);
    const log = await srv.waitLog((r) => r.msg === 'ip blocked' && r.ip === FLOODER);
    assert.deepEqual([log.scope, log.level, log.ttlSec], ['ip', 1, BASE_SEC]);

    // The exempt address is served; the WebSocket that 127.0.0.2 opened before is not cut.
    assert.equal(await getFrom('127.0.0.1'), 200);
    assert.equal(await tlsFrom(FLOODER), 'ECONNRESET');
    await table.playAll(['g1f3', 'b8c6']);
    assert.equal(bobby.client.state, 'ready');
});

test('the block ends after its time; a second flood is blocked 4 times longer', { skip }, async () => {
    // The first block lasts BASE_SEC: the address connects again afterwards.
    const t0 = Date.now();
    let status;
    while ((status = await getFrom(FLOODER)) !== 200 && Date.now() - t0 < 4 * BASE_SEC * 1000) await sleep(200);
    assert.equal(status, 200, 'served again once the block ended');
    assert.ok(Date.now() - t0 <= (BASE_SEC + 2) * 1000);

    let blockedAt = 0;
    const watcher = (async () => {
        while (!blockedAt) {
            await sleep(100);
            const t = await blockedEverywhere(FLOODER, 6, 300);
            if (t) blockedAt = t;
        }
    })();
    await flood(FLOODER, { stop: () => blockedAt > 0, ms: 15000 });
    await watcher;
    const log = await srv.waitLog((r) => r.msg === 'ip blocked' && r.ip === FLOODER && r.level === 2);
    assert.equal(log.ttlSec, 4 * BASE_SEC);
    await sleep((BASE_SEC + 1) * 1000);
    assert.equal(await tlsFrom(FLOODER), 'ECONNRESET', 'still blocked after the first duration');
    await table.playAll(['f1c4', 'g8f6']);
});

test('an exempt address is never refused nor blocked', { skip }, async () => {
    const f = await flood('127.0.0.1', { ms: 3000 });
    assert.deepEqual(Object.keys(f.counts), ['404'], JSON.stringify(f.counts));
    assert.ok(f.counts[404] > 200, 'well beyond HTTP_RATE_PER_IP');
    await sleep(1500);
    assert.equal(srv.lines.some((r) => r.msg === 'ip blocked' && r.ip === '127.0.0.1'), false);
    assert.equal(await getFrom('127.0.0.1'), 200);
});
