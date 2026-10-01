import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openStore, migrate } from '../../src/store/index.js';
import { register, uciOf } from '../../src/http/routes/players.js';
import { testConfig } from '../../src/config.js';
import { enums } from '../../src/protocol/schema.js';

const { GameStatus, EndReason } = enums;

function applyGame(white, black, score) {
    const d = Math.round(20 * (score - 0.5));
    const side = (r, delta, s) => ({
        before: r.rating, after: r.rating + delta,
        record: { rating: r.rating + delta, games: r.games + 1, wins: r.wins + (s === 1 ? 1 : 0), draws: r.draws + (s === 0.5 ? 1 : 0),
            losses: r.losses + (s === 0 ? 1 : 0), peak: Math.max(r.peak, r.rating + delta), reachedSenior: false },
    });
    return { white: side(white, d, score), black: side(black, -d, 1 - score) };
}

// Fake router capturing registrations (the real one is src/http/router.js).
function fakeRouter() {
    const routes = [];
    const reg = (method) => (p, handler, opts) => routes.push({ method, path: p, handler, opts });
    return { routes, get: reg('GET'), post: reg('POST'), put: reg('PUT'), delete: reg('DELETE') };
}

function setup() {
    const config = testConfig({ DB_PATH: ':memory:', PROVISIONAL_GAMES: '2', SERVER_NAME: 'Test Server', SERVER_PUBLIC_HOST: 'chess.example.org' });
    const store = openStore(config, { applyGame });
    migrate(store);
    const router = fakeRouter();
    register(router, { store, config });
    const call = (p, params = {}, query = '') => {
        const r = router.routes.find((x) => x.path === p);
        assert.ok(r, `route ${p}`);
        return r.handler({ params, query: new URLSearchParams(query), store, config });
    };
    return { config, store, router, call };
}

let nextId = 2_000_000_000_000;
function game(white, black, extra = {}) {
    const moves = extra.moves ?? Uint16Array.from([12 | (28 << 6), 52 | (36 << 6), 6 | (21 << 6), 57 | (42 << 6)]);
    return {
        id: ++nextId, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: white.id, blackId: black.id,
        whiteName: white.name, blackName: black.name, whiteRating: 1500, blackRating: 1500, startedAt: Date.UTC(2026, 8, 28, 12),
        endedAt: Date.UTC(2026, 8, 28, 12, 10), status: GameStatus.WhiteWins, reason: EndReason.Resignation, moves,
        spentMs: Uint32Array.from([0, 0, 1500, 2100]), clockMs: Uint32Array.from([180000, 180000, 180500, 179900]), rematchOf: 0, flags: 0,
        ...extra,
    };
}

test('registers the five public routes; all but the leaderboard take an optional session', () => {
    const { router } = setup();
    assert.deepEqual(router.routes.map((r) => `${r.method} ${r.path} ${r.opts.auth}`), [
        'GET /api/v1/players/:username optional', 'GET /api/v1/players/:username/games optional', 'GET /api/v1/games/:id optional',
        'GET /api/v1/games/:id/pgn optional', 'GET /api/v1/leaderboard none']);
    const rates = router.routes.filter((r) => r.opts.rate).map((r) => r.opts.rate);
    assert.deepEqual(rates.map((r) => r.key), ['public_read', 'public_read', 'public_read', 'public_read'],
        'the profile, the game lists, the record and its PGN share one limit');
    assert.ok(rates.every((r) => r.by === 'user'), 'per account when signed in (per client otherwise)');
    const other = fakeRouter();
    register(other, { store: {}, config: testConfig(), prefix: '' });
    assert.equal(other.routes[0].path, '/players/:username');
});

test('uciOf: squares and promotions from the u16 move', () => {
    assert.equal(uciOf(12 | (28 << 6)), 'e2e4');
    assert.equal(uciOf(4 | (6 << 6)), 'e1g1');
    assert.equal(uciOf(52 | (60 << 6) | (5 << 12)), 'e7e8q');
    assert.equal(uciOf(9 | (0 << 6) | (2 << 12)), 'b2a1n');
    assert.equal(uciOf(63 | (0 << 6)), 'h8a1');
});

test('profile: public data only, ratings with provisional flag, counts; validation and 404s', () => {
    const { store, call } = setup();
    const a = { id: store.users.create({ username: 'Alice', email: 'alice@secret.org', passwordHash: 'x' }), name: 'Alice' };
    const b = { id: store.users.create({ username: 'Bob', email: 'bob@secret.org' }), name: 'Bob' };
    store.games.finishBatch([game(a, b), game(b, a, { status: GameStatus.Draw }), game(a, b, { category: '5+0', baseMs: 300000, incMs: 0 }),
        game(a, b, { rated: false })]);
    store.sanctions.create({ userId: a.id, kind: 'warning', source: 'moderator', reason: 'secret reason' });
    store.integrity.set(a.id, { level: 'suspected', score: 3 });

    const res = call('/api/v1/players/:username', { username: 'alice' });
    assert.equal(res.status, 200);
    const body = res.body;
    assert.equal(body.username, 'Alice');
    assert.ok(body.createdAt > 0);
    assert.deepEqual(body.ratings.map((r) => [r.category, r.games, r.provisional]), [['3+2', 2, false], ['5+0', 1, true]],
        'config order, played categories only');
    assert.equal(body.ratings[0].rating, 1500 + 10 + 0);
    assert.deepEqual(body.games, { total: 4, rated: 3, wins: 2, draws: 1, losses: 0 });
    const text = JSON.stringify(body);
    for (const secret of ['secret', 'email', 'suspected', 'integrity', 'sanction', 'password', 'x@', 'alice@']) {
        assert.ok(!text.includes(secret), `no ${secret} in the profile`);
    }

    assert.equal(call('/api/v1/players/:username', { username: 'nobody' }).status, 404);
    assert.equal(call('/api/v1/players/:username', { username: 'a' }).body.error, 'invalid_username');
    assert.equal(call('/api/v1/players/:username', { username: 'x'.repeat(25) }).status, 400);
    assert.equal(call('/api/v1/players/:username', { username: "bob'; DROP TABLE users;--" }).status, 400);
    assert.equal(call('/api/v1/players/:username', { username: 'Al%69ce' }).body.username, 'Alice', 'percent-decoded');
    store.users.anonymize(b.id);
    assert.equal(call('/api/v1/players/:username', { username: 'bob' }).status, 404);
    assert.equal(call('/api/v1/players/:username', { username: `deleted#${b.id}` }).status, 400);
    store.close();
});

test('recent games: newest first, before cursor, limit cap, player colour', () => {
    const { store, call } = setup();
    const a = { id: store.users.create({ username: 'Carol', email: 'c@e.org' }), name: 'Carol' };
    const b = { id: store.users.create({ username: 'Dave', email: 'd@e.org' }), name: 'Dave' };
    const list = [];
    for (let i = 0; i < 60; i++) list.push(i % 2 ? game(a, b, { rated: false }) : game(b, a, { rated: false }));
    store.games.finishBatch(list);
    const p = '/api/v1/players/:username/games';
    let res = call(p, { username: 'carol' });
    assert.equal(res.status, 200);
    assert.equal(res.body.games.length, 20);
    assert.equal(res.body.games[0].id, list[59].id);
    assert.equal(res.body.games[0].color, 'white');
    assert.equal(res.body.games[1].color, 'black');
    assert.equal(res.body.games[0].result, '1-0');
    assert.equal(res.body.games[0].termination, 'Resignation');
    assert.equal(res.body.games[0].timeControl, '180+2');
    assert.equal(res.body.next, list[40].id);
    res = call(p, { username: 'carol' }, `before=${res.body.next}&limit=50`);
    assert.equal(res.body.games.length, 40);
    assert.equal(res.body.games[0].id, list[39].id);
    assert.equal(res.body.next, null);
    assert.equal(call(p, { username: 'carol' }, 'limit=500').body.games.length, 50, 'capped at 50');
    assert.equal(call(p, { username: 'carol' }, 'before=abc').body.error, 'invalid_cursor');
    assert.equal(call(p, { username: 'carol' }, 'before=-5').status, 400);
    assert.equal(call(p, { username: 'carol' }, 'before=99999999999999999999').status, 400);
    assert.equal(call(p, { username: 'carol' }, 'limit=0').body.error, 'invalid_limit');
    assert.equal(call(p, { username: 'zed' }).status, 404);
    store.close();
});

test('game record: players, result, UCI moves with times, PGN tags, rating changes', () => {
    const { store, call } = setup();
    const a = { id: store.users.create({ username: 'Erin', email: 'e@e.org' }), name: 'Erin' };
    const b = { id: store.users.create({ username: 'Finn', email: 'f@e.org' }), name: 'Finn' };
    const g = game(a, b);
    store.games.finishBatch([g]);
    const res = call('/api/v1/games/:id', { id: String(g.id) });
    assert.equal(res.status, 200);
    const body = res.body;
    assert.equal(body.id, g.id);
    assert.deepEqual(body.moves.map((m) => m.uci), ['e2e4', 'e7e5', 'g1f3', 'b8c6']);
    assert.deepEqual(body.moves[2], { uci: 'g1f3', spentMs: 1500, clockMs: 180500 });
    assert.deepEqual(body.white, { name: 'Erin', rating: 1500, ratingAfter: 1510, ratingDiff: 10 });
    assert.deepEqual(body.black, { name: 'Finn', rating: 1500, ratingAfter: 1490, ratingDiff: -10 });
    assert.equal(body.result, '1-0');
    assert.equal(body.statusName, 'WhiteWins');
    assert.equal(body.termination, 'Resignation');
    assert.equal(body.plies, 4);
    assert.equal(body.rematchOf, null);
    assert.equal(body.color, undefined);
    assert.deepEqual(body.pgn, {
        Event: 'Test Server rated 3+2', Site: 'chess.example.org', Date: '2026.09.28', Round: '-', White: 'Erin', Black: 'Finn',
        Result: '1-0', WhiteElo: 1500, BlackElo: 1500, TimeControl: '180+2', Termination: 'Resignation', PlyCount: 4,
    });
    assert.ok(!JSON.stringify(body).includes('@'), 'no e-mail address');

    const casual = game(a, b, { rated: false, status: GameStatus.Aborted, reason: EndReason.NoShow, moves: new Uint16Array(0),
        spentMs: new Uint32Array(0), clockMs: new Uint32Array(0) });
    store.games.finishBatch([casual]);
    const c = call('/api/v1/games/:id', { id: String(casual.id) }).body;
    assert.equal(c.result, '*');
    assert.equal(c.termination, 'NoShow');
    assert.deepEqual(c.moves, []);
    assert.equal(c.white.ratingDiff, null);

    assert.equal(call('/api/v1/games/:id', { id: '12345' }).status, 404);
    for (const bad of ['abc', '0', '-1', '1.5', '99999999999999999', '', '12e3']) {
        assert.equal(call('/api/v1/games/:id', { id: bad }).body.error, 'invalid_game_id', bad);
    }
    store.close();
});

test('leaderboard: official categories only, established players, "+" spellings, cache', () => {
    const { store, call } = setup();
    const p = [];
    for (const n of ['Gil', 'Hal', 'Ivy', 'Jon']) p.push({ id: store.users.create({ username: n, email: `${n}@e.org` }), name: n });
    const [g, h, i, j] = p;
    store.games.finishBatch([game(g, h), game(g, h), game(g, i), game(i, h), game(j, g)]);
    // Ratings: Gil 1500+10+10+10-10=1520 (4 games), Hal 1480-10=1470 (3), Ivy 1490+10=1500 (2), Jon 1510 (1 game).
    let res = call('/api/v1/leaderboard', {}, 'category=3+2');           // '+' decodes to a space
    assert.equal(res.status, 200);
    assert.equal(res.body.category, '3+2');
    assert.equal(res.body.minGames, 2);
    assert.deepEqual(res.body.players.map((x) => [x.rank, x.username, x.rating]), [[1, 'Gil', 1520], [2, 'Ivy', 1500], [3, 'Hal', 1470]]);
    assert.ok(!('userId' in res.body.players[0]));
    res = call('/api/v1/leaderboard', {}, 'category=3%2B2&limit=1');
    assert.equal(res.body.players.length, 1);
    assert.equal(call('/api/v1/leaderboard', {}, 'category=4%2B2').body.error, 'invalid_category');
    assert.equal(call('/api/v1/leaderboard', {}, 'category=custom').status, 400);
    assert.equal(call('/api/v1/leaderboard', {}, '').status, 400);
    assert.equal(call('/api/v1/leaderboard', {}, 'category=5%2B0').body.players.length, 0);
    // Cached for a few seconds: a new result does not show at once.
    store.games.finishBatch([game(j, h), game(j, h)]);
    assert.equal(call('/api/v1/leaderboard', {}, 'category=3%2B2').body.players.length, 3);
    // Plain-object queries work too.
    const r = setup();
    const route = r.router.routes.find((x) => x.path === '/api/v1/leaderboard');
    assert.equal(route.handler({ params: {}, query: { category: '3+2' } }).status, 200);
    r.store.close();
    store.close();
});
