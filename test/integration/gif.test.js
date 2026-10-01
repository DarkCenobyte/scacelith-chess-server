// The animated GIF of a played game, end to end on the real server (bin/scacelith-server.js, TLS,
// SQLite, the cluster worker and its rendering thread): two players play a fool's mate through the
// realtime protocol, the game is committed, and its owner downloads GET /api/v1/games/:id/gif. The
// file is decoded here (test/unit/helpers/gif-decode.js): one frame per position, the last one
// held, the chosen size. A second download comes from the worker's cache (no render, no quota),
// the PGN of the game sent to POST /api/v1/gif gives the same number of frames, and the metrics
// endpoint counts the renders. Then SIGTERM: the worker closes its rendering pool and the server
// exits cleanly. Needs the openssl command line (skipped without it).
import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import https from 'node:https';
import { startServer, haveOpenssl } from './helpers/harness.js';
import { player, challengeGame, Table, closeAll } from './helpers/players.js';
import { decodeGif } from '../unit/helpers/gif-decode.js';

const skip = !haveOpenssl() && 'openssl not available';

let srv;
before(async () => {
    if (skip) return;
    srv = await startServer({ workers: 1, env: { FIRST_MOVE_TIMEOUT_MS: '20000', DB_COMMIT_MS: '20' } });
});
after(async () => { if (srv && !srv.exited) await srv.stop(); });

/** A request to the API answered as raw bytes (the SDK's ApiClient decodes bodies as text). */
function raw(method, p, { token, json } = {}) {
    return new Promise((resolve, reject) => {
        const body = json === undefined ? null : Buffer.from(JSON.stringify(json));
        const headers = {
            ...(token ? { Authorization: `Bearer ${token}` } : {}),
            ...(body ? { 'Content-Type': 'application/json', 'Content-Length': body.length } : {}),
        };
        const req = https.request({ host: '127.0.0.1', port: srv.apiPort, method, path: `/api/v1${p}`, headers, ca: srv.ca, servername: 'localhost', agent: false }, (res) => {
            const chunks = [];
            res.on('data', (c) => chunks.push(c));
            res.on('end', () => {
                const bytes = Buffer.concat(chunks);
                let parsed = null;
                if (/json/.test(res.headers['content-type'] || '')) parsed = JSON.parse(bytes.toString('utf8'));
                resolve({ status: res.statusCode, headers: res.headers, bytes, json: parsed });
            });
        });
        req.on('error', reject);
        req.end(body || undefined);
    });
}

const metric = (text, line) => {
    const m = text.split('\n').find((l) => l.startsWith(line + ' '));
    return m ? Number(m.slice(line.length + 1)) : 0;
};

test('GIF of a played game: download, decode, cache, the same game from its PGN', { skip }, async () => {
    const alice = await player(srv, 'alice_gif'), bob = await player(srv, 'bob_gif');
    try {
        // Fool's mate: Black (Bob) mates in two.
        const g = await challengeGame(alice, bob, { baseSec: 180, incSec: 2 });
        const table = new Table(g);
        const m = alice.client.mark();
        await table.playAll(['f2f3', 'e7e5', 'g2g4', 'd8h4']);
        await alice.client.waitFor('GameEnd', (x) => x.game === g.id, 10000, { since: m });

        // The game record is there once committed.
        const deadline = Date.now() + 10000;
        let rec;
        do {
            rec = await raw('GET', `/games/${g.id}`, { token: alice.token });
            if (rec.status === 200) break;
            await new Promise((r) => setTimeout(r, 50));
        } while (Date.now() < deadline);
        assert.equal(rec.status, 200, rec.bytes.toString());
        assert.equal(rec.json.result, '0-1');

        // A session is needed.
        assert.equal((await raw('GET', `/games/${g.id}/gif`)).status, 401);

        // The download.
        const before = await srv.metrics();
        const r = await raw('GET', `/games/${g.id}/gif?size=small&delay=250`, { token: alice.token });
        assert.equal(r.status, 200, r.bytes.toString().slice(0, 200));
        assert.equal(r.headers['content-type'], 'image/gif');
        assert.equal(r.headers['content-disposition'], `attachment; filename="scacelith-${g.id}.gif"`);
        assert.equal(Number(r.headers['content-length']), r.bytes.length);
        assert.equal(r.bytes.subarray(0, 6).toString('latin1'), 'GIF89a');
        const gif = decodeGif(r.bytes);
        assert.equal(gif.frames.length, 5, 'the start position and one frame per move');
        assert.deepEqual(gif.frames.map((f) => f.delayCs), [100, 25, 25, 25, 300], 'the start held 1 s, the mate 3 s');
        assert.ok(gif.width > 0 && gif.width < 300, `small: ${gif.width}`);

        // The same picture again: from the cache, byte for byte, and the other player gets it too.
        const again = await raw('GET', `/games/${g.id}/gif?size=small&delay=250`, { token: bob.token });
        assert.equal(again.status, 200);
        assert.ok(again.bytes.equals(r.bytes));

        // The PGN of the game, sent to POST /gif: the same moves, so the same frames.
        const pgn = await raw('GET', `/games/${g.id}/pgn`, { token: alice.token });
        assert.equal(pgn.status, 200);
        const p = await raw('POST', '/gif', { token: alice.token, json: { pgn: pgn.bytes.toString('utf8'), size: 'small', delayMs: 250 } });
        assert.equal(p.status, 200, p.bytes.toString().slice(0, 200));
        assert.equal(p.headers['content-disposition'], 'attachment; filename="scacelith-game.gif"');
        assert.equal(decodeGif(p.bytes).frames.length, 5);

        const bad = await raw('POST', '/gif', { token: alice.token, json: { pgn: '1. e4 e5 2. Ke3 *' } });
        assert.deepEqual([bad.status, bad.json.error, bad.json.line], [400, 'invalid_pgn', 1]);

        // Two renders (the GET and the POST), one cache hit.
        const after = await srv.metrics();
        assert.equal(metric(after, 'scacelith_gif_renders_total{result="ok"}') - metric(before, 'scacelith_gif_renders_total{result="ok"}'), 2);
        assert.equal(metric(after, 'scacelith_gif_cache_total{result="hit"}') - metric(before, 'scacelith_gif_cache_total{result="hit"}'), 1);
    } finally {
        await closeAll(alice, bob);
    }
});

test('SIGTERM after renders: the pool is closed and the server exits 0', { skip }, async () => {
    const t0 = Date.now();
    const exited = await srv.stop({ timeoutMs: 15000 });
    assert.deepEqual(exited, { code: 0, signal: null }, srv.lines.slice(-10).map((l) => JSON.stringify(l)).join('\n'));
    assert.ok(Date.now() - t0 < 10000, `stopped in ${Date.now() - t0} ms`);
    assert.ok(!srv.lines.some((l) => l.level === 'error' || /Error/.test(l.raw || '')), 'no error logged');
});
