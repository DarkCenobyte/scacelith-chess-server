import test from 'node:test';
import assert from 'node:assert/strict';
import { testConfig } from '../../src/config.js';
import { register } from '../../src/http/routes/reports.js';
import { reporterWeight, cappedWeight, reviewPriority, validateReport, canReport, REPORT_RULES } from '../../src/anticheat/reports.js';
import { createFakeStore } from '../../src/anticheat/testing/fake-store.js';
import { StoreError } from '../../src/store/index.js';

const DAY = 86400000;
const NOW = 1_800_000_000_000;

function setup({ reportsPerDay = 5 } = {}) {
    const store = createFakeStore();
    const config = testConfig({ REPORTS_PER_DAY: String(reportsPerDay) });
    const routes = [];
    const router = { post: (path, handler, opts) => routes.push({ method: 'POST', path, handler, opts }), get() { throw new Error('no GET expected'); } };
    let now = NOW;
    register(router, { now: () => now });
    assert.equal(routes.length, 1);
    const route = routes[0];
    const security = [];
    const call = (user, body) => route.handler({ body, user, store, config, log: { security: (e, f) => security.push([e, f]) }, ip: '127.0.0.1', params: {}, query: {} });
    const users = {};
    for (const name of ['alice', 'bob', 'carol', 'dave']) {
        const id = store._.addUser(name);
        users[name] = store.users.byId(id);
        users[name].createdAt = NOW - 365 * DAY;
        store._.setRating(id, '5+0', { games: 200 });
    }
    let gid = 1000;
    const game = (white, black, endedAt = NOW - 3600000) => store._.addGame({ id: ++gid, whiteId: white.id, blackId: black.id, whiteName: white.username, blackName: black.username, endedAt, category: '5+0' });
    return { store, config, route, call, users, game, security, setNow: (t) => { now = t; } };
}

test('route registration: POST /api/v1/reports, auth required, rate limited', () => {
    const { route } = setup();
    assert.equal(route.path, '/api/v1/reports');
    assert.equal(route.opts.auth, 'required');
    assert.ok(route.opts.rate && route.opts.rate.limit > 0);
});

test('a player reports their opponent: 202, stored with a weight', () => {
    const { store, call, users, game, security } = setup();
    const g = game(users.alice, users.bob);
    const res = call(users.alice, { gameId: g, reported: 'BOB', category: 'cheating', comment: '  too perfect\u0007 ' });
    assert.deepEqual(res, { status: 202, body: { status: 'received' } });
    assert.equal(store._.reports.length, 1);
    const r = store._.reports[0];
    assert.equal(r.reporterId, users.alice.id);
    assert.equal(r.reportedId, users.bob.id);
    assert.equal(r.gameId, g);
    assert.equal(r.comment, 'too perfect');
    assert.equal(r.category, 'cheating');
    assert.ok(r.weight > 0.9 && r.weight <= 1, `weight ${r.weight}`);
    assert.equal(security[0][0], 'report.filed');
    // Game ids may come as strings (53-bit ids in JSON).
    const g2 = game(users.bob, users.alice);
    assert.equal(call(users.alice, { gameId: String(g2), reported: 'bob', category: 'abuse' }).status, 202);
});

test('the same answer for duplicates, nothing about the reported account leaks', () => {
    const { store, call, users, game } = setup();
    const g = game(users.alice, users.bob);
    const a = call(users.alice, { gameId: g, reported: 'bob', category: 'cheating' });
    const b = call(users.alice, { gameId: g, reported: 'bob', category: 'other', comment: 'again' });
    assert.deepEqual(a, b);
    assert.equal(store._.reports.length, 1);
    // A flagged or unflagged reported player gets exactly the same answer.
    store.integrity.set(users.carol.id, { level: 'high_confidence', score: 6, evidence: {} });
    const g2 = game(users.carol, users.alice);
    assert.deepEqual(call(users.alice, { gameId: g2, reported: 'carol', category: 'cheating' }), a);
});

test('a duplicate that passed the exists() check (a race between shards) gets the same 202, not an error', () => {
    const { store, call, users, game } = setup();
    const g = game(users.alice, users.bob);
    const first = call(users.alice, { gameId: g, reported: 'bob', category: 'cheating' });
    // Another shard's connection filed it between this one's exists() and create(): the real
    // store's UNIQUE (reporter, reported, game) index answers StoreError 'duplicate'.
    store.reports.exists = () => false;
    store.reports.create = () => { throw new StoreError('duplicate', 'UNIQUE constraint failed: reports.reporter_id, reports.reported_id, reports.game_id'); };
    assert.deepEqual(call(users.alice, { gameId: g, reported: 'bob', category: 'cheating' }), first);
    assert.equal(store._.reports.length, 1);
    // Any other store failure still reaches the route's error handling.
    store.reports.create = () => { throw new StoreError('busy', 'database is locked'); };
    assert.throws(() => call(users.alice, { gameId: g, reported: 'bob', category: 'cheating' }), /database is locked/);
});

test('eligibility: own game, against that opponent, ended within 7 days', () => {
    const { store, call, users, game } = setup();
    const g = game(users.alice, users.bob);
    const notAllowed = (res) => res.status === 403 && res.body.error === 'report_not_allowed';
    assert.ok(notAllowed(call(users.carol, { gameId: g, reported: 'bob', category: 'cheating' })), 'not a player of that game');
    assert.ok(notAllowed(call(users.alice, { gameId: g, reported: 'carol', category: 'cheating' })), 'not the opponent');
    assert.ok(notAllowed(call(users.alice, { gameId: g, reported: 'alice', category: 'cheating' })), 'oneself');
    assert.ok(notAllowed(call(users.alice, { gameId: 999999, reported: 'bob', category: 'cheating' })), 'unknown game');
    const old = game(users.alice, users.bob, NOW - 8 * DAY);
    assert.ok(notAllowed(call(users.alice, { gameId: old, reported: 'bob', category: 'cheating' })), 'too old');
    const recent = game(users.alice, users.bob, NOW - 6 * DAY);
    assert.equal(call(users.alice, { gameId: recent, reported: 'bob', category: 'cheating' }).status, 202);
    // A renamed opponent is still found through the account.
    const g3 = game(users.alice, users.dave);
    store.users.update(users.dave.id, { username: 'dave2' });
    assert.equal(call(users.alice, { gameId: g3, reported: 'dave2', category: 'cheating' }).status, 202);
    assert.equal(store._.reports.length, 2);
});

test('validation of the body', () => {
    const { call, users, game } = setup();
    const g = game(users.alice, users.bob);
    const bad = (body) => call(users.alice, body).status === 400;
    assert.ok(bad(null));
    assert.ok(bad({ gameId: -1, reported: 'bob', category: 'cheating' }));
    assert.ok(bad({ gameId: 1.5, reported: 'bob', category: 'cheating' }));
    assert.ok(bad({ gameId: g, reported: '', category: 'cheating' }));
    assert.ok(bad({ gameId: g, reported: 'x'.repeat(25), category: 'cheating' }));
    assert.ok(bad({ gameId: g, reported: 'bob', category: 'rude' }));
    assert.ok(bad({ gameId: g, reported: 'bob', category: 'other', comment: 'é'.repeat(501) }));
    assert.ok(bad({ gameId: g, reported: 'bob', category: 'other', comment: 42 }));
    assert.equal(validateReport({ gameId: g, reported: 'bob', category: 'other', comment: 'é'.repeat(500) }).value.comment.length, 500);
    assert.equal(call(null, { gameId: g, reported: 'bob', category: 'other' }).status, 401);
});

test('REPORTS_PER_DAY per reporter', () => {
    const { call, users, game, setNow } = setup({ reportsPerDay: 2 });
    const opponents = [users.bob, users.carol, users.dave];
    const ids = opponents.map((o) => game(users.alice, o));
    assert.equal(call(users.alice, { gameId: ids[0], reported: 'bob', category: 'cheating' }).status, 202);
    assert.equal(call(users.alice, { gameId: ids[1], reported: 'carol', category: 'cheating' }).status, 202);
    const res = call(users.alice, { gameId: ids[2], reported: 'dave', category: 'cheating' });
    assert.equal(res.status, 429);
    assert.equal(res.body.error, 'report_limit');
    setNow(NOW + DAY + 1);
    assert.equal(call(users.alice, { gameId: ids[2], reported: 'dave', category: 'cheating' }).status, 202);
});

test('reporter weight: account age, games played and track record', () => {
    const base = { now: NOW, createdAt: NOW - 365 * DAY, gamesPlayed: 500 };
    const vet = reporterWeight(base).weight;
    assert.equal(vet, 1);
    assert.ok(reporterWeight({ ...base, createdAt: NOW - 3600000 }).weight < 0.2, 'account one hour old');
    assert.ok(reporterWeight({ ...base, gamesPlayed: 2 }).weight < 0.3, 'hardly played');
    assert.ok(reporterWeight({ ...base, actioned: 5 }).weight > 1.5, 'useful reporter');
    assert.ok(reporterWeight({ ...base, dismissed: 6 }).weight <= 0.25, 'reports keep being dismissed');
    assert.ok(reporterWeight({ ...base, level: 'confirmed' }).weight <= 0.2);
    assert.ok(reporterWeight({ ...base, createdAt: NOW - 3600000, gamesPlayed: 0 }).weight >= 0.02);
});

test('past outcomes feed the weight of new reports', () => {
    const { store, call, users, game } = setup();
    const g1 = game(users.alice, users.bob);
    call(users.alice, { gameId: g1, reported: 'bob', category: 'cheating' });
    store.reports.resolve(store._.reports[0].id, 'dismissed', 'mod', NOW);
    const g2 = game(users.alice, users.carol);
    call(users.alice, { gameId: g2, reported: 'carol', category: 'cheating' });
    assert.ok(store._.reports[1].weight < store._.reports[0].weight, 'a dismissed report lowers credibility');
});

test('brigading: reports against one player do not add up linearly', () => {
    assert.equal(cappedWeight(1, [], NOW), 1);
    assert.equal(cappedWeight(1, [{ weight: 1, at: NOW - 1000 }, { weight: 0.8, at: NOW - 2000 }], NOW), 0.2);
    assert.equal(cappedWeight(1, [{ weight: 2, at: NOW - 2 * DAY }], NOW), 1, 'yesterday does not count');
    // Low-credibility reports share a small budget.
    assert.equal(cappedWeight(0.3, [{ weight: 0.3, at: NOW }], NOW), 0.2);
    assert.equal(cappedWeight(0.3, [{ weight: 0.3, at: NOW }, { weight: 0.2, at: NOW }], NOW), 0);

    const { store, call, users, game } = setup();
    // Ten brand-new accounts that each played the target once.
    for (let i = 0; i < 10; i++) {
        const id = store._.addUser(`sock${i}`);
        const u = store.users.byId(id);
        u.createdAt = NOW - 3600000;
        const g = game(u, users.bob);
        assert.equal(call(u, { gameId: g, reported: 'bob', category: 'cheating' }).status, 202);
    }
    const total = store._.reports.reduce((a, r) => a + r.weight, 0);
    assert.ok(total <= REPORT_RULES.lowCredDailyCap + 1e-9, `ten sock puppets weigh ${total}`);
    assert.equal(store._.reports.length, 10, 'all reports are kept for moderators');
    // A credible reporter still counts, up to the daily cap.
    const g = game(users.alice, users.bob);
    call(users.alice, { gameId: g, reported: 'bob', category: 'cheating' });
    assert.ok(store._.reports[10].weight >= 0.9);
});

test('only a credible report asks for the analysis at report priority; a low-credibility one at signal priority', () => {
    const { store, call, users, game } = setup();
    const requests = () => store.calls.filter((c) => c.name === 'analysis.request').map((c) => c.args[1]);
    // Credible reporters (a year old, 200 games): weight about 1 each, up to the daily cap of 2.
    call(users.alice, { gameId: game(users.alice, users.bob), reported: 'bob', category: 'cheating' });
    call(users.carol, { gameId: game(users.carol, users.bob), reported: 'bob', category: 'other' });
    // Beyond the cap the stored weight is 0: no longer credible.
    call(users.dave, { gameId: game(users.dave, users.bob), reported: 'bob', category: 'cheating' });
    // A brand-new account.
    const id = store._.addUser('fresh');
    const fresh = store.users.byId(id);
    fresh.createdAt = NOW - 3600000;
    call(fresh, { gameId: game(fresh, users.carol), reported: 'carol', category: 'cheating' });
    assert.deepEqual(store._.reports.map((r) => r.weight >= REPORT_RULES.lowCredibility), [true, true, false, false]);
    assert.deepEqual(requests(), ['report', 'report', 'signal', 'signal']);
});

test('the account age comes from the account when the session user object lacks it (as the HTTP server passes it)', () => {
    const { store, call, users, game } = setup();
    const session = { id: users.alice.id, userId: users.alice.id, username: 'alice', emailVerified: true };
    assert.equal(call(session, { gameId: game(users.alice, users.bob), reported: 'bob', category: 'cheating' }).status, 202);
    assert.ok(store._.reports[0].weight > 0.9, `a year-old account with 200 games: ${store._.reports[0].weight}`);
    const id = store._.addUser('newbie');
    store.users.byId(id).createdAt = NOW - 3600000;
    const fresh = { id, userId: id, username: 'newbie' };
    call(fresh, { gameId: game(store.users.byId(id), users.carol), reported: 'carol', category: 'cheating' });
    assert.ok(store._.reports[1].weight < 0.2, 'an account one hour old stays low');
});

test('canReport: the rules of POST /reports, answered for one game without filing anything', () => {
    const { store, config, call, users, game, setNow } = setup({ reportsPerDay: 2 });
    const byId = (gid) => store.games.byId(gid);
    const g = game(users.alice, users.bob);
    assert.equal(canReport(store, config, users.alice.id, byId(g), NOW), true);
    assert.equal(canReport(store, config, users.bob.id, byId(g), NOW), true, 'either player');
    assert.equal(canReport(store, config, users.carol.id, byId(g), NOW), false, 'not a player of that game');
    assert.equal(canReport(store, config, users.alice.id, null, NOW), false, 'no game');
    assert.equal(canReport(store, config, users.alice.id, byId(game(users.alice, users.bob, NOW - 8 * DAY)), NOW), false, 'too old');
    assert.equal(canReport(store, config, users.alice.id, byId(game(users.alice, users.bob, NOW + 3600000)), NOW), false, 'not ended yet');
    assert.equal(canReport(store, config, users.alice.id, byId(game(users.alice, users.alice)), NOW), false, 'against oneself');
    assert.equal(store._.reports.length, 0, 'nothing filed');
    assert.equal(call(users.alice, { gameId: g, reported: 'bob', category: 'cheating' }).status, 202);
    assert.equal(canReport(store, config, users.alice.id, byId(g), NOW), false, 'already reported');
    assert.equal(canReport(store, config, users.bob.id, byId(g), NOW), true, 'the opponent still may');
    const g2 = game(users.alice, users.carol);
    const g3 = game(users.alice, users.dave);
    assert.equal(call(users.alice, { gameId: g2, reported: 'carol', category: 'abuse' }).status, 202);
    assert.equal(canReport(store, config, users.alice.id, byId(g3), NOW), false, 'REPORTS_PER_DAY reached');
    assert.equal(call(users.alice, { gameId: g3, reported: 'dave', category: 'abuse' }).status, 429, 'as POST answers');
    setNow(NOW + DAY + 1);
    assert.equal(canReport(store, config, users.alice.id, byId(g3), NOW + DAY + 1), true, 'the next day');
    assert.equal(canReport({ reports: { countByReporterSince() { throw new Error('db down'); } } }, config, users.alice.id, byId(g3), NOW), false);
});

test('review priority grows with level, score and (logarithmically) with reports', () => {
    assert.equal(reviewPriority({}), 0);
    const one = reviewPriority({ reportWeight: 1 });
    const ten = reviewPriority({ reportWeight: 10 });
    assert.ok(one > 0 && ten > one && ten < 10 * one);
    assert.ok(reviewPriority({ level: 'high_confidence', score: 4 }) > reviewPriority({ level: 'suspected', score: 4 }));
    assert.ok(reviewPriority({ level: 'suspected', score: 3.6 }) > reviewPriority({ reportWeight: 2 }));
});
