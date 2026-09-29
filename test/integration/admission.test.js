// A full server (DESIGN 5.8, "Players first at MAX_CONNECTIONS"), on the real server with one
// worker and MAX_CONNECTIONS=2, both places held by two signed-in players with a game in progress.
// Newcomers are all refused: at MAX_CONNECTIONS itself each one completes TLS and the upgrade and
// gets ServerFull at Hello, since a ServerFull at Hello does not make the worker shed; once the
// upgrade reserve (max(16, 2 %), so 16 here) is in use as well, an upgrade may get HTTP 503 and the
// TLS gate may close connections before TLS. A player of the game in progress who comes back while
// the server is full still gets Welcome and the game, and the connection counts of the primary
// return to the players actually online. Needs the openssl command line (skipped without it).
import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import { startServer, haveOpenssl } from './helpers/harness.js';
import { account, connect, challengeGame, Table, closeAll } from './helpers/players.js';
import { enums } from '../../src/protocol/index.js';
import { CLOSE_SERVER_FULL } from '../../src/cluster/router.js';

const { ErrorCode: EC, GameEventKind: EV } = enums;
const skip = !haveOpenssl() && 'openssl not available';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let srv, alice, bobby, carol, dave, game, table;

before(async () => {
    if (skip) return;
    srv = await startServer({
        workers: 1,
        env: {
            MAX_CONNECTIONS: '2',
            // Every client of the test comes from 127.0.0.1: without this, the per-group handshake
            // cap (4) would refuse some newcomers before TLS for a reason other than a full server.
            MAX_PENDING_HANDSHAKES_PER_IP: '127',
            SHUTDOWN_GRACE_MS: '0',
            PASSWORD_HASH_CONCURRENCY: '3',              // the accounts below are made together
            LOG_LEVEL: 'warn',
        },
    });
    // carol is the newcomer (signed in, never let in); dave takes a place a player freed.
    [alice, bobby, carol, dave] = await Promise.all(['alice', 'bobby', 'carol', 'dave'].map((n) => account(srv, n)));
    alice.client = await connect(srv, alice.token);
    bobby.client = await connect(srv, bobby.token);
    game = await challengeGame(alice, bobby, { baseSec: 180, incSec: 2 });
    table = new Table(game);
    await table.playAll(['e2e4', 'e7e5']);
});
after(async () => {
    if (!srv) return;
    await closeAll(alice, bobby, carol, dave);
    await srv.stop();
});

/** One connection attempt with `token`, classified by how it ended. */
async function attempt(token) {
    try {
        const c = await connect(srv, token, { timeoutMs: 10000 });
        await c.close().catch(() => {});
        return 'welcome';
    } catch (e) {
        if (e.errorCode === EC.ServerFull && e.closeCode === CLOSE_SERVER_FULL) return 'server_full_at_hello';
        if (e.status === 503) return 'http_503';
        if (/ECONNRESET|before secure TLS connection was established|socket hang up/.test(e.message)) return 'closed_before_tls';
        return `other: ${e.message}`;
    }
}

/** `total` attempts, `concurrency` at a time; returns { outcome: count }. */
async function newcomers(token, total, concurrency) {
    const out = {};
    let started = 0;
    const lane = async () => {
        while (started < total) {
            started++;
            const k = await attempt(token);
            out[k] = (out[k] || 0) + 1;
        }
    };
    await Promise.all(Array.from({ length: concurrency }, lane));
    return out;
}

/** Value of one sample of the primary's /metrics (name with its labels, as printed), 0 when absent. */
async function metric(sample) {
    const text = await srv.metrics();
    const line = text.split('\n').find((l) => l.startsWith(`${sample} `));
    return line ? Number(line.slice(sample.length + 1)) : 0;
}

const HELLO_FULL = 'scacelith_ws_hello_total{result="server_full"}';
const UPGRADE_FULL = 'scacelith_ws_handshakes_rejected_total{reason="server_full"}';
const TLS_FULL = 'scacelith_tls_refused_total{reason="server_full"}';

/** Waits until the primary counts `n` players online and `n` WebSocket connections. */
async function presenceBackTo(n, ms = 5000) {
    const t0 = Date.now();
    let seen;
    for (;;) {
        seen = [await metric('scacelith_presence_online'), await metric('scacelith_presence_connections')];
        if (seen[0] === n && seen[1] === n) return;
        if (Date.now() - t0 > ms) break;
        await sleep(50);
    }
    assert.deepEqual(seen, [n, n], 'players online and WebSocket connections counted by the primary');
}

test('a server at MAX_CONNECTIONS refuses every newcomer, at Hello while the upgrade reserve lasts', { skip }, async () => {
    await presenceBackTo(2);
    const before = { hello: await metric(HELLO_FULL), upgrade: await metric(UPGRADE_FULL), tls: await metric(TLS_FULL) };

    // A few at a time, well inside the reserve: every newcomer completes TLS and the upgrade and
    // is refused at Hello. The worker does not shed (players first), so nothing is closed before TLS.
    const calm = await newcomers(carol.token, 24, 4);
    assert.deepEqual(calm, { server_full_at_hello: 24 });
    assert.equal(await metric(HELLO_FULL) - before.hello, 24, 'the metric of a full server counts them');
    assert.equal(await metric(UPGRADE_FULL) - before.upgrade, 0, 'no upgrade refused inside the reserve');
    assert.equal(await metric(TLS_FULL) - before.tls, 0, 'no shedding before TLS at MAX_CONNECTIONS itself');
    await presenceBackTo(2);

    // More at once than the reserve: some upgrades may now be refused (503) and the gate may shed,
    // but nobody gets in, whatever the timing.
    const mid = { hello: await metric(HELLO_FULL), upgrade: await metric(UPGRADE_FULL) };
    const burst = await newcomers(carol.token, 64, 32);
    const allowed = new Set(['server_full_at_hello', 'http_503', 'closed_before_tls']);
    for (const k of Object.keys(burst)) assert.ok(allowed.has(k), `unexpected outcome ${k}: ${JSON.stringify(burst)}`);
    assert.equal(Object.values(burst).reduce((a, b) => a + b, 0), 64);
    assert.equal(await metric(HELLO_FULL) - mid.hello, burst.server_full_at_hello || 0);
    assert.equal(await metric(UPGRADE_FULL) - mid.upgrade, burst.http_503 || 0);
    // The reserve's counts do not drift: only the two players remain.
    await presenceBackTo(2);
});

test('a player whose game is in progress comes back to a full server and gets Welcome and the game', { skip }, async () => {
    // Alice loses her connection; a newcomer takes the place she freed, so the server is full again.
    const mb = bobby.client.mark();
    alice.client.ws.terminate();
    const gone = await bobby.client.waitFor('GameEvent', (m) => m.kind === EV.PlayerDisconnected, 5000, { since: mb });
    assert.equal(gone.color, 0);
    dave.client = await connect(srv, dave.token);
    try {
        await presenceBackTo(2);
        assert.equal(await attempt(carol.token), 'server_full_at_hello', 'the server is full');

        // Alice comes back while it is full: admitted at Hello for her game, beyond MAX_CONNECTIONS.
        const c2 = await connect(srv, alice.token);
        alice.client = c2;
        assert.equal(c2.welcome.activeGame, game.id);
        const snap = await c2.waitFor('GameSnapshot', (m) => m.game === game.id, 5000, { since: 0 });
        assert.equal(snap.moves.length, 2);
        await bobby.client.waitFor('GameEvent', (m) => m.kind === EV.PlayerReconnected, 5000, { since: mb });
        await presenceBackTo(3);
        await table.play('g1f3');                       // the game goes on
        assert.equal(await attempt(carol.token), 'server_full_at_hello', 'still full for newcomers');
    } finally {
        await dave.client.close().catch(() => {});
    }
    await presenceBackTo(2);
});
