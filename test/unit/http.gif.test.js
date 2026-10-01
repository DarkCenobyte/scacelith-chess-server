// The GIF routes (src/http/routes/gif.js) through the real API handler: sessions, options,
// the job built from a stored record or a PGN, invalid PGN, game length, the cache, the render
// quotas per account and per address (two workers sharing one primary), the busy pool and its
// refunds, failures, the lazy pool, and a real stored game rendered by the real thread pool.

import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import fs from 'node:fs';
import os from 'node:os';
import { createApiHandler } from '../../src/http/server.js';
import * as gifRoutes from '../../src/http/routes/gif.js';
import { gifCacheKey, gifOptions, GifCache, tagText, GIF_THREAD_URL } from '../../src/http/routes/gif.js';
import { GIF_THREAD_NICE } from '../../src/http/routes/gif-thread.js';
import { createGifPool } from '../../src/gif/pool.js';
import { gamePgn } from '../../src/http/routes/players.js';
import { ChessGame, Position } from '../../src/chess/index.js';
import { enums } from '../../src/protocol/schema.js';
import { openStore, migrate } from '../../src/store/index.js';
import { testConfig } from '../../src/config.js';
import { metrics } from '../../src/metrics.js';
import { logger } from '../../src/log.js';
import { decodeGif } from './helpers/gif-decode.js';
import { startTestServer } from './helpers/auth-fakes.js';

const { GameStatus, EndReason } = enums;
const PW = 'correct horse battery';
const OPERA_UCI = 'e2e4 e7e5 g1f3 d7d6 d2d4 c8g4 d4e5 g4f3 d1f3 d6e5 f1c4 g8f6 f3b3 d8e7 b1c3 c7c6 c1g5 b7b5 c3b5 c6b5 c4b5 b8d7 e1c1 a8d8 d1d7 d8d7 h1d1 e7e6 b5d7 f6d7 b3b8 d7b8 d1d8';
const OPERA_PGN = `[Event "Paris"]
[Site "Paris FRA"]
[Date "1858.??.??"]
[White "Paul Morphy"]
[Black "Duke Karl / Count Isouard"]
[Result "1-0"]

1. e4 e5 2. Nf3 d6 3. d4 Bg4 4. dxe5 Bxf3 5. Qxf3 dxe5 6. Bc4 Nf6 7. Qb3 Qe7
8. Nc3 c6 9. Bg5 b5 10. Nxb5 cxb5 11. Bxb5+ Nbd7 12. O-O-O Rd8 13. Rxd7 Rxd7
14. Rd1 Qe6 15. Bxd7+ Nxd7 16. Qb8+ Nxb8 17. Rd8# 1-0
`;

function uciMoves(list) {
    const p = Position.start();
    return list.split(' ').map((u) => { const m = p.parseUCI(u); assert.notEqual(m, -1, u); p.play(m); return m; });
}

let nextId = 7_000_000_000_000;
/** A finished game record as the store returns it. */
function record(white, black, uci = OPERA_UCI, extra = {}) {
    const moves = uciMoves(uci);
    return {
        id: ++nextId, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: white.id, blackId: black.id,
        whiteName: white.username, blackName: black.username, whiteRating: 1612, blackRating: 1588,
        startedAt: Date.UTC(2026, 8, 28, 10), endedAt: Date.UTC(2026, 8, 28, 11), status: GameStatus.WhiteWins, reason: EndReason.Checkmate,
        plyCount: moves.length, moves: Uint16Array.from(moves), spentMs: new Uint32Array(moves.length), clockMs: new Uint32Array(moves.length),
        rematchOf: 0, flags: 0, ratingChanges: null, ...extra,
    };
}

/** Stand-ins for createGifPool: each records its jobs; mode ok | busy | fail | hold. */
function fakePools() {
    const pools = [];
    const factory = (opts) => {
        const p = {
            opts, jobs: [], mode: 'ok', closed: false, held: [],
            render(job) {
                p.jobs.push(structuredClone(job));
                if (p.mode === 'busy') return Promise.reject(Object.assign(new Error('GIF renderer busy (queue full)'), { code: 'busy' }));
                if (p.mode === 'fail') return Promise.reject(Object.assign(new Error('GIF render failed: illegal move at ply 3'), { code: 'render_failed' }));
                const gif = Buffer.concat([Buffer.from('GIF89a'), Buffer.from(`#${pools.indexOf(p)}.${p.jobs.length}`)]);
                if (p.mode === 'hold') return new Promise((r) => p.held.push(() => r(gif)));
                return Promise.resolve(gif);
            },
            stats() { return { queued: 0, running: 0, live: 1 }; },
            async close() { p.closed = true; },
        };
        pools.push(p);
        return p;
    };
    return { pools, factory, get jobs() { return pools.flatMap((p) => p.jobs); } };
}

async function server(env = {}, more = {}) {
    const fp = more.pools || fakePools();
    const s = await startTestServer({ env, handlerOptions: { deps: { createGifPool: fp.factory } }, ...more.shared });
    s.fp = fp;
    return s;
}

async function signedIn(s, username) {
    const u = s.store.users.byUsername(username) ? { id: s.store.users.byUsername(username).id, username } : await s.createUser({ username, password: PW });
    const { token } = await s.login(username, PW);
    return { ...u, token };
}

const gif = (s, id, { token, ip, query = '' } = {}) => s.request('GET', `/api/v1/games/${id}/gif${query}`, { token, ip, binary: true });
const postGif = (s, body, { token, ip } = {}) => s.request('POST', '/api/v1/gif', { body, token, ip, binary: true });
const takes = (s, prefix) => s.primary.calls.filter((c) => c.type === 'ratelimit.take' && c.payload.key.startsWith(prefix));
const counter = (name, label) => metrics.metrics.get(name)?.children.get(label)?.value ?? 0;

test('both routes need a session; GIF_ENABLED=false answers 404 gif_disabled', async (t) => {
    const s = await server();
    t.after(s.close);
    const alice = await signedIn(s, 'alice');
    s.store._raw.games.push(record(alice, { id: 99, username: 'bob' }));
    let r = await gif(s, nextId);
    assert.deepEqual([r.status, r.json.error], [401, 'unauthorized']);
    r = await postGif(s, { pgn: OPERA_PGN });
    assert.deepEqual([r.status, r.json.error], [401, 'unauthorized']);
    assert.equal(s.fp.pools.length, 0);

    const off = await server({ GIF_ENABLED: 'false' });
    t.after(off.close);
    const bob = await signedIn(off, 'bob');
    off.store._raw.games.push(record(bob, { id: 98, username: 'carol' }));
    r = await gif(off, nextId, { token: bob.token });
    assert.deepEqual([r.status, r.json.error], [404, 'gif_disabled']);
    r = await postGif(off, { pgn: OPERA_PGN }, { token: bob.token });
    assert.deepEqual([r.status, r.json.error], [404, 'gif_disabled']);
    assert.equal(off.fp.pools.length, 0, 'no rendering thread');
});

test('options: defaults, validation (400 invalid_option), game ids, unknown games; nothing rendered or counted', async (t) => {
    const s = await server();
    t.after(s.close);
    const alice = await signedIn(s, 'alice');
    s.store._raw.games.push(record(alice, { id: 99, username: 'bob' }));
    const id = nextId;
    const bad = [
        ['?size=huge', 'size'], ['?orientation=left', 'orientation'], ['?delay=99', 'delay'], ['?delay=3001', 'delay'], ['?delay=fast', 'delay'],
        ['?delay=500.5', 'delay'], ['?coords=2', 'coords'], ['?coords=true', 'coords'],
    ];
    for (const [q, field] of bad) {
        const r = await gif(s, id, { token: alice.token, query: q });
        assert.deepEqual([r.status, r.json.error, r.json.field], [400, 'invalid_option', field], q);
    }
    for (const [p, code, status] of [['abc', 'invalid_game_id', 400], ['0', 'invalid_game_id', 400], ['99999999999999999', 'invalid_game_id', 400], ['12345', 'not_found', 404]]) {
        const r = await gif(s, p, { token: alice.token });
        assert.deepEqual([r.status, r.json.error], [status, code], p);
    }
    const badBodies = [
        [{ pgn: OPERA_PGN, size: 'xl' }, 400, 'invalid_option', 'size'], [{ pgn: OPERA_PGN, delayMs: 50 }, 400, 'invalid_option', 'delayMs'],
        [{ pgn: OPERA_PGN, delayMs: '500' }, 400, 'invalid_option', 'delayMs'], [{ pgn: OPERA_PGN, coords: 'yes' }, 400, 'invalid_option', 'coords'],
        [{ pgn: OPERA_PGN, orientation: 1 }, 400, 'invalid_option', 'orientation'], [{ pgn: OPERA_PGN, speed: 2 }, 400, 'invalid_request', 'speed'],
        [{}, 400, 'invalid_request', 'pgn'], [{ pgn: 42 }, 400, 'invalid_request', 'pgn'], [[OPERA_PGN], 400, 'invalid_request', undefined],
    ];
    for (const [body, status, code, field] of badBodies) {
        const r = await postGif(s, body, { token: alice.token });
        assert.deepEqual([r.status, r.json?.error, r.json?.field], [status, code, field], `${JSON.stringify(body).slice(0, 60)}: ${r.text}`);
    }
    assert.equal(s.fp.jobs.length, 0);
    assert.equal(takes(s, 'gif_').length, 0, 'no render quota taken');

    await gif(s, id, { token: alice.token });
    await gif(s, id, { token: alice.token, query: '?size=large&orientation=black&delay=100&coords=0' });
    await postGif(s, { pgn: OPERA_PGN, size: 'small', orientation: 'black', delayMs: 3000, coords: false }, { token: alice.token });
    assert.deepEqual(s.fp.jobs.map((j) => j.options), [
        { size: 'medium', orientation: 'white', delayMs: 500, coords: true },
        { size: 'large', orientation: 'black', delayMs: 100, coords: false },
        { size: 'small', orientation: 'black', delayMs: 3000, coords: false },
    ]);
    assert.deepEqual(gifOptions({ delay: '250', coords: '1' }).options, { size: 'medium', orientation: 'white', delayMs: 250, coords: true });
    assert.equal(gifOptions({ delay: 250 }).error.body.field, 'delay', 'the query form takes text');
    assert.deepEqual(gifOptions({ delay: 250, coords: false }, 'body').options, { size: 'medium', orientation: 'white', delayMs: 250, coords: false });
    assert.equal(gifOptions({ coords: '0' }, 'body').error.body.field, 'coords', 'the body form takes a boolean');
});

test('GET: the job from the stored record (names, ratings, result, ending); the answer is the file', async (t) => {
    const s = await server();
    t.after(s.close);
    const alice = await signedIn(s, 'alice');
    const g = record(alice, { id: 99, username: 'deleted#99' }, OPERA_UCI.split(' ').slice(0, 10).join(' '),
        { status: GameStatus.BlackWins, reason: EndReason.Resignation, whiteRating: null });
    s.store._raw.games.push(g);
    const r = await gif(s, g.id, { token: alice.token });
    assert.equal(r.status, 200);
    assert.equal(r.headers['content-type'], 'image/gif');
    assert.equal(r.headers['content-disposition'], `attachment; filename="scacelith-${g.id}.gif"`);
    assert.equal(Number(r.headers['content-length']), r.bytes.length);
    assert.equal(r.bytes.subarray(0, 6).toString('latin1'), 'GIF89a');
    assert.equal(r.headers['cache-control'], 'no-store');
    const job = s.fp.jobs[0];
    assert.deepEqual(job, {
        startFen: null, moves: Array.from(g.moves), white: { name: 'alice', rating: null }, black: { name: 'deleted#99', rating: 1588 },
        result: '0-1', footer: 'Resignation', options: { size: 'medium', orientation: 'white', delayMs: 500, coords: true },
    });
    const head = await s.request('HEAD', `/api/v1/games/${g.id}/gif`, { token: alice.token, binary: true });
    assert.deepEqual([head.status, head.bytes.length, head.headers['content-length']], [200, 0, r.headers['content-length']]);

    const aborted = record(alice, { id: 99, username: 'bob' }, 'e2e4', { status: GameStatus.Aborted, reason: EndReason.Aborted });
    s.store._raw.games.push(aborted);
    assert.equal((await gif(s, aborted.id, { token: alice.token })).status, 200);
    assert.deepEqual([s.fp.jobs.at(-1).result, s.fp.jobs.at(-1).footer], ['*', 'Game aborted']);
});

test('POST: the first game of the PGN, its tags in the header; invalid PGN with line and column; game length', async (t) => {
    const s = await server({ GIF_MAX_PLIES: '40' });
    t.after(s.close);
    const alice = await signedIn(s, 'alice');
    const pgn = `[Event "Club"]\n[White "Müller, Hans"]\n[Black "Ljubojević ♞"]\n[WhiteElo "2650"]\n[BlackElo "?"]\n[Result "1/2-1/2"]\n`
        + '[Termination "time forfeit"]\n\n1. e4 e5 2. Nf3 Nc6 {a comment} 3. Bb5 (3. Bc4 Bc5) a6 1/2-1/2\n\n[Event "Second"]\n\n1. d4 d5 *\n';
    let r = await postGif(s, { pgn }, { token: alice.token });
    assert.equal(r.status, 200);
    assert.equal(r.headers['content-disposition'], 'attachment; filename="scacelith-game.gif"');
    assert.deepEqual(s.fp.jobs[0], {
        startFen: null, moves: uciMoves('e2e4 e7e5 g1f3 b8c6 f1b5 a7a6'), white: { name: 'Muller, Hans', rating: 2650 },
        black: { name: 'Ljubojevic ?', rating: null }, result: '1/2-1/2', footer: 'Time forfeit',
        options: { size: 'medium', orientation: 'white', delayMs: 500, coords: true },
    });
    await postGif(s, { pgn: '[Termination "Normal"]\n[SetUp "1"]\n[FEN "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1"]\n\n1. e4 Kd7 *' }, { token: alice.token });
    assert.deepEqual([s.fp.jobs[1].startFen, s.fp.jobs[1].footer, s.fp.jobs[1].result, s.fp.jobs[1].white.name],
        ['4k3/8/8/8/8/8/4P3/4K3 w - - 0 1', null, '*', '']);

    r = await postGif(s, { pgn: '[White "a"]\n\n1. e4 e5\n2. Ke3 Nf6 *' }, { token: alice.token });
    assert.deepEqual([r.status, r.json.error, r.json.line, r.json.column], [400, 'invalid_pgn', 4, 4]);
    assert.match(r.json.message, /illegal move 'Ke3'/);
    r = await postGif(s, { pgn: 'just text' }, { token: alice.token });
    assert.deepEqual([r.status, r.json.error, typeof r.json.line, typeof r.json.column], [400, 'invalid_pgn', 'number', 'number']);
    r = await postGif(s, { pgn: '1. e4 ' + '{' + 'x'.repeat(66000) + '} *' }, { token: alice.token });
    assert.deepEqual([r.status, r.json.error, r.json.line, r.json.column], [400, 'invalid_pgn', 1, 1], 'more than 65536 bytes');
    assert.match(r.json.message, /too large/);
    // Above HTTP_BODY_LIMIT (16 KiB): the route's own body limit.
    r = await postGif(s, { pgn: `{${'x'.repeat(40000)}}\n1. d4 d5 *` }, { token: alice.token });
    assert.equal(r.status, 200);

    // GIF_MAX_PLIES = 40: 41 plies answer 422 on both routes.
    const long = 'Nf3 Nf6 Ng1 Ng8 '.repeat(10).trim().split(' ');     // 40 plies
    assert.equal((await postGif(s, { pgn: long.join(' ') + ' *' }, { token: alice.token })).status, 200, '40 plies');
    r = await postGif(s, { pgn: long.join(' ') + ' Nf3 *' }, { token: alice.token });
    assert.deepEqual([r.status, r.json.error], [422, 'game_too_long']);
    const g = record(alice, { id: 99, username: 'bob' }, 'g1f3 g8f6 f3g1 f6g8 '.repeat(10) + 'g1f3', { status: GameStatus.Draw, reason: EndReason.Agreement });
    s.store._raw.games.push(g);
    r = await gif(s, g.id, { token: alice.token });
    assert.deepEqual([r.status, r.json.error], [422, 'game_too_long']);
    assert.match(r.json.message, /41 plies, at most 40/);
    assert.equal(takes(s, 'gif_user_min').length, 4, 'only the four renders took quotas');
});

test('cache: a cached or in-flight GIF costs no render and no render quota; a new name is a new picture', async (t) => {
    const s = await server({ GIF_USER_RENDERS_PER_MIN: '50', GIF_USER_RENDERS_PER_HOUR: '100' });
    t.after(s.close);
    const alice = await signedIn(s, 'alice');
    const g = record(alice, { id: 99, username: 'bob' });
    s.store._raw.games.push(g);
    const hits = counter('scacelith_gif_cache_total', 'hit'), misses = counter('scacelith_gif_cache_total', 'miss');
    const a = await gif(s, g.id, { token: alice.token });
    const b = await gif(s, g.id, { token: alice.token });
    assert.equal(a.status, 200);
    assert.ok(a.bytes.equals(b.bytes));
    assert.equal(s.fp.jobs.length, 1);
    assert.equal(takes(s, 'gif_user_min').length, 1, 'the second request took no render quota');
    assert.equal(counter('scacelith_gif_cache_total', 'hit') - hits, 1);
    assert.equal(counter('scacelith_gif_cache_total', 'miss') - misses, 1);
    await gif(s, g.id, { token: alice.token, query: '?orientation=black' });
    assert.equal(s.fp.jobs.length, 2, 'other options: another picture');
    // The account of black is deleted: its name changes in the record, the old picture is not served.
    s.store._raw.games.find((x) => x.id === g.id).blackName = 'deleted#99';
    const c = await gif(s, g.id, { token: alice.token });
    assert.equal(s.fp.jobs.length, 3);
    assert.equal(s.fp.jobs[2].black.name, 'deleted#99');
    assert.ok(!c.bytes.equals(a.bytes));
    // The same game through POST: the job is the key, never the PGN's text.
    await postGif(s, { pgn: OPERA_PGN }, { token: alice.token });
    await postGif(s, { pgn: OPERA_PGN.replace('[Site "Paris FRA"]\n', '') + '\n\n' }, { token: alice.token });
    assert.equal(s.fp.jobs.length, 4, 'a PGN that differs only by tags the picture does not show');

    // Two requests at once for one GIF: one render.
    const pool = s.fp.pools[0];
    pool.mode = 'hold';
    const p1 = gif(s, g.id, { token: alice.token, query: '?delay=700' });
    for (let i = 0; i < 400 && pool.held.length === 0; i++) await new Promise((r) => setTimeout(r, 5));
    assert.equal(pool.held.length, 1, 'the first request is rendering');
    const quota = takes(s, 'gif_user_min').length;
    const p2 = gif(s, g.id, { token: alice.token, query: '?delay=700' });
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(pool.held.length, 1, 'the second waits for the same render');
    assert.equal(takes(s, 'gif_user_min').length, quota, 'and takes no quota');
    pool.held.forEach((f) => f());
    const [r1, r2] = await Promise.all([p1, p2]);
    assert.deepEqual([r1.status, r2.status], [200, 200]);
    assert.ok(r1.bytes.equals(r2.bytes));
    assert.equal(s.fp.jobs.length, 5);

    assert.equal(gifCacheKey({ moves: [1, 2], options: {} }), gifCacheKey({ moves: Uint16Array.from([1, 2]), options: {} }));
    assert.notEqual(gifCacheKey({ moves: [1, 2], options: {} }), gifCacheKey({ moves: [1, 2], options: {}, footer: 'x' }));
});

test('GifCache: bounded in bytes, least recently used out, no single GIF above a quarter; 0 disables it', async (t) => {
    const c = new GifCache(1000);
    c.set('a', Buffer.alloc(200)); c.set('b', Buffer.alloc(200)); c.set('c', Buffer.alloc(200));
    c.get('a');
    c.set('d', Buffer.alloc(250)); c.set('e', Buffer.alloc(240));
    assert.deepEqual([...c.map.keys()], ['c', 'a', 'd', 'e']);
    assert.equal(c.bytes, 890);
    assert.equal(c.set('big', Buffer.alloc(251)), false);
    assert.equal(new GifCache(0).set('x', Buffer.alloc(1)), false);

    const s = await server({ GIF_CACHE_MB: '0' });
    t.after(s.close);
    const alice = await signedIn(s, 'alice');
    const g = record(alice, { id: 99, username: 'bob' });
    s.store._raw.games.push(g);
    await gif(s, g.id, { token: alice.token });
    await gif(s, g.id, { token: alice.token });
    assert.equal(s.fp.jobs.length, 2, 'every request renders without a cache');
});

/** Two workers of one server: two handlers (each its pool and cache), one primary, store and clock. */
async function twoWorkers(env = {}) {
    const a = await server(env);
    const b = await server(env, { shared: { primary: a.primary, store: a.store, now: a.now, hasher: a.hasher } });
    return { a, b, close: async () => { await b.close(); await a.close(); } };
}

test('render quotas per account, whole server: 4 per minute and 30 per hour (429 with Retry-After)', async (t) => {
    const w = await twoWorkers();
    t.after(w.close);
    const { a, b } = w;
    const alice = await signedIn(a, 'alice'), bob = await signedIn(a, 'bob');
    const g = record(alice, bob);
    a.store._raw.games.push(g);
    let delay = 100;
    const next = (s, who = alice) => gif(s, g.id, { token: who.token, query: `?delay=${delay++}` });
    for (const s of [a, b, a, b]) assert.equal((await next(s)).status, 200);
    const before = counter('scacelith_http_rate_limited_total', 'gif_user_min');
    let r = await next(a);
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited'], 'the fifth of the minute, whatever the worker');
    assert.equal(r.headers['retry-after'], String(r.json.retryAfter));
    assert.ok(r.json.retryAfter >= 1 && r.json.retryAfter <= 60);
    assert.equal(counter('scacelith_http_rate_limited_total', 'gif_user_min'), before + 1);
    assert.equal((await gif(a, g.id, { token: alice.token, query: '?delay=100' })).status, 200, 'a cached GIF is still served');
    assert.equal((await next(b, bob)).status, 200, 'another account');
    assert.ok(takes(a, `gif_user_min:u${alice.id}`).every((c) => c.payload.limit === 4 && c.payload.windowMs === 60000));

    // The hour: 26 more at 3 a minute (30 in all), then the hourly quota. (The primary counts over
    // a sliding minute: two minutes later the first one no longer counts.)
    a.now.advance(120000);
    for (let i = 0; i < 26; i++) {
        r = await next(i % 2 ? a : b);
        assert.equal(r.status, 200, `render ${5 + i}`);
        a.now.advance(20000);
    }
    a.now.advance(60000);
    const hour = counter('scacelith_http_rate_limited_total', 'gif_user_hour');
    r = await next(a);
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited'], 'the 31st of the hour');
    assert.equal(counter('scacelith_http_rate_limited_total', 'gif_user_hour'), hour + 1);
    assert.ok(r.json.retryAfter > 60);
});

test('render quotas per address: accounts behind one address share 12 a minute (IPv6: 3 times per /48)', async (t) => {
    const s = await server({ GIF_IP_RENDERS_PER_MIN: '2', GIF_IP_RENDERS_PER_HOUR: '100', GIF_USER_RENDERS_PER_MIN: '50', GIF_USER_RENDERS_PER_HOUR: '100' });
    t.after(s.close);
    const users = [];
    for (const n of ['alice', 'bob', 'carol', 'dave']) users.push(await signedIn(s, n));
    const g = record(users[0], users[1]);
    s.store._raw.games.push(g);
    let delay = 100;
    const next = (u, ip) => gif(s, g.id, { token: u.token, ip, query: `?delay=${delay++}` });
    assert.equal((await next(users[0], '192.0.2.9')).status, 200);
    assert.equal((await next(users[1], '192.0.2.9')).status, 200);
    const r = await next(users[2], '192.0.2.9');
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited'], 'a third account behind the same address');
    assert.equal((await next(users[2], '192.0.2.10')).status, 200, 'another address');
    for (let net = 1; net <= 3; net++) {
        assert.equal((await next(users[net - 1], `2001:db8:9:${net}::1`)).status, 200);
        assert.equal((await next(users[net], `2001:db8:9:${net}::2`)).status, 200);
    }
    assert.equal((await next(users[3], '2001:db8:9:4::1')).status, 429, 'the /48: 6');
    assert.ok(takes(s, 'gif_ip_min/48:2001:db8:9::/48').length >= 6);
});

test('busy pool: 503 server_busy with Retry-After, and every token of the request is given back', async (t) => {
    const s = await server({ GIF_USER_RENDERS_PER_MIN: '1', GIF_USER_RENDERS_PER_HOUR: '1', GIF_IP_RENDERS_PER_MIN: '1', GIF_IP_RENDERS_PER_HOUR: '1' });
    t.after(s.close);
    const alice = await signedIn(s, 'alice');
    const g = record(alice, { id: 99, username: 'bob' });
    s.store._raw.games.push(g);
    // Make the pool exist (bob, from another address: alice's quotas stay whole), then make it busy.
    const bob = await signedIn(s, 'bob');
    assert.equal((await postGif(s, { pgn: '1. e4 *' }, { token: bob.token, ip: '198.51.100.50' })).status, 200);
    const pool = s.fp.pools[0];
    pool.mode = 'busy';
    const busy = counter('scacelith_gif_renders_total', 'busy');
    for (let i = 0; i < 31; i++) {
        const r = await gif(s, g.id, { token: alice.token });
        assert.deepEqual([r.status, r.json.error], [503, 'server_busy'], `attempt ${i}`);
        assert.ok(r.json.retryAfter >= 3 && r.json.retryAfter <= 10);
        assert.equal(r.headers['retry-after'], String(r.json.retryAfter));
    }
    assert.equal(counter('scacelith_gif_renders_total', 'busy') - busy, 31);
    const refunded = new Set(s.primary.calls.filter((c) => c.type === 'ratelimit.refund').map((c) => c.payload.key));
    assert.deepEqual([...refunded].sort(), [`gif_ip_hour:127.0.0.1`, 'gif_ip_min:127.0.0.1', `gif_user_hour:u${alice.id}`, `gif_user_min:u${alice.id}`].sort());
    pool.mode = 'ok';
    // 31 attempts > the route's 30 per minute, every quota is 1, and no time passed: all of it
    // was given back each time.
    const r = await gif(s, g.id, { token: alice.token });
    assert.equal(r.status, 200);
    assert.equal((await gif(s, g.id, { token: alice.token, query: '?delay=900' })).status, 429, 'the quotas are spent by the render');
});

test('a failed render answers 500 render_failed and keeps the quotas', async (t) => {
    const s = await server({ GIF_USER_RENDERS_PER_MIN: '1' });
    t.after(s.close);
    const alice = await signedIn(s, 'alice');
    const g = record(alice, { id: 99, username: 'bob' });
    s.store._raw.games.push(g);
    await postGif(s, { pgn: '1. d4 *' }, { token: alice.token });       // creates the pool (1 render: the minute's quota)
    s.now.advance(120000);
    s.fp.pools[0].mode = 'fail';
    const failed = counter('scacelith_gif_renders_total', 'failed');
    let r = await gif(s, g.id, { token: alice.token });
    assert.deepEqual([r.status, r.json.error], [500, 'render_failed']);
    assert.equal(counter('scacelith_gif_renders_total', 'failed') - failed, 1);
    s.fp.pools[0].mode = 'ok';
    r = await gif(s, g.id, { token: alice.token });
    assert.equal(r.status, 429, 'the failed render counted');
});

test('the pool is created on the first render with the GIF_* settings, and closed with the handler', async (t) => {
    const s = await server({ GIF_THREADS: '2', GIF_QUEUE_MAX: '7', GIF_QUEUE_TIMEOUT_MS: '1234', GIF_RENDER_TIMEOUT_MS: '5678' });
    const alice = await signedIn(s, 'alice');
    assert.equal(s.fp.pools.length, 0);
    assert.equal((await postGif(s, { pgn: 'x' }, { token: alice.token })).status, 400);
    assert.equal(s.fp.pools.length, 0, 'not for a refused request');
    await postGif(s, { pgn: '1. e4 *' }, { token: alice.token });
    await postGif(s, { pgn: '1. d4 *' }, { token: alice.token });
    assert.equal(s.fp.pools.length, 1);
    assert.deepEqual(s.fp.pools[0].opts, { threads: 2, queueMax: 7, timeoutMs: 1234, renderTimeoutMs: 5678, workerUrl: GIF_THREAD_URL });
    const route = s.handler.router.routes.find((r) => r.path === '/api/v1/gif');
    assert.equal(route.opts.timeoutMs, 1234 + 5678 + 5000, 'the handler waits for the queue and the render');
    await s.close();
    assert.equal(s.fp.pools[0].closed, true);
    assert.equal(tagText('Ünïcödé ✓  name'), 'Unicode ? name');
});

/** The nice value of each thread of this process (Linux), by thread id. */
function threadNices() {
    const out = new Map();
    for (const tid of fs.readdirSync('/proc/self/task')) {
        try {
            const st = fs.readFileSync(`/proc/self/task/${tid}/stat`, 'utf8');
            out.set(Number(tid), Number(st.slice(st.lastIndexOf(')') + 2).split(' ')[16]));
        } catch { /* the thread ended */ }
    }
    return out;
}

test('the rendering thread runs at the lowest priority on Linux; the rest of the process keeps its own', { skip: process.platform !== 'linux' }, async (t) => {
    assert.equal(GIF_THREAD_NICE, 19);
    const before = threadNices();
    // This file imported the thread module: the main thread still has the nice value of the threads
    // that Node started with it (whatever the test runner's own priority).
    for (const [tid, nice] of before) assert.equal(nice, os.getPriority(), `thread ${tid} at the process's priority`);
    const pool = createGifPool({ threads: 1, workerUrl: GIF_THREAD_URL });
    t.after(() => pool.close());
    const gifBytes = await pool.render({ moves: [], options: { size: 'small' } });
    assert.equal(gifBytes.subarray(0, 6).toString(), 'GIF89a', 'the thread renders');
    const after = threadNices();
    const started = [...after].filter(([tid]) => !before.has(tid));
    assert.deepEqual(started.filter(([, nice]) => nice === GIF_THREAD_NICE).length, 1, 'one rendering thread at nice 19');
    assert.equal(after.get(process.pid), before.get(process.pid), 'the event loop keeps its priority');
    for (const [tid, nice] of after) if (before.has(tid)) assert.equal(nice, before.get(tid), `thread ${tid} unchanged`);
});

function applyGame(white, black, score) {
    const d = Math.round(20 * (score - 0.5));
    const side = (r, delta, sc) => ({
        before: r.rating, after: r.rating + delta,
        record: { rating: r.rating + delta, games: r.games + 1, wins: r.wins + (sc === 1 ? 1 : 0), draws: r.draws + (sc === 0.5 ? 1 : 0),
            losses: r.losses + (sc === 0 ? 1 : 0), peak: Math.max(r.peak, r.rating + delta), reachedSenior: false },
    });
    return { white: side(white, d, score), black: side(black, -d, 1 - score) };
}

test('a game of the SQLite store rendered by the real thread pool: one frame per position; its own PGN gives the same GIF', async (t) => {
    const config = testConfig({ DB_PATH: ':memory:', HTTP_RATE_PER_IP: '100000', SERVER_PUBLIC_HOST: 'chess.example.org' });
    const store = openStore(config, { applyGame });
    migrate(store);
    const white = { id: store.users.create({ username: 'Morphy', email: 'm@example.org' }), username: 'Morphy' };
    const black = { id: store.users.create({ username: 'Isouard', email: 'i@example.org' }), username: 'Isouard' };
    const rec = record(white, black, OPERA_UCI, { rated: false });
    const cg = ChessGame.fromMoves(undefined, rec.moves);
    assert.ok(cg && cg.isOver, 'the Opera game ends in checkmate');
    store.games.finishBatch([rec]);
    const token = `sct_${'m'.repeat(43)}`;
    const auth = { validateToken: (x) => (x === token ? { userId: white.id, username: 'Morphy', sessionId: 1, emailVerified: true, tokenHash: 'h' } : null) };
    const handler = createApiHandler({ config, store, auth, log: logger.child('gif-test'), routes: [gifRoutes] });
    const srv = http.createServer(handler);
    await new Promise((r) => srv.listen(0, '127.0.0.1', r));
    t.after(async () => { await handler.close(); srv.closeAllConnections?.(); await new Promise((r) => srv.close(r)); store.close(); });
    const get = (path, body) => new Promise((resolve, reject) => {
        const payload = body ? Buffer.from(JSON.stringify(body)) : null;
        const headers = { Authorization: `Bearer ${token}`, ...(payload ? { 'Content-Type': 'application/json', 'Content-Length': payload.length } : {}) };
        const r = http.request({ host: '127.0.0.1', port: srv.address().port, method: payload ? 'POST' : 'GET', path, headers, agent: false }, (res) => {
            const c = [];
            res.on('data', (d) => c.push(d));
            res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, bytes: Buffer.concat(c) }));
        });
        r.on('error', reject);
        r.end(payload || undefined);
    });
    const r = await get(`/api/v1/games/${rec.id}/gif?size=small&delay=200`);
    assert.equal(r.status, 200, r.bytes.toString());
    const decoded = decodeGif(new Uint8Array(r.bytes));
    assert.equal(decoded.frames.length, rec.moves.length + 1, 'the start position and one frame per move');
    assert.equal(decoded.loop, 0, 'loops forever');
    assert.equal(decoded.frames[1].delayCs, 20);
    assert.equal(decoded.frames.at(-1).delayCs, 300, 'the final position held 3 s');
    assert.equal(decoded.width, 284, 'small, with coordinates');

    const pgn = gamePgn(store.games.byId(rec.id), config);
    const p = await get('/api/v1/gif', { pgn, size: 'small', delayMs: 200 });
    assert.equal(p.status, 200);
    assert.equal(decodeGif(new Uint8Array(p.bytes)).frames.length, rec.moves.length + 1);
});
