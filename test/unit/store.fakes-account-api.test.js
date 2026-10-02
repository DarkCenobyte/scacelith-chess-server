// The in-memory test stores (test/unit/helpers/auth-fakes.js, src/anticheat/testing/fake-store.js)
// answer the account API's store calls like the real store (src/store/index.js) for the same data.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openStore, migrate } from '../../src/store/index.js';
import { testConfig } from '../../src/config.js';
import { createFakeStore as createAuthFake } from './helpers/auth-fakes.js';
import { createFakeStore as createAnticheatFake } from '../../src/anticheat/testing/fake-store.js';
import { enums } from '../../src/protocol/schema.js';

const { GameStatus, EndReason } = enums;
const T = 1_900_000_000_000;

function realStore() {
    const store = openStore(testConfig({ DB_PATH: ':memory:' }));
    migrate(store);
    return store;
}

// Unrated games (the real store then needs no rating function) between players 1 and 2.
function games(me, op) {
    const out = [];
    const statuses = [GameStatus.WhiteWins, GameStatus.BlackWins, GameStatus.Draw, GameStatus.Aborted];
    for (let i = 0; i < 12; i++) {
        const white = i % 2 ? me : op, black = i % 2 ? op : me;
        out.push({
            id: 9_000_000 + i, category: i % 3 ? '3+2' : 'custom', rated: false, baseMs: 180000, incMs: 2000, whiteId: white, blackId: black,
            whiteName: `u${white}`, blackName: `u${black}`, whiteRating: 1500, blackRating: 1500, startedAt: T + i, endedAt: T + i + 1,
            status: statuses[i % 4], reason: EndReason.Resignation, moves: new Uint16Array(0), spentMs: new Uint32Array(0),
            clockMs: new Uint32Array(0), rematchOf: 0, flags: 0, ratingChanges: null,
        });
    }
    return out;
}

const FILTERS = [{}, { result: 'win' }, { result: 'loss' }, { result: 'draw' }, { category: 'custom' }, { category: '3+2', result: 'win' },
    { rated: false }, { rated: true }];

test('games.listForUser / countForUser: the fakes agree with the real store', () => {
    const real = realStore();
    const me = real.users.create({ username: 'Me', email: 'me@e.org' });
    const op = real.users.create({ username: 'Op', email: 'op@e.org' });
    const list = games(me, op);
    real.games.finishBatch(list);
    const auth = createAuthFake();
    auth._raw.games.push(...list);
    const ac = createAnticheatFake();
    for (const g of list) ac._.addGame(g);
    for (const f of FILTERS) {
        const want = real.games.listForUser(me, { limit: 50, ...f }).map((g) => g.id);
        assert.deepEqual(auth.games.listForUser(me, { limit: 50, ...f }).map((g) => g.id), want, JSON.stringify(f));
        assert.deepEqual(ac.games.listForUser(me, { limit: 50, ...f }).map((g) => g.id), want, JSON.stringify(f));
        assert.equal(auth.games.countForUser(me, f), real.games.countForUser(me, f));
        assert.equal(ac.games.countForUser(me, f), real.games.countForUser(me, f));
        const page = real.games.listForUser(me, { before: list[7].id, limit: 3, ...f }).map((g) => g.id);
        assert.deepEqual(auth.games.listForUser(me, { before: list[7].id, limit: 3, ...f }).map((g) => g.id), page);
        assert.deepEqual(ac.games.listForUser(me, { before: list[7].id, limit: 3, ...f }).map((g) => g.id), page);
    }
    real.close();
});

test('users.update({ email }) refuses an address of another account in every store', () => {
    const real = realStore();
    const auth = createAuthFake();
    const ac = createAnticheatFake();
    for (const store of [real, auth]) {
        const a = store.users.create({ username: 'Ann', email: 'ann@e.org' });
        store.users.create({ username: 'Bea', email: 'Bea@E.org' });
        assert.throws(() => store.users.update(a, { email: ' bea@e.ORG' }), (e) => e.code === 'email_taken');
        assert.equal(store.users.byId(a).email, 'ann@e.org');
        assert.equal(store.users.update(a, { email: 'new@e.org' }), true);
        assert.equal(store.users.byId(a).email, 'new@e.org');
    }
    const a = ac._.addUser('ann');
    ac._.addUser('bea');
    assert.throws(() => ac.users.update(a, { email: 'BEA@example.test' }), (e) => e.code === 'email_taken');
    ac.users.update(a, { email: 'ann2@example.test' });
    assert.equal(ac.users.byId(a).email, 'ann2@example.test');
    real.close();
});

test('tokens, sessions, security events, conduct, reports filed: the auth fake answers like the real store', () => {
    const real = realStore();
    const auth = createAuthFake({ now: () => T });
    for (const store of [real, auth]) {
        const me = store.users.create({ username: 'Tia', email: 't@e.org' });
        const other = store.users.create({ username: 'Uma', email: 'u@e.org' });
        store.tokens.create({ kind: 'email_change', tokenHash: 'h1', userId: me, data: { email: 'a@x.org' }, createdAt: T - 10, expiresAt: T + 1000 });
        store.tokens.create({ kind: 'email_change', tokenHash: 'h2', userId: me, data: { email: 'b@x.org' }, createdAt: T - 5, expiresAt: T - 1 });
        store.tokens.create({ kind: 'reset', tokenHash: 'h3', userId: me, createdAt: T, expiresAt: T + 1000 });
        const live = store.tokens.liveForUser(me, 'email_change', T);
        const data = typeof live.data === 'string' ? JSON.parse(live.data) : live.data;
        assert.equal(data.email, 'a@x.org');
        assert.equal(store.tokens.deleteForUser(me, 'email_change'), 2);
        assert.equal(store.tokens.liveForUser(me, 'email_change', T), null);
        assert.ok(store.tokens.liveForUser(me, 'reset', T));

        const s1 = store.sessions.create({ userId: me, tokenHash: Buffer.from('s1'), createdAt: T, expiresAt: T + 1000, idleExpiresAt: T + 1000, ip: '203.0.113.7' });
        const s2 = store.sessions.create({ userId: me, tokenHash: Buffer.from('s2'), createdAt: T + 1, expiresAt: T + 1000, idleExpiresAt: T + 1000 });
        assert.ok(Buffer.from('s2').equals(store.sessions.revoke(s2, me, T + 2)));
        assert.equal(store.sessions.revoke(s2, me, T + 3), null, 'already revoked');
        assert.deepEqual(store.sessions.listForUser(me).map((s) => s.id), [s1]);
        assert.deepEqual(Object.keys(store.sessions.listForUser(me)[0]).sort(), ['clientLabel', 'createdAt', 'expiresAt', 'id', 'idleExpiresAt', 'ip', 'lastSeenAt']);
        const all = store.sessions.allForUser(me);
        assert.deepEqual(all.map((s) => s.id), [s2, s1]);
        assert.equal(all[1].ip, '203.0.113.7');
        assert.ok(all[0].revokedAt > 0);
        assert.deepEqual(Object.keys(all[0]).sort(), ['clientLabel', 'createdAt', 'expiresAt', 'id', 'idleExpiresAt', 'ip', 'lastSeenAt', 'revokedAt']);

        store.security.insertBatch([{ kind: 'login', userId: me, ip: '203.0.113.7', at: T }, { kind: 'logout', userId: me, ip: null, at: T + 5 },
            { kind: 'login', userId: other, ip: null, at: T + 1 }]);
        assert.deepEqual(store.security.forUser(me).map((e) => [e.kind, e.at, e.ip]), [['logout', T + 5, null], ['login', T, '203.0.113.7']]);
        assert.equal(store.security.forUser(me, 1).length, 1);
    }
    // Rows the auth fake keeps for the reads it does not write.
    const me = auth.users.byUsername('Tia').id, other = auth.users.byUsername('Uma').id;
    auth._raw.conduct.push({ userId: me, kind: 'abandon', at: T }, { userId: me, kind: 'noshow', at: T + 3 }, { userId: other, kind: 'abort', at: T });
    assert.deepEqual(auth.conduct.forUser(me), [{ kind: 'noshow', at: T + 3 }, { kind: 'abandon', at: T }]);
    auth._raw.reports.push({ id: 1, reporterId: me, reportedId: other, gameId: 5, category: 'abuse', createdAt: T, status: 'dismissed' });
    const filed = auth.reports.forReporter(me);
    assert.equal(filed[0].reportedName, 'Uma');
    assert.equal(filed[0].outcome, 'dismissed');
    auth._raw.refunds.push({ id: 1, gameId: 5, victimId: me, cheaterId: other, category: '3+2', points: 8, createdAt: T });
    assert.equal(auth.refunds.list({ victimId: me })[0].points, 8);
    assert.deepEqual(auth.refunds.list({ victimId: other }), []);
    real.close();
});
