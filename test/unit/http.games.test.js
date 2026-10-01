// The game routes of the account API through the real HTTP handler and a real (in-memory) store:
// GET /api/v1/games/:id/pgn (the PGN export and its fixtures for the game's reader),
// GET /api/v1/games/:id with an optional session (you, reportable), GET /api/v1/account/games.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { createApiHandler } from '../../src/http/server.js';
import * as playerRoutes from '../../src/http/routes/players.js';
import * as reportRoutes from '../../src/http/routes/reports.js';
import * as accountGameRoutes from '../../src/http/routes/account-games.js';
import { gamePgn, pgnClock, PGN_CONTENT_TYPE } from '../../src/http/routes/players.js';
import { historySummary, outcomeFor, parseHistoryQuery } from '../../src/http/routes/account-games.js';
import { DEFAULT_ROUTES } from '../../src/http/server.js';
import { openStore, migrate } from '../../src/store/index.js';
import { ChessGame } from '../../src/chess/index.js';
import { testConfig } from '../../src/config.js';
import { enums } from '../../src/protocol/schema.js';
import { FIXTURES_DIR, FIXTURE_CONFIG, fixtureGames, renderFixtures } from '../../tools/gen-pgn-fixtures.js';

const { GameStatus, EndReason } = enums;
const NOW = Date.UTC(2026, 9, 1, 12, 0, 0);
const HOUR = 3600000;
const DAY = 24 * HOUR;

function applyGame(white, black, score) {
    const d = Math.round(20 * (score - 0.5));
    const side = (r, delta, s) => ({
        before: r.rating, after: r.rating + delta,
        record: { rating: r.rating + delta, games: r.games + 1, wins: r.wins + (s === 1 ? 1 : 0), draws: r.draws + (s === 0.5 ? 1 : 0),
            losses: r.losses + (s === 0 ? 1 : 0), peak: Math.max(r.peak, r.rating + delta), reachedSenior: false },
    });
    return { white: side(white, d, score), black: side(black, -d, 1 - score) };
}

function captureLog() {
    const lines = [];
    const log = {
        lines, debugEnabled: false,
        error: (msg, f) => lines.push(['error', msg, f]), warn: (msg, f) => lines.push(['warn', msg, f]),
        info() {}, debug() {}, security: (msg, f) => lines.push(['security', msg, f]),
    };
    log.child = () => log;
    return log;
}

// The real API handler on 127.0.0.1 with a real in-memory store; tokens map to users.
async function start() {
    const config = testConfig({ DB_PATH: ':memory:', HTTP_RATE_PER_IP: '100000', SERVER_NAME: 'Test Server', SERVER_PUBLIC_HOST: 'chess.example.org',
        REPORTS_PER_DAY: '2' });
    const store = openStore(config, { applyGame });
    migrate(store);
    const tokens = new Map();
    const auth = { validateToken: (t) => tokens.get(t) || null };
    const log = captureLog();
    let t = NOW;
    const now = () => t;
    const handler = createApiHandler({ config, store, auth, log, now, routes: [playerRoutes, reportRoutes, accountGameRoutes] });
    const server = http.createServer(handler);
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const { port } = server.address();
    const req = (method, p, { token, body } = {}) => new Promise((resolve, reject) => {
        const headers = {};
        let payload = null;
        if (token) headers.Authorization = `Bearer ${token}`;
        if (body !== undefined) { payload = Buffer.from(JSON.stringify(body)); headers['Content-Type'] = 'application/json'; headers['Content-Length'] = payload.length; }
        const r = http.request({ host: '127.0.0.1', port, method, path: p, headers, agent: false }, (res) => {
            const c = [];
            res.on('data', (d) => c.push(d));
            res.on('end', () => {
                const text = Buffer.concat(c).toString('utf8');
                let json; try { json = JSON.parse(text); } catch { /* text answer */ }
                resolve({ status: res.statusCode, headers: res.headers, text, json });
            });
        });
        r.on('error', reject);
        if (payload) r.write(payload);
        r.end();
    });
    const user = (name) => {
        const id = store.users.create({ username: name, email: `${name.toLowerCase()}@example.org`, createdAt: NOW - 400 * DAY });
        const token = `sct_${name.toLowerCase().padEnd(43, 'x')}`;
        tokens.set(token, { userId: id, username: name, sessionId: id, emailVerified: true, tokenHash: `h-${name}` });
        return { id, name, token };
    };
    const close = () => new Promise((r) => { server.closeAllConnections?.(); server.close(() => { store.close(); r(); }); });
    return { config, store, req, user, log, close, setNow: (v) => { t = v; } };
}

let nextId = 5_000_000_000_000;
const SPENT = [0, 0, 1530, 2210, 4120, 980, 3333, 2047, 1500, 999, 1200, 1300];
function record(white, black, uci, extra = {}) {
    const g = new ChessGame();
    const moves = uci ? uci.split(' ').map((u) => { const m = g.position.parseUCI(u); assert.ok(g.play(m).ok, u); return m; }) : [];
    const clocks = [180000, 180000];
    const spentMs = [], clockMs = [];
    moves.forEach((_, i) => {
        const s = i < 2 ? 0 : SPENT[i % SPENT.length];
        if (i >= 2) clocks[i % 2] += 2000 - s;
        spentMs.push(s);
        clockMs.push(clocks[i % 2]);
    });
    return {
        id: ++nextId, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: white.id, blackId: black.id, whiteName: white.name,
        blackName: black.name, whiteRating: 1500, blackRating: 1500, startedAt: NOW - 2 * HOUR, endedAt: NOW - HOUR,
        status: GameStatus.WhiteWins, reason: EndReason.Resignation, moves: Uint16Array.from(moves), spentMs: Uint32Array.from(spentMs),
        clockMs: Uint32Array.from(clockMs), rematchOf: 0, flags: 0, ...extra,
    };
}

// ---- a small PGN reader (tags, SAN replay, [%clk] / [%emt]) for the round trip -------------------

function readPgn(text) {
    const split = text.indexOf('\n\n');
    const tags = [];
    for (const line of text.slice(0, split).split('\n')) {
        const m = /^\[([A-Za-z0-9_]+) "((?:[^"\\]|\\.)*)"\]$/.exec(line);
        assert.ok(m, `tag line ${line}`);
        tags.push([m[1], m[2].replace(/\\(.)/g, '$1')]);
    }
    const game = new ChessGame();
    const plies = [];
    let result = null, comment = '';
    const time = (s) => { const m = /^(\d+):(\d\d):(\d\d)\.(\d)$/.exec(s); assert.ok(m, `time ${s}`); return ((+m[1] * 60 + +m[2]) * 60 + +m[3]) * 1000 + +m[4] * 100; };
    for (const tok of text.slice(split + 2).replace(/\n/g, ' ').match(/\{[^}]*\}|\S+/g)) {
        if (tok.startsWith('{')) {
            let rest = tok.slice(1, -1);
            rest = rest.replace(/\[%(clk|emt) ([^\]]+)\]/g, (_, k, v) => { plies[plies.length - 1][k] = time(v); return ''; });
            if (rest.trim()) comment += rest.trim().replace(/\s+/g, ' ');
        } else if (/^\d+\.(\.\.)?$/.test(tok)) {
            continue;
        } else if (/^(1-0|0-1|1\/2-1\/2|\*)$/.test(tok)) {
            result = tok;
        } else {
            const m = game.position.legalMoves().find((x) => game.position.san(x) === tok);
            assert.ok(m !== undefined, `SAN ${tok} in ${game.position.fen()}`);
            game.play(m);
            plies.push({ san: tok, move: m });
        }
    }
    return { tags, tag: (n) => tags.find(([k]) => k === n)?.[1], plies, result, comment };
}

// ---- the PGN text ----------------------------------------------------------------------------------

test('pgnClock: h:mm:ss.f, tenths truncated', () => {
    assert.equal(pgnClock(0), '0:00:00.0');
    assert.equal(pgnClock(99), '0:00:00.0');
    assert.equal(pgnClock(178_649), '0:02:58.6');
    assert.equal(pgnClock(59_999), '0:00:59.9');
    assert.equal(pgnClock(3_600_000 + 61_000), '1:01:01.0');
    assert.equal(pgnClock(36_000_000 * 3 + 5), '30:00:00.0');
    assert.equal(pgnClock(-5), '0:00:00.0');
});

test('the server PGN: tag order, SAN, clocks, wrapping, endings, deleted players (fixtures round trip)', () => {
    const games = fixtureGames();
    assert.ok(games.length >= 6);
    const ORDER = ['Event', 'Site', 'Date', 'Round', 'White', 'Black', 'Result', 'UTCDate', 'UTCTime', 'WhiteElo', 'BlackElo',
        'WhiteRatingDiff', 'BlackRatingDiff', 'TimeControl', 'Termination', 'PlyCount', 'ScacelithGameId'];
    const seen = { termination: new Set(), flags: 0 };
    for (const g of games) {
        const text = gamePgn(g, FIXTURE_CONFIG);
        assert.ok(!text.includes('\r'), 'LF line endings');
        assert.ok(text.endsWith('\n') && !text.endsWith('\n\n'));
        for (const line of text.split('\n')) assert.ok(line.length < 80, `${g.file}: ${line}`);
        const p = readPgn(text);
        const names = p.tags.map(([k]) => k);
        assert.deepEqual(names, ORDER.filter((k) => names.includes(k)), `${g.file}: tag order`);
        assert.deepEqual(names.filter((k) => !k.endsWith('RatingDiff')), ORDER.filter((k) => !k.endsWith('RatingDiff')), `${g.file}: every tag`);
        assert.equal(names.includes('WhiteRatingDiff'), !!g.ratingChanges, `${g.file}: rating diffs only for a rated result`);
        assert.equal(p.tag('Event'), `Scacelith Test Server ${g.rated ? 'rated' : 'casual'} ${g.category}`);
        assert.equal(p.tag('Site'), 'chess.example.org');
        assert.equal(p.tag('White'), g.whiteName);
        assert.equal(p.tag('Black'), g.blackName);
        assert.equal(p.tag('Date'), p.tag('UTCDate'));
        assert.match(p.tag('UTCTime'), /^\d\d:\d\d:\d\d$/);
        assert.equal(p.tag('PlyCount'), String(g.moves.length));
        assert.equal(p.tag('ScacelithGameId'), String(g.id));
        assert.equal(p.tag('TimeControl'), `${g.baseMs / 1000}+${g.incMs / 1000}`);
        assert.equal(p.tag('WhiteElo'), g.whiteRating === null ? '-' : String(g.whiteRating));
        assert.equal(p.result, p.tag('Result'));
        assert.equal(p.result, { 1: '1-0', 2: '0-1', 3: '1/2-1/2', 4: '*' }[g.status]);
        if (g.ratingChanges) {
            const d = g.ratingChanges.white.after - g.ratingChanges.white.before;
            assert.equal(p.tag('WhiteRatingDiff'), d < 0 ? String(d) : `+${d}`);
        }
        assert.deepEqual(p.plies.map((x) => x.move), [...g.moves], `${g.file}: SAN replays the stored moves`);
        p.plies.forEach((x, i) => {
            assert.equal(x.clk, Math.floor(g.clockMs[i] / 100) * 100, `${g.file} ply ${i} clk`);
            assert.equal(x.emt, Math.floor(g.spentMs[i] / 100) * 100, `${g.file} ply ${i} emt`);
        });
        seen.termination.add(p.tag('Termination'));
        if (p.plies.some((x) => /=/.test(x.san))) seen.flags |= 1;
        if (p.plies.some((x) => /^O-O/.test(x.san))) seen.flags |= 2;
        if (p.plies.some((x) => /^O-O-O/.test(x.san))) seen.flags |= 4;
        if (/deleted#/.test(p.tag('Black') + p.tag('White'))) seen.flags |= 8;
        if (p.comment) seen.flags |= 16;
    }
    assert.deepEqual([...seen.termination].sort(), ['abandoned', 'normal', 'rules infraction', 'time forfeit', 'unterminated']);
    assert.equal(seen.flags, 31, 'promotion, both castlings, a deleted player, end comments');
    const byFile = Object.fromEntries(games.map((g) => [g.file, readPgn(gamePgn(g, FIXTURE_CONFIG))]));
    assert.equal(byFile['04-flag-fall-draw.pgn'].tag('Termination'), 'time forfeit', 'a drawn flag fall is a time forfeit');
    assert.equal(byFile['04-flag-fall-draw.pgn'].result, '1/2-1/2');
    assert.equal(byFile['06-aborted.pgn'].tag('Termination'), 'unterminated');
    assert.equal(byFile['06-aborted.pgn'].comment, 'Aborted: first move not played in time');
    assert.ok(byFile['05-abandonment-en-passant.pgn'].plies.some((x) => x.san === 'exd6'), 'en passant');
    assert.ok(byFile['08-forfeit-underpromotion.pgn'].plies.some((x) => x.san === 'gxh1=N'), 'underpromotion');
});

test('the fixtures of the game\'s PGN reader are up to date (tests/data/server-pgn)', (t) => {
    if (!fs.existsSync(FIXTURES_DIR)) { t.skip('not in a full checkout of the repository'); return; }
    const files = renderFixtures();
    assert.ok(Object.keys(files).filter((n) => n.endsWith('.pgn')).length >= 6);
    for (const [name, text] of Object.entries(files)) {
        assert.equal(fs.readFileSync(path.join(FIXTURES_DIR, name), 'utf8'), text, `${name}: run node dedicated-server/tools/gen-pgn-fixtures.js`);
    }
    const index = JSON.parse(files['index.json']);
    assert.equal(index.games.length, fixtureGames().length);
    assert.deepEqual(index.games[0].clockMs.slice(0, 2), [180000, 180000]);
});

test('gamePgn refuses moves that do not replay, or replay to another ending', () => {
    const a = { id: 1, name: 'A' }, b = { id: 2, name: 'B' };
    const ok = record(a, b, 'e2e4 e7e5');
    assert.match(gamePgn(ok, FIXTURE_CONFIG), /\{Resignation\} 1-0\n$/);
    assert.equal(gamePgn({ ...ok, moves: Uint16Array.from([12 | (36 << 6)]) }, FIXTURE_CONFIG), null, 'e2e5 is not a move');
    const mate = record(a, b, 'f2f3 e7e5 g2g4 d8h4', { status: GameStatus.BlackWins, reason: EndReason.Checkmate });
    assert.match(gamePgn(mate, FIXTURE_CONFIG), /Qh4#\s\{\[%clk [^}]*\}\s\{Checkmate\}\s0-1\n$/);
    assert.equal(gamePgn({ ...mate, reason: EndReason.Resignation }, FIXTURE_CONFIG), null, 'mated, yet stored as a resignation');
    assert.equal(gamePgn({ ...mate, moves: Uint16Array.from([...mate.moves, 12 | (20 << 6)]) }, FIXTURE_CONFIG), null, 'a move after the mate');
    // Clock arrays shorter than the moves: the missing values are left out.
    const partial = gamePgn({ ...ok, clockMs: Uint32Array.from([180000]), spentMs: new Uint32Array(0) }, FIXTURE_CONFIG);
    assert.match(partial, /1\. e4 \{\[%clk 0:03:00\.0\]\} 1\.\.\. e5 \{Resignation\} 1-0/);
});

// ---- GET /api/v1/games/:id/pgn -------------------------------------------------------------------

test('GET /games/:id/pgn: the PGN file with its headers; 400, 404, 500 for an unreplayable record', async (t) => {
    const s = await start();
    t.after(s.close);
    const alice = s.user('Alice'), bob = s.user('Bob');
    const g = record(alice, bob, 'e2e4 e7e5 g1f3 b8c6 f1b5 a7a6 b5c6 d7c6 e1g1');
    const bad = record(alice, bob, 'e2e4', { moves: Uint16Array.from([12 | (28 << 6), 12 | (28 << 6)]) });
    s.store.games.finishBatch([g, bad]);
    let r = await s.req('GET', `/api/v1/games/${g.id}/pgn`);
    assert.equal(r.status, 200);
    assert.equal(r.headers['content-type'], PGN_CONTENT_TYPE);
    assert.equal(r.headers['content-disposition'], `attachment; filename="scacelith-${g.id}.pgn"`);
    assert.equal(r.headers['content-security-policy'], "default-src 'none'; frame-ancestors 'none'");
    assert.equal(r.headers['x-content-type-options'], 'nosniff');
    assert.equal(r.headers['cache-control'], 'no-store');
    assert.equal(r.text, gamePgn(s.store.games.byId(g.id), s.config));
    const p = readPgn(r.text);
    assert.equal(p.tag('Event'), 'Test Server rated 3+2');
    assert.equal(p.tag('Site'), 'chess.example.org');
    assert.equal(p.tag('WhiteRatingDiff'), '+10');
    assert.equal(p.tag('BlackRatingDiff'), '-10');
    assert.equal(p.tag('Termination'), 'normal');
    assert.equal(p.plies[8].san, 'O-O');
    assert.equal(p.plies[2].clk, 180400, 'truncated to tenths (180470 ms)');
    assert.equal(+r.headers['content-length'], Buffer.byteLength(r.text));
    // The same answer with a session; HEAD without a body.
    assert.equal((await s.req('GET', `/api/v1/games/${g.id}/pgn`, { token: alice.token })).text, r.text);
    r = await s.req('HEAD', `/api/v1/games/${g.id}/pgn`);
    assert.equal(r.status, 200);
    assert.equal(r.text, '');
    // Errors.
    r = await s.req('GET', '/api/v1/games/123/pgn');
    assert.equal(r.status, 404);
    assert.equal(r.json.error, 'not_found');
    assert.equal((await s.req('GET', '/api/v1/games/abc/pgn')).json.error, 'invalid_game_id');
    assert.equal((await s.req('GET', '/api/v1/games/0/pgn')).status, 400);
    assert.equal((await s.req('GET', `/api/v1/games/${g.id}/pgn`, { token: 'sct_' + 'z'.repeat(43) })).status, 401, 'a token sent must be valid');
    r = await s.req('GET', `/api/v1/games/${bad.id}/pgn`);
    assert.equal(r.status, 500);
    assert.equal(r.json.error, 'internal_error');
    assert.ok(s.log.lines.some(([level, msg, f]) => level === 'error' && /cannot be replayed/.test(msg) && f.gameId === bad.id), 'logged');
});

test('GET /games/:id/pgn: aborted games, deleted players, the public_read limit shared with /games/:id', async (t) => {
    const s = await start();
    t.after(s.close);
    const carl = s.user('Carl'), dina = s.user('Dina');
    const aborted = record(carl, dina, '', { rated: false, status: GameStatus.Aborted, reason: EndReason.NoShow, category: 'custom', baseMs: 90000,
        incMs: 5000, whiteRating: null, blackRating: null });
    const won = record(dina, carl, 'd2d4 d7d5', { status: GameStatus.BlackWins, reason: EndReason.Timeout });
    s.store.games.finishBatch([aborted, won]);
    let p = readPgn((await s.req('GET', `/api/v1/games/${aborted.id}/pgn`)).text);
    assert.equal(p.tag('Result'), '*');
    assert.equal(p.result, '*');
    assert.equal(p.tag('Termination'), 'unterminated');
    assert.equal(p.tag('Event'), 'Test Server casual custom');
    assert.equal(p.tag('TimeControl'), '90+5');
    assert.equal(p.tag('WhiteElo'), '-');
    assert.equal(p.tag('WhiteRatingDiff'), undefined);
    assert.equal(p.plies.length, 0);
    assert.equal(p.comment, 'Aborted: first move not played in time');
    s.store.users.anonymize(carl.id);
    p = readPgn((await s.req('GET', `/api/v1/games/${won.id}/pgn`)).text);
    assert.equal(p.tag('Black'), `deleted#${carl.id}`);
    assert.equal(p.tag('Termination'), 'time forfeit');
    assert.equal(p.result, '0-1');
    assert.equal(p.comment, 'Loss on time');
    // public_read: 60 per minute per client, the record and the PGN together (2 requests above).
    let last;
    for (let i = 0; i < 58; i++) last = await s.req('GET', `/api/v1/games/${won.id}${i % 2 ? '/pgn' : ''}`);
    assert.equal(last.status, 200);
    last = await s.req('GET', `/api/v1/games/${won.id}/pgn`);
    assert.equal(last.status, 429);
    assert.equal(last.json.error, 'rate_limited');
});

// ---- GET /api/v1/games/:id with a session ----------------------------------------------------------

test('GET /games/:id: you and reportable for the game\'s players only; the public answer unchanged', async (t) => {
    const s = await start();
    t.after(s.close);
    const alice = s.user('Alice'), bob = s.user('Bob'), eve = s.user('Eve');
    const g = record(alice, bob, 'e2e4 e7e5');
    const old = record(bob, alice, 'd2d4', { endedAt: NOW - 8 * DAY, startedAt: NOW - 8 * DAY - HOUR });
    s.store.games.finishBatch([g, old]);
    const pub = (await s.req('GET', `/api/v1/games/${g.id}`)).json;
    assert.equal(pub.you, undefined);
    assert.equal(pub.reportable, undefined);
    assert.equal(pub.color, undefined);
    const asEve = (await s.req('GET', `/api/v1/games/${g.id}`, { token: eve.token })).json;
    assert.deepEqual(asEve, pub, 'a session of someone else: the public answer');
    const asAlice = (await s.req('GET', `/api/v1/games/${g.id}`, { token: alice.token })).json;
    assert.equal(asAlice.you, 'white');
    assert.equal(asAlice.reportable, true);
    const { you, reportable, ...rest } = asAlice;
    assert.deepEqual(rest, pub, 'otherwise the same answer');
    const asBob = (await s.req('GET', `/api/v1/games/${g.id}`, { token: bob.token })).json;
    assert.equal(asBob.you, 'black');
    assert.equal(asBob.reportable, true);
    assert.equal((await s.req('GET', `/api/v1/games/${old.id}`, { token: alice.token })).json.reportable, false, 'ended more than 7 days ago');
    assert.equal((await s.req('GET', `/api/v1/games/${old.id}`, { token: alice.token })).json.you, 'black');
    // After a report of Bob for this game, Alice cannot report again; Bob still can.
    const filed = await s.req('POST', '/api/v1/reports', { token: alice.token, body: { gameId: String(g.id), reported: 'Bob', category: 'cheating' } });
    assert.equal(filed.status, 202);
    assert.equal((await s.req('GET', `/api/v1/games/${g.id}`, { token: alice.token })).json.reportable, false);
    assert.equal((await s.req('GET', `/api/v1/games/${g.id}`, { token: bob.token })).json.reportable, true);
    // The game becomes too old for a report.
    s.setNow(NOW + 7 * DAY);
    assert.equal((await s.req('GET', `/api/v1/games/${g.id}`, { token: bob.token })).json.reportable, false);
    assert.equal((await s.req('GET', `/api/v1/games/${g.id}`, { token: 'sct_' + 'q'.repeat(43) })).status, 401, 'an invalid token is refused');
    // The report weighed the reporter as a year-old account (createdAt from the account, not the session).
    const rep = s.store.reports.forReporter(alice.id)[0];
    assert.equal(rep.reportedName, 'Bob');
    assert.ok(rep.weight > 0.1, `weight ${rep.weight}`);
});

// ---- GET /api/v1/account/games ------------------------------------------------------------------

test('GET /account/games: the player\'s own history with filters, cursor, total and outcome', async (t) => {
    const s = await start();
    t.after(s.close);
    const me = s.user('Mira'), op = s.user('Otto'), other = s.user('Pia');
    const W = GameStatus.WhiteWins, L = GameStatus.BlackWins, D = GameStatus.Draw, X = GameStatus.Aborted;
    const list = [];
    for (let i = 0; i < 30; i++) {
        const mineWhite = i % 2 === 0;
        const status = [W, L, D, W, X][i % 5];
        const extra = { status, reason: status === X ? EndReason.Aborted : EndReason.Resignation, rated: i % 3 !== 0 && status !== X };
        if (i % 7 === 0) Object.assign(extra, { category: 'custom', baseMs: 60000, incMs: 1000, rated: false });
        list.push(mineWhite ? record(me, op, 'e2e4', extra) : record(op, me, 'e2e4', extra));
    }
    list.push(record(op, other, 'e2e4'));
    s.store.games.finishBatch(list);
    const mine = list.slice(0, 30).reverse();
    const outcome = (g) => outcomeFor(g, me.id);

    assert.equal((await s.req('GET', '/api/v1/account/games')).status, 401);
    let r = await s.req('GET', '/api/v1/account/games', { token: me.token });
    assert.equal(r.status, 200);
    assert.equal(r.json.games.length, 20);
    assert.equal(r.json.total, 30);
    assert.deepEqual(r.json.games.map((x) => x.id), mine.slice(0, 20).map((x) => x.id));
    assert.equal(r.json.next, mine[19].id);
    const first = r.json.games[0];
    assert.deepEqual(Object.keys(first).sort(), ['baseMs', 'black', 'category', 'color', 'endedAt', 'id', 'incMs', 'outcome', 'plies', 'rated',
        'reason', 'result', 'startedAt', 'status', 'termination', 'timeControl', 'white'].sort());
    assert.deepEqual(first, JSON.parse(JSON.stringify(historySummary(s.store.games.byId(mine[0].id), me.id))));
    r = await s.req('GET', `/api/v1/account/games?before=${r.json.next}`, { token: me.token });
    assert.equal(r.json.games.length, 10);
    assert.equal(r.json.next, null, 'the last page');
    assert.equal(r.json.total, 30, 'total counts every page');
    // Exactly one page: no next.
    r = await s.req('GET', '/api/v1/account/games?limit=30', { token: me.token });
    assert.equal(r.json.games.length, 30);
    assert.equal(r.json.next, null);
    // Filters.
    const check = async (query, pred) => {
        const res = await s.req('GET', `/api/v1/account/games?limit=50&${query}`, { token: me.token });
        assert.equal(res.status, 200, query);
        const want = mine.filter(pred);
        assert.deepEqual(res.json.games.map((x) => x.id), want.map((x) => x.id), query);
        assert.equal(res.json.total, want.length, `${query}: total`);
        return res.json;
    };
    const wins = await check('result=win', (g) => outcome(g) === 'win');
    assert.ok(wins.games.length > 0 && wins.games.every((x) => x.outcome === 'win'));
    assert.ok(wins.games.some((x) => x.color === 'black'), 'wins as Black count');
    await check('result=loss', (g) => outcome(g) === 'loss');
    await check('result=draw', (g) => outcome(g) === 'draw');
    await check('rated=true', (g) => g.rated);
    await check('rated=false', (g) => !g.rated);
    await check('category=custom', (g) => g.category === 'custom');
    await check('category=3%2B2&rated=true&result=win', (g) => g.category === '3+2' && g.rated && outcome(g) === 'win');
    await check('category=3+2', (g) => g.category === '3+2');
    await check('category=&result=&rated=', () => true);
    const all = await check('', () => true);
    assert.ok(all.games.some((x) => x.outcome === 'aborted'), 'aborted games without a result filter');
    assert.deepEqual(new Set(all.games.map((x) => x.outcome)), new Set(['win', 'loss', 'draw', 'aborted']));
    const custom = all.games.find((x) => x.category === 'custom');
    assert.equal(custom.baseMs, 60000);
    assert.equal(custom.incMs, 1000);
    assert.equal(custom.timeControl, '60+1');
    // Paging through a filter.
    r = await s.req('GET', '/api/v1/account/games?result=win&limit=3', { token: me.token });
    const page2 = await s.req('GET', `/api/v1/account/games?result=win&limit=3&before=${r.json.next}`, { token: me.token });
    assert.deepEqual([...r.json.games, ...page2.json.games].map((x) => x.id), wins.games.slice(0, 6).map((x) => x.id));
    // Someone else's history is their own.
    r = await s.req('GET', '/api/v1/account/games', { token: other.token });
    assert.equal(r.json.total, 1);
    assert.equal(r.json.games[0].outcome, 'loss');
    assert.equal(r.json.games[0].color, 'black');
});

test('GET /account/games: query errors, the limit cap, the per-player rate limit', async (t) => {
    const s = await start();
    t.after(s.close);
    const me = s.user('Quin'), op = s.user('Rhea');
    const batch = [];
    for (let i = 0; i < 60; i++) batch.push(record(me, op, 'e2e4', { rated: false }));
    s.store.games.finishBatch(batch);
    const get = (q) => s.req('GET', `/api/v1/account/games${q}`, { token: me.token });
    for (const [q, error, field] of [
        ['?before=abc', 'invalid_cursor', 'before'], ['?before=-1', 'invalid_cursor', 'before'], ['?before=0', 'invalid_cursor', 'before'],
        ['?before=99999999999999999999', 'invalid_cursor', 'before'], ['?limit=0', 'invalid_limit', 'limit'], ['?limit=x', 'invalid_limit', 'limit'],
        ['?limit=1000', 'invalid_limit', 'limit'], ['?category=4%2B2', 'invalid_filter', 'category'], ['?category=CUSTOM', 'invalid_filter', 'category'],
        ['?rated=yes', 'invalid_filter', 'rated'], ['?rated=1', 'invalid_filter', 'rated'], ['?result=aborted', 'invalid_filter', 'result'],
        ['?result=WIN', 'invalid_filter', 'result'],
    ]) {
        const r = await get(q);
        assert.equal(r.status, 400, q);
        assert.equal(r.json.error, error, q);
        assert.equal(r.json.field, field, q);
        assert.equal(typeof r.json.message, 'string');
    }
    const r = await get('?limit=500');
    assert.equal(r.json.games.length, 50, 'capped at 50');
    assert.equal(r.json.next, batch[10].id);
    assert.equal((await get('?unknown=1')).status, 200, 'unknown parameters are ignored');
    // 60 per minute per player (the requests above: 15, refused ones included).
    let last;
    for (let i = 0; i < 45; i++) last = await get('?limit=1');
    assert.equal(last.status, 200);
    last = await get('?limit=1');
    assert.equal(last.status, 429);
    assert.ok(last.json.retryAfter >= 1);
    // Per player: someone else is not limited.
    assert.equal((await s.req('GET', '/api/v1/account/games', { token: op.token })).status, 200);
});

test('parseHistoryQuery and outcomeFor', () => {
    const cats = new Set(['3+2', 'custom']);
    assert.deepEqual(parseHistoryQuery({}, cats), { before: null, limit: 20, filter: {} });
    assert.deepEqual(parseHistoryQuery(new URLSearchParams('before=12&limit=5&category=3 2&rated=false&result=draw'), cats),
        { before: 12, limit: 5, filter: { category: '3+2', rated: false, result: 'draw' } });
    assert.equal(parseHistoryQuery({ result: 'x' }, cats).error.body.error, 'invalid_filter');
    const g = { status: GameStatus.WhiteWins, whiteId: 1 };
    assert.equal(outcomeFor(g, 1), 'win');
    assert.equal(outcomeFor(g, 2), 'loss');
    assert.equal(outcomeFor({ status: GameStatus.BlackWins, whiteId: 1 }, 2), 'win');
    assert.equal(outcomeFor({ status: GameStatus.Draw, whiteId: 1 }, 2), 'draw');
    assert.equal(outcomeFor({ status: GameStatus.Aborted, whiteId: 1 }, 1), 'aborted');
});

test('the default route list serves /account/games', () => {
    assert.ok(DEFAULT_ROUTES.includes(accountGameRoutes));
    const handler = createApiHandler({ config: testConfig(), store: {}, auth: { validateToken: () => null }, log: captureLog() });
    const found = handler.router.match('GET', '/api/v1/account/games');
    assert.equal(found.route.opts.auth, 'required');
    assert.deepEqual(found.route.opts.rate, { key: 'account_games', limit: 60, windowMs: 60000, by: 'user' });
    assert.equal(handler.router.match('GET', '/api/v1/games/1/pgn').route.opts.auth, 'optional');
});
