import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { testConfig } from '../../src/config.js';
import { runAdmin, parseArgs, COMMANDS } from '../../src/anticheat/admin.js';
import { createFakeStore } from '../../src/anticheat/testing/fake-store.js';
import { reviewPriority } from '../../src/anticheat/reports.js';
import { openStore, migrate, AnalysisPriority } from '../../src/store/index.js';
import { enums } from '../../src/protocol/index.js';
import { DatabaseSync } from 'node:sqlite';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const NOW = 1_800_000_000_000;

function sink() { let s = ''; return { write: (x) => { s += x; }, get text() { return s; } }; }

async function run(store, argv, extra = {}) {
    const out = sink(), err = sink();
    const security = [];
    const code = await runAdmin(argv, { store, config: testConfig(), out, err, now: () => NOW, moderator: 'mod-anna', log: { security: (e, f) => security.push([e, f]) }, ...extra });
    return { code, out: out.text, err: err.text, security, json: argv.includes('--json') && code === 0 ? JSON.parse(out.text) : null };
}

function world() {
    const store = createFakeStore();
    const bob = store._.addUser('bob');
    const eve = store._.addUser('eve');
    store._.setRating(bob, '5+0', { rating: 1620, games: 44 });
    store.sessions.create({ userId: eve, tokenHash: 'h1', createdAt: NOW - 1000, expiresAt: NOW + 1e9, idleExpiresAt: NOW + 1e9 });
    store.sessions.create({ userId: eve, tokenHash: 'h2', createdAt: NOW - 1000, expiresAt: NOW + 1e9, idleExpiresAt: NOW + 1e9 });
    return { store, bob, eve };
}

const moderatorEvents = (store) => store._.security.filter((e) => e.kind === 'moderator_action');

test('argument parsing', () => {
    assert.deepEqual(parseArgs(['user', 'ban', 'eve', '--hours', '5', '--reason=spam here', '--json']),
        { positional: ['user', 'ban', 'eve'], flags: { hours: '5', reason: 'spam here', json: true } });
    assert.deepEqual(parseArgs(['bench-accounts', '--i-know-this-is-a-test-server', '--count', '3']).flags, { 'i-know-this-is-a-test-server': true, count: '3' });
    assert.ok(Object.keys(COMMANDS).length >= 15);
});

test('usage and unknown commands', async () => {
    const { store } = world();
    const r = await run(store, ['nope']);
    assert.equal(r.code, 2);
    assert.match(r.err, /Usage/);
    // Names inherited from Object.prototype are not commands either.
    for (const name of ['constructor', 'toString', '__proto__', 'hasOwnProperty']) {
        const x = await run(store, [name]);
        assert.equal(x.code, 2, name);
        assert.match(x.err, /Usage/);
        assert.equal((await run(store, [name, '--help'])).code, 2, `${name} --help`);
    }
    const e = await run(store, ['user', 'show', 'nobody']);
    assert.equal(e.code, 1);
    assert.match(e.err, /no user named "nobody"/);
});

test('user show hides secrets', async () => {
    const { store, bob } = world();
    store.users.update(bob, { passwordHash: 'scrypt$secret', mfaSecretEnc: 'enc' });
    const r = await run(store, ['user', 'show', 'bob', '--json']);
    assert.equal(r.code, 0);
    assert.equal(r.json.user.username, 'bob');
    assert.equal(r.json.ratings[0].rating, 1620);
    assert.ok(!JSON.stringify(r.json).includes('scrypt$secret'));
    assert.ok(!JSON.stringify(r.json).includes('"enc"'));
    const t = await run(store, ['user', 'show', 'bob']);
    assert.match(t.out, /integrity none/);
});

test('user show counts the sessions that are neither expired nor idle-expired', async () => {
    const { store, eve } = world();
    store.sessions.create({ userId: eve, tokenHash: 'idle', createdAt: NOW - 40 * 86400000, expiresAt: NOW + 1e9, idleExpiresAt: NOW - 1 });
    store.sessions.create({ userId: eve, tokenHash: 'old', createdAt: NOW - 100 * 86400000, expiresAt: NOW - 1, idleExpiresAt: NOW + 1e9 });
    const r = await run(store, ['user', 'show', 'eve', '--json']);
    assert.equal(r.json.activeSessions, 2, 'the two live sessions of world()');
});

test('user ban / unban are audited', async () => {
    const { store, eve } = world();
    assert.equal((await run(store, ['user', 'ban', 'eve', '--hours', '5'])).code, 1, 'reason required');
    assert.equal((await run(store, ['user', 'ban', 'eve', '--hours', 'x', '--reason', 'r'])).code, 1);
    const r = await run(store, ['user', 'ban', 'eve', '--hours', '5', '--reason', 'harassment', '--revoke-sessions']);
    assert.equal(r.code, 0, r.err);
    const ban = store.sanctions.activeBan(eve, NOW);
    assert.equal(ban.endsAt, NOW + 5 * 3600000);
    assert.equal(ban.source, 'moderator');
    assert.equal(ban.createdBy, 'mod-anna');
    assert.equal(store.sessions.listForUser(eve).filter((s) => !s.revokedAt).length, 0);
    assert.deepEqual(moderatorEvents(store).map((e) => e.detail.action), ['ban']);
    assert.equal(r.security[0][0], 'moderator.action');
    const u = await run(store, ['user', 'unban', 'eve', '--by', 'mod-ben']);
    assert.equal(u.code, 0);
    assert.equal(store.sanctions.activeBan(eve, NOW), null);
    assert.equal(store._.sanctions[0].liftedBy, 'mod-ben');
    assert.equal(moderatorEvents(store)[1].detail.moderator, 'mod-ben');
    assert.match((await run(store, ['user', 'unban', 'eve'])).out, /no active ban/);
});

test('user reset-mfa, verify-email, revoke-sessions', async () => {
    const { store, eve } = world();
    store.users.update(eve, { mfaEnabled: true, mfaSecretEnc: 'x', emailVerified: false });
    store.mfa.replaceRecoveryCodes(eve, ['a', 'b']);
    assert.equal((await run(store, ['user', 'reset-mfa', 'eve'])).code, 0);
    const u = store.users.byId(eve);
    assert.equal(u.mfaEnabled, false);
    assert.equal(u.mfaSecretEnc, null);
    assert.equal(store.mfa.countRecoveryCodes(eve), 0);
    assert.equal(store.sessions.listForUser(eve).filter((s) => !s.revokedAt).length, 0);
    assert.equal((await run(store, ['user', 'verify-email', 'eve'])).code, 0);
    assert.equal(store.users.byId(eve).emailVerified, true);
    store.sessions.create({ userId: eve, tokenHash: 'h3', createdAt: NOW, expiresAt: NOW + 1e9, idleExpiresAt: NOW + 1e9 });
    const r = await run(store, ['user', 'revoke-sessions', 'eve', '--json']);
    assert.equal(r.json.revokedSessions, 1);
    assert.deepEqual(moderatorEvents(store).map((e) => e.detail.action), ['reset_mfa', 'verify_email', 'revoke_sessions']);
});

test('user show / verify-email reach the pending signup that holds a name (its confirmation mail never arrived)', async (t) => {
    const store = openStore(testConfig({ DB_PATH: ':memory:' }));
    t.after(() => store.close());
    migrate(store);
    const signup = (username, email, over = {}) => store.signups.create({ username, email, passwordHash: `scrypt$${username}`,
        tokenHash: `link-${username}`, createdAt: NOW - 3600000, expiresAt: NOW + 3600000, ...over });
    signup('Alice_1', 'alice@example.org');
    const show = await run(store, ['user', 'show', 'alice_1', '--json']);
    assert.equal(show.code, 0, show.err);
    assert.deepEqual(show.json, { pendingSignup: { username: 'Alice_1', email: 'alice@example.org', createdAt: NOW - 3600000,
        expiresAt: NOW + 3600000, link: true } });
    assert.ok(!show.out.includes('scrypt$'));
    assert.match((await run(store, ['user', 'show', 'alice_1'])).out, /^Pending signup Alice_1: no account until its link is used/);

    // The account is created as the link would: its address verified, the signup's password, the signup gone.
    const r = await run(store, ['user', 'verify-email', 'alice_1', '--by', 'mod-ben']);
    assert.equal(r.code, 0, r.err);
    const u = store.users.byUsername('alice_1');
    assert.deepEqual([u.username, u.email, u.emailVerified, u.passwordHash, u.createdAt], ['Alice_1', 'alice@example.org', true, 'scrypt$Alice_1', NOW]);
    assert.match(r.out, new RegExp(`Account Alice_1 \\(#${u.id}\\) created`));
    assert.equal(store.signups.byUsername('alice_1'), null);
    const events = store.security.forUser(u.id).map((e) => [e.kind, e.detail?.action ?? null, e.detail?.moderator ?? null]);
    assert.deepEqual(events.sort(), [['moderator_action', 'confirm_signup', 'mod-ben'], ['register', null, null]]);
    // Then the account itself, as before.
    assert.match((await run(store, ['user', 'verify-email', 'alice_1'])).out, /E-mail address of Alice_1 marked verified/);
    assert.equal((await run(store, ['user', 'show', 'alice_1', '--json'])).json.user.id, u.id);

    // A signup whose address has an account (no link) or another account took meanwhile: dropped, nothing created.
    signup('Bob_2', 'Alice@Example.org', { tokenHash: null });
    assert.match((await run(store, ['user', 'show', 'bob_2'])).out, /no link \(the address had an account/);
    const taken = await run(store, ['user', 'verify-email', 'bob_2', '--json']);
    assert.deepEqual([taken.code, taken.json], [0, { status: 'taken' }]);
    assert.equal(store.users.byUsername('bob_2'), null);
    assert.equal(store.signups.byUsername('bob_2'), null, 'the name is free again');

    // An expired signup holds nothing: no such user.
    signup('Cleo_3', 'cleo@example.org', { expiresAt: NOW });
    for (const cmd of ['show', 'verify-email']) {
        const x = await run(store, ['user', cmd, 'cleo_3']);
        assert.deepEqual([x.code, x.err], [1, 'error: no user named "cleo_3"\n'], cmd);
    }
    assert.equal(store.users.byUsername('cleo_3'), null);
    assert.notEqual(store.signups.byUsername('cleo_3'), null, 'left to the retention purge');
});

test('integrity list / show / confirm / clear', async () => {
    const { store, bob, eve } = world();
    store.integrity.set(bob, { level: 'suspected', score: 3.8, evidence: { statistics: { model: 1, computedAt: NOW, level: 'suspected', score: 3.8, groups: { Q: 3.8, E: 2, J: 0, T: 0.4 }, reasons: ['Move quality Q=3.80 over 12 games'], windows: { all: { games: 12 } } } }, updatedAt: NOW });
    store.integrity.set(eve, { level: 'high_confidence', score: 4.5, evidence: {}, updatedAt: NOW });
    store._.addGame({ id: 77, whiteId: bob, blackId: eve, endedAt: NOW });
    store.analysis.complete(77, { v: 1, gameId: 77, category: '5+0', white: { userId: bob, rating: 1600, n: 30, accuracy: 97.2, acpl: 9, t1Deep: 0.9, t1Fast: 0.8, t1Complex: 0.85, nComplex: 12, timeCorr: 0.01, timeCv: 0.3 }, black: { userId: eve, n: 30 } });
    store.anomalies.insertBatch([{ userId: bob, gameId: 77, kind: 'clock_implausible', severity: 'suspicious', detail: { thinkMs: 9000 }, at: NOW }]);
    store.reports.create({ reporterId: eve, reportedId: bob, gameId: 77, category: 'cheating', comment: 'engine', weight: 1, at: NOW });

    const list = await run(store, ['integrity', 'list', '--json']);
    assert.deepEqual(list.json.map((r) => r.username), ['eve', 'bob'], 'sorted by review priority');
    assert.ok(list.json.find((r) => r.username === 'bob').reports30d === 1);
    const hc = await run(store, ['integrity', 'list', '--level', 'high_confidence', '--json']);
    assert.deepEqual(hc.json.map((r) => r.username), ['eve']);
    assert.equal((await run(store, ['integrity', 'list', '--level', 'none'])).code, 1);

    const show = await run(store, ['integrity', 'show', 'bob']);
    assert.equal(show.code, 0);
    assert.match(show.out, /Move quality Q=3.80/);
    assert.match(show.out, /97\.2/);
    assert.match(show.out, /clock_implausible/);
    assert.match(show.out, /engine/);

    const conf = await run(store, ['integrity', 'confirm', 'bob', '--reason', 'engine moves confirmed by review', '--hours', '720']);
    assert.equal(conf.code, 0, conf.err);
    const ib = store.integrity.get(bob);
    assert.equal(ib.level, 'confirmed');
    assert.equal(ib.reviewedBy, 'mod-anna');
    assert.equal(ib.evidence.reviews[0].action, 'confirm');
    assert.equal(ib.evidence.statistics.score, 3.8, 'evidence kept');
    assert.equal(store.sanctions.activeBan(bob, NOW).endsAt, NOW + 720 * 3600000);
    assert.equal(store._.reports[0].outcome, 'actioned', 'open cheating reports actioned');

    const clr = await run(store, ['integrity', 'clear', 'eve', '--reason', 'strong club player, verified']);
    assert.equal(clr.code, 0);
    const ie = store.integrity.get(eve);
    assert.equal(ie.level, 'none');
    assert.equal(ie.reviewedBy, 'mod-anna');
    assert.equal(ie.evidence.review.clearedScore, 4.5);
    assert.deepEqual(moderatorEvents(store).map((e) => e.detail.action), ['integrity_confirm', 'integrity_clear']);
});

test('integrity show: a side without scored moves shows no rates, not 0 %', async () => {
    const { store, bob, eve } = world();
    store._.addGame({ id: 78, whiteId: bob, blackId: eve, endedAt: NOW });
    // analyzer.js emptySide(): no scored move, every rate null.
    const empty = { n: 0, accuracy: null, acpl: null, t1Deep: null, t1Fast: null, nComplex: 0, t1Complex: null, timeCorr: null, timeCv: null };
    store.analysis.complete(78, { v: 1, gameId: 78, category: '5+0', white: { userId: bob, rating: 1600, ...empty }, black: { userId: eve, ...empty } });
    const show = await run(store, ['integrity', 'show', 'bob']);
    assert.equal(show.code, 0);
    const row = show.out.split('\n').find((l) => /^78 /.test(l));
    assert.deepEqual(row.trim().split(/\s+/), ['78', '5+0', '1600', '0', '-', '-', '-', '-', '-', '-', '-']);
});

test('integrity show: a report comment cannot break the table or forge lines', async () => {
    const { store, bob, eve } = world();
    const plain = (await run(store, ['integrity', 'show', 'bob'])).out;
    store.reports.create({ reporterId: eve, reportedId: bob, gameId: 1, category: 'cheating', comment: 'x\nSanctions\n(none)\u009b\u202eabc\u2028', weight: 1, at: NOW });
    const show = await run(store, ['integrity', 'show', 'bob']);
    assert.equal(show.out.split('\n').length, plain.split('\n').length + 2, 'one report row (and its table header)');
    assert.ok(show.out.includes('x\\u000aSanctions\\u000a(none)\\u009b\\u202eabc\\u2028'));
    assert.doesNotMatch(show.out, /[\u0080-\u009f\u2028\u202e]/);
});

test('integrity confirm: a ban that cannot be stored leaves the level as it was', async () => {
    const { store, bob } = world();
    store.integrity.set(bob, { level: 'suspected', score: 3.8, evidence: {}, updatedAt: NOW });
    store.sanctions.create = () => { throw new Error('database is locked'); };
    await assert.rejects(run(store, ['integrity', 'confirm', 'bob', '--reason', 'engine', '--no-refund']), /locked/);
    assert.equal(store.integrity.get(bob).level, 'suspected');
    assert.deepEqual(moderatorEvents(store), []);
});

test('reports list / resolve, anomalies, stats', async () => {
    const { store, bob, eve } = world();
    const r1 = store.reports.create({ reporterId: eve, reportedId: bob, gameId: 1, category: 'cheating', comment: '', weight: 0.8, at: NOW });
    store.reports.create({ reporterId: bob, reportedId: eve, gameId: 1, category: 'abuse', comment: '', weight: 0.1, at: NOW });
    store.integrity.set(eve, { level: 'suspected', score: 3.6, evidence: {} });
    const list = await run(store, ['reports', 'list', '--json']);
    assert.deepEqual(list.json.map((r) => r.username), ['eve', 'bob']);
    assert.deepEqual(list.json[1].ids, [r1]);
    assert.equal((await run(store, ['reports', 'resolve', String(r1), 'maybe'])).code, 1);
    assert.equal((await run(store, ['reports', 'resolve', String(r1), 'dismissed'])).code, 0);
    assert.equal(store._.reports[0].outcome, 'dismissed');
    assert.equal(store._.reports[0].resolvedBy, 'mod-anna');
    assert.equal((await run(store, ['reports', 'resolve', String(r1), 'actioned'])).code, 1, 'already resolved');
    store.anomalies.insertBatch([{ userId: bob, gameId: 5, kind: 'bad_seq', severity: 'suspicious', detail: '{"count":3}', at: NOW }]);
    const an = await run(store, ['anomalies', 'bob', '--json']);
    assert.equal(an.json[0].detail.count, 3);
    const st = await run(store, ['stats', '--json']);
    assert.deepEqual(st.json.integrity, { suspected: 1, high_confidence: 0, confirmed: 0 });
    assert.equal(st.json.openReports, 1);
});

test('integrity show / reports list on the real store: report dates and outcomes', async (t) => {
    const store = openStore(testConfig({ DB_PATH: ':memory:' }));
    t.after(() => store.close());
    migrate(store);
    const bob = store.users.create({ username: 'bob', email: 'bob@example.org' });
    const eve = store.users.create({ username: 'eve', email: 'eve@example.org' });
    const ann = store.users.create({ username: 'ann', email: 'ann@example.org' });
    const r1 = store.reports.create({ reporterId: eve, reportedId: bob, gameId: 0, category: 'cheating', comment: 'first', weight: 1, at: NOW - 7200000 });
    const r2 = store.reports.create({ reporterId: ann, reportedId: bob, gameId: 0, category: 'cheating', comment: 'second', weight: 0.5, at: NOW - 3600000 });
    store.reports.resolve(r1, 'dismissed', 'mod', NOW - 60000);
    const show = await run(store, ['integrity', 'show', 'bob']);
    assert.equal(show.code, 0, show.err);
    const row = (id) => show.out.split('\n').find((l) => l.startsWith(`${id} `)).trim().split(/\s+/);
    assert.deepEqual([row(r1)[1], row(r1)[5]], [new Date(NOW - 7200000).toISOString().replace('.000Z', 'Z'), 'dismissed']);
    assert.deepEqual([row(r2)[1], row(r2)[5]], [new Date(NOW - 3600000).toISOString().replace('.000Z', 'Z'), 'open']);
    const list = await run(store, ['reports', 'list', '--json']);
    assert.deepEqual(list.json.map((r) => [r.username, r.ids, r.latest]), [['bob', [r2], NOW - 3600000]]);
});

test('integrity confirm / clear resolve every open cheating report, not only those among the newest 200', async (t) => {
    const store = openStore(testConfig({ DB_PATH: ':memory:' }));
    t.after(() => store.close());
    migrate(store);
    const bob = store.users.create({ username: 'bob', email: 'bob@example.org' });
    const eve = store.users.create({ username: 'eve', email: 'eve@example.org' });
    const ann = store.users.create({ username: 'ann', email: 'ann@example.org' });
    // Per reported player: 20 old open cheating reports, then 200 newer ones, resolved or about abuse.
    const file = (reportedId, game, category, at) => store.reports.create({ reporterId: ann, reportedId, gameId: game, category, comment: '', weight: 0.1, at });
    for (const target of [bob, eve]) {
        for (let i = 0; i < 20; i++) file(target, 1000 + i, 'cheating', NOW - 10 * 86400000 + i);
        for (let i = 0; i < 200; i++) {
            const id = file(target, 2000 + i, i % 2 ? 'abuse' : 'cheating', NOW - 86400000 + i);
            store.reports.resolve(id, 'dismissed', 'mod', NOW - 86400000 + i);
        }
    }
    const open = (userId) => store.reports.listOpen(1000).filter((r) => r.reportedId === userId);
    assert.equal(open(eve).length, 20);
    const clr = await run(store, ['integrity', 'clear', 'eve', '--dismiss-reports', '--json']);
    assert.equal(clr.code, 0, clr.err);
    assert.equal(clr.json.reportsDismissed, 20);
    assert.deepEqual(open(eve), []);
    assert.equal(store.reports.forReporter(ann, 1000).filter((r) => r.reportedId === eve && r.outcome === 'dismissed' && r.resolvedBy === 'mod-anna').length, 20);
    const conf = await run(store, ['integrity', 'confirm', 'bob', '--reason', 'engine', '--no-refund', '--json']);
    assert.equal(conf.code, 0, conf.err);
    assert.equal(conf.json.reportsActioned, 20);
    assert.deepEqual(open(bob), []);
    assert.equal(store.reports.forReporter(ann, 1000).filter((r) => r.reportedId === bob && r.outcome === 'actioned').length, 20);
});

test('user show, integrity list / show and reports list count and weigh every report, not only the newest 200', async (t) => {
    const store = openStore(testConfig({ DB_PATH: ':memory:' }));
    t.after(() => store.close());
    migrate(store);
    const bob = store.users.create({ username: 'bob', email: 'bob@example.org' });
    const ann = store.users.create({ username: 'ann', email: 'ann@example.org' });
    store.integrity.set(bob, { level: 'suspected', score: 2, evidence: {}, updatedAt: NOW });
    // One open report older than 30 days, then 250 of weight 0.1 (25 in all): the 30 oldest open.
    store.reports.create({ reporterId: ann, reportedId: bob, gameId: 1, category: 'cheating', comment: '', weight: 1, at: NOW - 40 * 86400000 });
    for (let i = 0; i < 250; i++) {
        const id = store.reports.create({ reporterId: ann, reportedId: bob, gameId: 100 + i, category: 'cheating', comment: '', weight: 0.1, at: NOW - 20 * 86400000 + i * 60000 });
        if (i >= 30) store.reports.resolve(id, 'dismissed', 'mod', NOW);
    }
    const priority = reviewPriority({ level: 'suspected', score: 2, reportWeight: 25 });
    const user = await run(store, ['user', 'show', 'bob', '--json']);
    assert.equal(user.code, 0, user.err);
    assert.deepEqual(user.json.reports, { total: 251, open: 31, weight30d: 25 });
    assert.match((await run(store, ['user', 'show', 'bob'])).out, /reports received: 251 \(31 open\)/);
    const list = await run(store, ['integrity', 'list', '--json']);
    assert.deepEqual(list.json.map((r) => [r.username, r.reports30d, r.priority]), [['bob', 25, priority]]);
    assert.equal((await run(store, ['integrity', 'show', 'bob', '--json'])).json.priority, priority);
    const open = await run(store, ['reports', 'list', '--json']);
    assert.deepEqual(open.json.map((r) => [r.username, r.open, r.weight, r.priority]), [['bob', 31, 4, priority]]);
});

test('analysis queue: the game is analysed before every other one; a game being or already analysed is left alone', async (t) => {
    const keep = (r) => ({ before: r.rating, after: r.rating });
    const store = openStore(testConfig({ DB_PATH: ':memory:' }), { applyGame: (w, b) => ({ white: keep(w), black: keep(b) }) });
    t.after(() => store.close());
    migrate(store);
    const [ann, ben] = ['ann', 'ben'].map((n) => store.users.create({ username: n, email: `${n}@example.org` }));
    let lastId = 5_000_000_000_000;
    const game = (extra = {}) => ({
        id: ++lastId, category: '5+0', rated: true, baseMs: 300000, incMs: 0, whiteId: ann, blackId: ben, whiteName: 'ann', blackName: 'ben',
        startedAt: NOW - 600000, endedAt: NOW, status: enums.GameStatus.WhiteWins, reason: enums.EndReason.Resignation,
        moves: new Uint16Array(40), spentMs: new Uint32Array(40), clockMs: new Uint32Array(40), ...extra,
    });
    // Rated games are queued at the ordinary priority; the casual and the short one are left out.
    const [running, done, failed, ordinary, reported, casual, short] = [game(), game(), game(), game(), game(), game({ rated: false }),
        game({ moves: new Uint16Array(8), spentMs: new Uint32Array(8), clockMs: new Uint32Array(8) })];
    store.games.finishBatch([running, done, failed, ordinary, reported, casual, short]);
    assert.deepEqual(store.analysis.next(3, 'w', NOW).map((j) => j.gameId), [running.id, done.id, failed.id]);
    store.analysis.complete(done.id, { gameId: done.id }, NOW);
    for (let i = 0; i < 3; i++) {
        if (store.analysis.fail(failed.id, 'engine crashed', NOW) === 'queued') assert.equal(store.analysis.next(1, 'w', NOW)[0].gameId, failed.id);
    }
    assert.equal(store.analysis.request(reported.id, 'report', NOW), true);
    const job = (g) => { const j = store.analysis.job(g.id); return [j.status, j.priority]; };
    assert.deepEqual([running, done, failed, ordinary, reported].map(job),
        [['running', 0], ['done', 0], ['failed', 0], ['queued', AnalysisPriority.ordinary], ['queued', AnalysisPriority.report]]);
    assert.deepEqual([store.analysis.job(casual.id), store.analysis.job(short.id)], [null, null]);

    for (const bad of [[], ['abc'], ['0'], ['-3'], ['1.5']]) {
        const r = await run(store, ['analysis', 'queue', ...bad]);
        assert.equal(r.code, 1, bad.join());
        assert.match(r.err, /analysis queue <gameId>/);
    }
    const missing = await run(store, ['analysis', 'queue', '424242']);
    assert.equal(missing.code, 1);
    assert.match(missing.err, /no game #424242/);

    const first = await run(store, ['analysis', 'queue', String(casual.id), '--json']);
    assert.equal(first.code, 0, first.err);
    assert.deepEqual(first.json, { gameId: casual.id, queued: true, previous: null });
    assert.deepEqual(first.security, [['moderator.action', { userId: null, action: 'analysis_queue', moderator: 'mod-anna', gameId: casual.id, previousStatus: null }]]);
    assert.match((await run(store, ['analysis', 'queue', String(short.id)])).out, /queued .* it was not in the queue \(a casual or short game/);
    assert.match((await run(store, ['analysis', 'queue', String(ordinary.id)])).out, /queued .* it was waiting at priority ordinary/);
    assert.match((await run(store, ['analysis', 'queue', String(failed.id)])).out, /its analysis had failed \(engine crashed\)/);
    assert.equal(store.analysis.job(failed.id).attempts, 0);
    for (const [g, said] of [[running, /is being analysed now/], [done, /was already analysed/], [casual, /is already queued before every other game/]]) {
        const r = await run(store, ['analysis', 'queue', String(g.id)]);
        assert.equal(r.code, 0, r.err);
        assert.match(r.out, said);
        assert.deepEqual(r.security, [], 'nothing changed, nothing audited');
    }
    assert.deepEqual([running, done, casual, short, ordinary, failed].map(job), [['running', 0], ['done', 0],
        ['queued', AnalysisPriority.manual], ['queued', AnalysisPriority.manual], ['queued', AnalysisPriority.manual], ['queued', AnalysisPriority.manual]]);
    // The engine takes the four requested games before the reported one.
    assert.deepEqual(store.analysis.next(4, 'w', NOW).map((j) => j.gameId).sort(), [ordinary.id, failed.id, casual.id, short.id].sort());
    assert.deepEqual(job(reported), ['queued', AnalysisPriority.report]);
});

test('bench-accounts: refused without the test-server flag; creates verified accounts with sessions', async (t) => {
    const { store } = world();
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bench-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const out = path.join(dir, 'tokens.txt');
    const refused = await run(store, ['bench-accounts', '--count', '3', '--out', out]);
    assert.equal(refused.code, 1);
    assert.match(refused.err, /i-know-this-is-a-test-server/);
    assert.equal(fs.existsSync(out), false);

    const hashToken = (tok) => 'H:' + crypto.createHash('sha256').update(tok).digest('base64url');
    const r = await run(store, ['bench-accounts', '--count', '3', '--prefix', 'bench', '--out', out, '--i-know-this-is-a-test-server'], { hashToken });
    assert.equal(r.code, 0, r.err);
    const tokens = fs.readFileSync(out, 'utf8').trim().split('\n');
    assert.equal(tokens.length, 3);
    for (const tok of tokens) assert.match(tok, /^sct_[A-Za-z0-9_-]{43}$/);
    if (process.platform !== 'win32') assert.equal(fs.statSync(out).mode & 0o777, 0o600);
    const u = store.users.byUsername('bench0002');
    assert.equal(u.emailVerified, true);
    const sess = store.sessions.listForUser(u.id);
    assert.equal(sess.length, 1);
    assert.equal(sess[0].tokenHash, hashToken(tokens[1]));
    assert.ok(sess[0].expiresAt > NOW);
    // Running again reuses the accounts and adds sessions; an existing file is made 600 too.
    if (process.platform !== 'win32') fs.chmodSync(out, 0o644);
    const again = await run(store, ['bench-accounts', '--count', '3', '--out', out, '--format', 'tsv', '--i-know-this-is-a-test-server']);
    assert.equal(again.json, null);
    assert.match(again.out, /0 created, 3 reused/);
    assert.match(fs.readFileSync(out, 'utf8'), /^bench0001\tsct_/);
    assert.equal(fs.readFileSync(out, 'utf8').trim().split('\n').length, 3);
    if (process.platform !== 'win32') assert.equal(fs.statSync(out).mode & 0o777, 0o600);
    // A real account with a matching name is never taken over.
    store._.addUser('bench0004');
    assert.equal((await run(store, ['bench-accounts', '--count', '4', '--out', out, '--i-know-this-is-a-test-server'])).code, 1);
});

test('bench-accounts: a pipe keeps its mode; another user\'s file is refused before any account is created', { skip: process.platform === 'win32' }, async (t) => {
    const { store } = world();
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bench-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    // A FIFO stands for a pipe or /dev/null: written to, never chmod'ed.
    const fifo = path.join(dir, 'tokens.fifo');
    assert.equal(spawnSync('mkfifo', ['-m', '644', fifo]).status, 0);
    const reader = fs.openSync(fifo, fs.constants.O_RDONLY | fs.constants.O_NONBLOCK);
    t.after(() => fs.closeSync(reader));
    const r = await run(store, ['bench-accounts', '--count', '2', '--out', fifo, '--i-know-this-is-a-test-server']);
    assert.equal(r.code, 0, r.err);
    const buf = Buffer.alloc(4096);
    const tokens = buf.subarray(0, fs.readSync(reader, buf)).toString().trim().split('\n');
    assert.equal(tokens.length, 2);
    for (const tok of tokens) assert.match(tok, /^sct_[A-Za-z0-9_-]{43}$/);
    assert.equal(fs.statSync(fifo).mode & 0o777, 0o644);

    // A regular file of another owner cannot be made 600: refused up front.
    const out = path.join(dir, 'theirs.txt');
    fs.writeFileSync(out, 'kept\n');
    t.mock.method(process, 'geteuid', () => fs.statSync(out).uid + 1);
    const refused = await run(store, ['bench-accounts', '--count', '3', '--prefix', 'other', '--out', out, '--i-know-this-is-a-test-server']);
    assert.equal(refused.code, 1);
    assert.match(refused.err, /belongs to another user/);
    assert.equal(store.users.byUsername('other0001'), null);
    assert.equal(fs.readFileSync(out, 'utf8'), 'kept\n');
});

test('bench-accounts: a symbolic link as --out is refused before any account is created', { skip: process.platform === 'win32' }, async (t) => {
    const { store } = world();
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bench-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const target = path.join(dir, 'elsewhere.txt');
    fs.writeFileSync(target, 'kept\n', { mode: 0o644 });
    const link = path.join(dir, 'tokens.txt');
    fs.symlinkSync(target, link);
    const refused = await run(store, ['bench-accounts', '--count', '2', '--out', link, '--i-know-this-is-a-test-server']);
    assert.equal(refused.code, 1);
    assert.match(refused.err, /is a symbolic link/);
    assert.equal(store.users.byUsername('bench0001'), null);
    assert.equal(fs.readFileSync(target, 'utf8'), 'kept\n');
    assert.equal(fs.statSync(target).mode & 0o777, 0o644);
    // A dangling link (the tokens would create the file it names) is refused the same way.
    fs.rmSync(target);
    const dangling = await run(store, ['bench-accounts', '--count', '2', '--out', link, '--i-know-this-is-a-test-server']);
    assert.equal(dangling.code, 1);
    assert.match(dangling.err, /is a symbolic link/);
    assert.equal(fs.existsSync(target), false);
});

test('backup refuses a source that is not a Scacelith database, and creates nothing', async (t) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-backup-src-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const store = createFakeStore();
    const target = path.join(dir, 'copy.db');
    const backup = (dbPath) => run(store, ['backup', target, '--verify'], { config: testConfig({ DB_PATH: dbPath }) });
    // A DB_PATH that does not exist (a wrong DATA_DIR, or a relative one run from another
    // directory): no empty database created there, no empty backup written.
    const missing = path.join(dir, 'data', 'scacelith.db');
    const r1 = await backup(missing);
    assert.equal(r1.code, 1);
    assert.match(r1.err, /no Scacelith database at .*scacelith\.db \(no such file\)/);
    assert.equal(fs.existsSync(path.join(dir, 'data')), false);
    assert.equal(fs.existsSync(target), false);
    // An SQLite database without the server's schema.
    const other = path.join(dir, 'other.db');
    const db = new DatabaseSync(other);
    db.exec('CREATE TABLE notes (x TEXT)');
    db.close();
    const r2 = await backup(other);
    assert.equal(r2.code, 1);
    assert.match(r2.err, /no Scacelith database at .*other\.db \(no schema_migrations\)/);
    assert.equal(fs.existsSync(target), false);
    // A file that is not a database at all.
    const text = path.join(dir, 'notes.txt');
    fs.writeFileSync(text, 'not a database\n'.repeat(500));
    const r3 = await backup(text);
    assert.equal(r3.code, 1);
    assert.match(r3.err, /no Scacelith database at .*notes\.txt/);
    assert.equal(fs.existsSync(target), false);
    assert.equal(fs.readFileSync(text, 'utf8'), 'not a database\n'.repeat(500), 'the source is left alone');
});

test('bin/admin.js refuses to run on a missing database instead of creating an empty one', (t) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-admin-cwd-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const bin = fileURLToPath(new URL('../../bin/admin.js', import.meta.url));
    const env = { PATH: process.env.PATH, SERVER_SECRET: Buffer.alloc(48, 9).toString('base64'), TLS_MODE: 'proxy' };
    for (const argv of [['stats'], ['backup', path.join(dir, 'copy.db'), '--verify']]) {
        const r = spawnSync(process.execPath, [bin, ...argv], { cwd: dir, env, encoding: 'utf8', timeout: 30000 });
        assert.equal(r.status, 1, r.stderr);
        assert.match(r.stderr, /no Scacelith database at .*data\/scacelith\.db/);
    }
    assert.deepEqual(fs.readdirSync(dir), [], 'no data directory, no database, no backup');
});

test('bin/admin.js does not print the node:sqlite ExperimentalWarning the store filters', () => {
    const bin = fileURLToPath(new URL('../../bin/admin.js', import.meta.url));
    const r = spawnSync(process.execPath, [bin, '--help'], { env: { PATH: process.env.PATH }, encoding: 'utf8', timeout: 30000 });
    assert.equal(r.status, 0);
    assert.match(r.stdout, /Usage/);
    assert.equal(r.stderr, '');
});

test('backup: consistent copy with VACUUM INTO, mode 600, never overwrites', async (t) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-backup-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const config = testConfig({ DB_PATH: path.join(dir, 'scacelith.db') });
    const store = openStore(config);
    migrate(store);
    const id = store.users.create({ username: 'Ann', email: 'ann@example.org' });
    const target = path.join(dir, 'copy.db');
    try {
        const r = await run(store, ['backup', target, '--verify', '--json'], { config });
        assert.equal(r.code, 0, r.err);
        assert.equal(r.json.verified, true);
        assert.equal(fs.statSync(target).mode & 0o777, 0o600);
        const b = new DatabaseSync(target, { readOnly: true });
        try { assert.equal(b.prepare('SELECT username FROM users WHERE id = ?').get(id).username, 'Ann'); } finally { b.close(); }
        const again = await run(store, ['backup', target], { config });
        assert.equal(again.code, 1);
        assert.match(again.err, /exists/);
        assert.equal((await run(store, ['backup'], { config })).code, 1, 'a target file is required');
    } finally {
        store.close();
    }
});
