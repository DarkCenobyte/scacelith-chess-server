// Store calls of the account API (src/store/index.js header, "Account API"): the filtered game
// history and its count (with their query plans), the e-mail change, the user's tokens, every
// session, security events, sanctions, conduct events, reports filed and refunds received.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { openStore, migrate, explainQueryPlan, StoreError, GAMES_FOR_USER_SQL, GAMES_COUNT_FOR_USER_SQL } from '../../src/store/index.js';
import { testConfig } from '../../src/config.js';
import { enums } from '../../src/protocol/schema.js';

const { GameStatus, EndReason } = enums;
const DAY = 86400000;
const sha = (s) => crypto.createHash('sha256').update(s).digest();

function applyGame(white, black, score) {
    const d = Math.round(20 * (score - 0.5));
    const side = (r, delta, s) => ({
        before: r.rating, after: r.rating + delta,
        record: { rating: r.rating + delta, games: r.games + 1, wins: r.wins + (s === 1 ? 1 : 0), draws: r.draws + (s === 0.5 ? 1 : 0),
            losses: r.losses + (s === 0 ? 1 : 0), peak: Math.max(r.peak, r.rating + delta), reachedSenior: false },
    });
    return { white: side(white, d, score), black: side(black, -d, 1 - score) };
}

function memStore() {
    const store = openStore(testConfig({ DB_PATH: ':memory:' }), { applyGame });
    migrate(store);
    return store;
}

let nextId = 3_000_000_000_000;
function record(whiteId, blackId, extra = {}) {
    return {
        id: ++nextId, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId, blackId, whiteName: `p${whiteId}`,
        blackName: `p${blackId}`, whiteRating: 1500, blackRating: 1500, startedAt: 1000, endedAt: 2000, status: GameStatus.WhiteWins,
        reason: EndReason.Resignation, moves: Uint16Array.from([12 | (28 << 6)]), spentMs: Uint32Array.from([0]),
        clockMs: Uint32Array.from([180000]), rematchOf: 0, flags: 0, ...extra,
    };
}

// The history fixture: player `me` with games of every kind, against two opponents.
function history() {
    const store = memStore();
    const me = store.users.create({ username: 'Me', email: 'me@e.org' });
    const a = store.users.create({ username: 'Ann', email: 'a@e.org' });
    const b = store.users.create({ username: 'Ben', email: 'b@e.org' });
    const W = GameStatus.WhiteWins, L = GameStatus.BlackWins, D = GameStatus.Draw, X = GameStatus.Aborted;
    const list = [
        record(me, a, { status: W }),                                           // win, rated 3+2
        record(a, me, { status: W }),                                           // loss
        record(me, b, { status: D, reason: EndReason.Agreement }),              // draw
        record(b, me, { status: L }),                                           // win as Black
        record(me, a, { status: X, reason: EndReason.Aborted, rated: false }),  // aborted
        record(me, b, { status: L, rated: false }),                             // casual loss
        record(a, me, { status: D, category: '5+0', baseMs: 300000, incMs: 0 }), // draw 5+0
        record(me, a, { status: W, category: 'custom', rated: false, baseMs: 60000, incMs: 1000 }), // custom win
        record(a, b, { status: W }),                                            // not mine
    ];
    store.games.finishBatch(list);
    return { store, me, a, b, list };
}

const ids = (rows) => rows.map((g) => g.id);

test('games.listForUser: filters by category, rated and result from the player\'s side, newest first', () => {
    const { store, me, list } = history();
    const mine = list.slice(0, 8).reverse();
    assert.deepEqual(ids(store.games.listForUser(me)), ids(mine), 'no filter: every game, both colours');
    assert.deepEqual(ids(store.games.listForUser(me, { limit: 3 })), ids(mine.slice(0, 3)));
    assert.equal(store.games.listForUser(me)[0].moves, undefined, 'summaries without move arrays');
    assert.deepEqual(ids(store.games.listForUser(me, { result: 'win' })), ids([list[7], list[3], list[0]]));
    assert.deepEqual(ids(store.games.listForUser(me, { result: 'loss' })), ids([list[5], list[1]]));
    assert.deepEqual(ids(store.games.listForUser(me, { result: 'draw' })), ids([list[6], list[2]]));
    assert.deepEqual(ids(store.games.listForUser(me, { rated: true })), ids([list[6], list[3], list[2], list[1], list[0]]));
    assert.deepEqual(ids(store.games.listForUser(me, { rated: false })), ids([list[7], list[5], list[4]]));
    assert.deepEqual(ids(store.games.listForUser(me, { category: 'custom' })), ids([list[7]]));
    assert.deepEqual(ids(store.games.listForUser(me, { category: '5+0', result: 'draw', rated: true })), ids([list[6]]));
    assert.deepEqual(ids(store.games.listForUser(me, { category: '3+2', rated: true, result: 'win' })), ids([list[3], list[0]]));
    assert.deepEqual(store.games.listForUser(me, { category: '1+0' }), []);
    // Aborted games only come without a result filter.
    for (const result of ['win', 'loss', 'draw']) {
        assert.ok(!store.games.listForUser(me, { result }).some((g) => g.status === GameStatus.Aborted), result);
    }
    // undefined and null mean "any".
    assert.equal(store.games.listForUser(me, { category: undefined, rated: null, result: undefined }).length, 8);
    assert.throws(() => store.games.listForUser(me, { result: 'aborted' }), (e) => e instanceof StoreError && e.code === 'invalid');
    assert.throws(() => store.games.countForUser(me, { result: 'won' }), (e) => e.code === 'invalid');
    store.close();
});

test('games.listForUser pages with the before cursor; countForUser counts the filter over every page', () => {
    const store = memStore();
    const me = store.users.create({ username: 'Pager', email: 'p@e.org' });
    const op = store.users.create({ username: 'Other', email: 'o@e.org' });
    const list = [];
    for (let i = 0; i < 57; i++) {
        list.push(i % 2 ? record(me, op, { rated: i % 3 === 0, status: i % 5 === 0 ? GameStatus.Draw : GameStatus.WhiteWins })
            : record(op, me, { rated: i % 3 === 0, status: GameStatus.WhiteWins }));
    }
    store.games.finishBatch(list);
    const filter = { rated: false };
    const expected = list.filter((g) => !g.rated).reverse();
    const seen = [];
    let before = null;
    for (;;) {
        const page = store.games.listForUser(me, { before, limit: 10, ...filter });
        seen.push(...page);
        if (page.length < 10) break;
        before = page[page.length - 1].id;
    }
    assert.deepEqual(ids(seen), ids(expected));
    assert.equal(store.games.countForUser(me, filter), expected.length);
    assert.equal(store.games.countForUser(me), 57, 'without a filter: every game, as before');
    assert.equal(store.games.countForUser(me, {}), 57, 'an empty filter too');
    assert.equal(store.games.countForUser(me, { result: 'win' }), list.filter((g) => (g.whiteId === me) === (g.status === GameStatus.WhiteWins)
        && g.status !== GameStatus.Draw).length);
    assert.equal(store.games.countForUser(me, { result: 'draw', rated: false }), list.filter((g) => g.status === GameStatus.Draw && !g.rated).length);
    assert.equal(store.games.countForUser(op, { result: 'loss' }), store.games.countForUser(me, { result: 'win' }));
    assert.equal(store.games.listForUser(me, { before: list[0].id }).length, 0, 'nothing before the first game');
    assert.equal(store.games.listForUser(me, { limit: 0 }).length, 0);
    assert.equal(store.games.listForUser(me, { limit: 500 }).length, 57, 'no cap in the store');
    store.close();
});

test('the history queries read the per-colour indexes, never the whole games table', () => {
    const store = memStore();
    const listPlan = explainQueryPlan(store, GAMES_FOR_USER_SQL, [1, 1e15, '3+2', 1, 1, 2, 20]).map((r) => r.detail);
    const countPlan = explainQueryPlan(store, GAMES_COUNT_FOR_USER_SQL, [1, null, null, 0, 3, 3]).map((r) => r.detail);
    for (const plan of [listPlan, countPlan]) {
        assert.ok(plan.some((d) => /SEARCH games USING (COVERING )?INDEX games_white \(white_id=\?/.test(d)), plan.join('\n'));
        assert.ok(plan.some((d) => /SEARCH games USING (COVERING )?INDEX games_black \(black_id=\?/.test(d)), plan.join('\n'));
        assert.ok(!plan.some((d) => /^SCAN games/.test(d)), `no table scan:\n${plan.join('\n')}`);
    }
    assert.ok(listPlan.some((d) => /games_white \(white_id=\? AND id<\?\)/.test(d)), 'the cursor bounds the range');
    assert.throws(() => explainQueryPlan({}, 'SELECT 1'), TypeError);
    store.close();
});

test('users.update({ email }): normalized lookups, email_taken for an address of another account, nothing changed then', () => {
    const store = memStore();
    const id = store.users.create({ username: 'Mover', email: 'old@example.org', emailVerified: true });
    const other = store.users.create({ username: 'Owner', email: 'Taken@Example.org' });
    assert.equal(store.users.update(id, { email: ' New.Addr@Example.ORG ', emailVerified: true }), true);
    let u = store.users.byId(id);
    assert.equal(u.email, 'New.Addr@Example.ORG');
    assert.equal(store.users.byEmail('new.addr@example.org').id, id);
    assert.equal(store.users.byEmail('old@example.org'), null, 'the old address is free again');
    assert.equal(store.users.update(id, { email: 'NEW.ADDR@example.org' }), true, 'own address, another case');
    assert.throws(() => store.users.update(id, { email: 'taken@EXAMPLE.org', emailVerified: false }),
        (e) => e instanceof StoreError && e.code === 'email_taken');
    u = store.users.byId(id);
    assert.equal(u.email, 'NEW.ADDR@example.org', 'unchanged after the refusal');
    assert.equal(u.emailVerified, true);
    assert.equal(store.users.byEmail('taken@example.org').id, other);
    store.users.create({ username: 'Later', email: 'old@example.org' });
    store.close();
});

test('tokens.deleteForUser and tokens.liveForUser', () => {
    const store = memStore();
    const id = store.users.create({ username: 'Tok', email: 't@e.org' });
    const other = store.users.create({ username: 'Oth', email: 'o@e.org' });
    const now = 1_900_000_000_000;
    store.tokens.create({ kind: 'email_change', tokenHash: sha('old'), userId: id, data: { email: 'a@x.org', from: 't@e.org' }, createdAt: now - 3000, expiresAt: now + DAY });
    store.tokens.create({ kind: 'email_change', tokenHash: sha('new'), userId: id, data: { email: 'b@x.org', from: 't@e.org' }, createdAt: now - 1000, expiresAt: now + DAY });
    store.tokens.create({ kind: 'email_change', tokenHash: sha('expired'), userId: id, data: { email: 'c@x.org' }, createdAt: now, expiresAt: now - 1 });
    store.tokens.create({ kind: 'reset', tokenHash: sha('reset'), userId: id, createdAt: now, expiresAt: now + DAY });
    store.tokens.create({ kind: 'email_change', tokenHash: sha('theirs'), userId: other, data: { email: 'd@x.org' }, createdAt: now, expiresAt: now + DAY });

    let live = store.tokens.liveForUser(id, 'email_change', now);
    assert.equal(live.data.email, 'b@x.org', 'the newest live token');
    assert.equal(live.userId, id);
    assert.equal(live.kind, 'email_change');
    assert.equal(live.tokenHash, undefined, 'no hash in the answer');
    store.tokens.consume('email_change', sha('new'), now);
    assert.equal(store.tokens.liveForUser(id, 'email_change', now).data.email, 'a@x.org', 'a consumed token is not live');
    assert.equal(store.tokens.liveForUser(id, 'email_change', now + 2 * DAY), null, 'expired');
    assert.equal(store.tokens.liveForUser(id, 'verify_email', now), null, 'another kind');

    assert.equal(store.tokens.deleteForUser(id, 'email_change'), 3, 'consumed and expired ones too');
    assert.equal(store.tokens.liveForUser(id, 'email_change', now), null);
    assert.equal(store.tokens.get('email_change', sha('old')), null);
    assert.ok(store.tokens.get('reset', sha('reset')), 'other kinds stay');
    assert.equal(store.tokens.liveForUser(other, 'email_change', now).data.email, 'd@x.org', 'other users\' tokens stay');
    assert.equal(store.tokens.deleteForUser(id, 'email_change'), 0);
    for (const sql of ['DELETE FROM tokens WHERE user_id = ? AND kind = ?']) {
        assert.ok(explainQueryPlan(store, sql, [id, 'x']).some((r) => /tokens_user/.test(r.detail)));
    }
    store.close();
});

test('sessions.allForUser: revoked and expired sessions included, with their IP, newest first', () => {
    const store = memStore();
    const id = store.users.create({ username: 'Sess', email: 's@e.org' });
    const other = store.users.create({ username: 'Else', email: 'x@e.org' });
    const t = 1_900_000_000_000;
    const s1 = store.sessions.create({ userId: id, tokenHash: sha('1'), createdAt: t, expiresAt: t + DAY, clientLabel: 'Scacelith 1.0 (Linux)', ip: '203.0.113.5' });
    const s2 = store.sessions.create({ userId: id, tokenHash: sha('2'), createdAt: t + 10, expiresAt: t + 20, ip: '2001:db8::1' });
    const s3 = store.sessions.create({ userId: id, tokenHash: sha('3'), createdAt: t + 20, expiresAt: t + DAY });
    store.sessions.create({ userId: other, tokenHash: sha('4'), createdAt: t, expiresAt: t + DAY });
    store.sessions.revoke(s3, id, t + 30);
    const all = store.sessions.allForUser(id);
    assert.deepEqual(all.map((s) => s.id), [s3, s2, s1]);
    assert.deepEqual(all[0], { id: s3, createdAt: t + 20, lastSeenAt: t + 20, expiresAt: t + DAY, idleExpiresAt: t + DAY, revokedAt: t + 30,
        clientLabel: null, ip: null });
    assert.equal(all[1].ip, '2001:db8::1');
    assert.equal(all[2].clientLabel, 'Scacelith 1.0 (Linux)');
    assert.equal(all[2].revokedAt, null);
    assert.ok(!('tokenHash' in all[0]));
    assert.deepEqual(store.sessions.listForUser(id).map((s) => s.id).sort(), [s1, s2].sort(), 'listForUser still leaves revoked ones out');
    assert.deepEqual(store.sessions.allForUser(424242), []);
    store.close();
});

test('security events, sanctions (lifted ones too), conduct events of one user', () => {
    const store = memStore();
    const id = store.users.create({ username: 'Hist', email: 'h@e.org' });
    const other = store.users.create({ username: 'Oth', email: 'o@e.org' });
    const t = 1_900_000_000_000;
    store.security.insertBatch([
        { kind: 'login', userId: id, ip: '203.0.113.1', at: t, detail: { method: 'password' } },
        { kind: 'password_changed', userId: id, ip: null, at: t + 10 },
        { kind: 'login', userId: other, ip: '203.0.113.2', at: t + 5 },
    ]);
    const ev = store.security.forUser(id);
    assert.deepEqual(ev.map((e) => e.kind), ['password_changed', 'login'], 'newest first');
    assert.deepEqual(ev[1].detail, { method: 'password' });
    assert.equal(ev[1].ip, '203.0.113.1');
    assert.equal(store.security.forUser(id, 1).length, 1, 'capped');

    const s1 = store.sanctions.create({ userId: id, kind: 'warning', reason: 'abuse', createdBy: 'mod-a', createdAt: t, startsAt: t });
    const s2 = store.sanctions.create({ userId: id, kind: 'ban', reason: 'x', createdBy: 'mod-b', createdAt: t + 1, startsAt: t + 1, endsAt: t + DAY });
    store.sanctions.lift(s2, 'mod-c', t + 2);
    const list = store.sanctions.list(id);
    assert.deepEqual(list.map((s) => s.id), [s2, s1]);
    assert.equal(list[0].liftedAt, t + 2);
    assert.equal(list[0].liftedBy, 'mod-c');
    assert.equal(store.sanctions.active(id, t + 3).length, 1, 'the lifted ban is not active');

    store.conduct.record(id, 'abandon', t);
    store.conduct.record(id, 'noshow', t + 5);
    store.conduct.record(other, 'abort', t + 1);
    assert.deepEqual(store.conduct.forUser(id), [{ kind: 'noshow', at: t + 5 }, { kind: 'abandon', at: t }]);
    assert.deepEqual(store.conduct.forUser(id, 1), [{ kind: 'noshow', at: t + 5 }]);
    assert.deepEqual(store.conduct.forUser(other), [{ kind: 'abort', at: t + 1 }]);
    store.close();
});

test('reports.forReporter: the reports a player filed, with the reported name and the outcome', () => {
    const store = memStore();
    const me = store.users.create({ username: 'Rep', email: 'r@e.org' });
    const x = store.users.create({ username: 'Xavier', email: 'x@e.org' });
    const y = store.users.create({ username: 'Yolanda', email: 'y@e.org' });
    const t = 1_900_000_000_000;
    const r1 = store.reports.create({ reporterId: me, reportedId: x, gameId: 11, category: 'cheating', comment: 'engine', weight: 0.8, at: t });
    const r2 = store.reports.create({ reporterId: me, reportedId: y, gameId: 12, category: 'abuse', at: t + 1 });
    store.reports.create({ reporterId: x, reportedId: me, gameId: 11, category: 'other', at: t + 2 });
    store.reports.resolve(r1, 'dismissed', 'mod', t + 3);
    const filed = store.reports.forReporter(me);
    assert.deepEqual(filed.map((r) => [r.id, r.reportedName, r.category, r.status, r.outcome]),
        [[r2, 'Yolanda', 'abuse', 'open', null], [r1, 'Xavier', 'cheating', 'dismissed', 'dismissed']]);
    assert.equal(filed[1].comment, 'engine');
    assert.equal(filed[1].gameId, 11);
    assert.equal(filed[1].createdAt, t);
    assert.equal(store.reports.forReporter(me, 1).length, 1);
    assert.deepEqual(store.reports.forReporter(y), []);
    // The name follows the account (anonymized after a deletion).
    store.users.anonymize(y);
    assert.equal(store.reports.forReporter(me)[0].reportedName, `deleted#${y}`);
    store.close();
});

test('refunds.list({ victimId }): the refunds a player received', () => {
    const store = memStore();
    const cheat = store.users.create({ username: 'Cheat', email: 'c@e.org' });
    const victim = store.users.create({ username: 'Vic', email: 'v@e.org' });
    const g = record(cheat, victim, { status: GameStatus.WhiteWins, endedAt: 5000 });
    store.games.finishBatch([g, record(victim, cheat, { status: GameStatus.WhiteWins })]);
    const given = store.refunds.applyForCheater({ cheaterId: cheat, since: 0, now: 6000 });
    assert.equal(given.length, 1);
    const got = store.refunds.list({ victimId: victim });
    assert.equal(got.length, 1);
    assert.equal(got[0].gameId, g.id);
    assert.equal(got[0].points, 10);
    assert.equal(got[0].victimName, 'Vic');
    assert.deepEqual(store.refunds.list({ victimId: cheat }), []);
    store.close();
});
