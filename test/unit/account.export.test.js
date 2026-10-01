// POST /api/v1/account/export (http/routes/account-export.js): the document, its re-authentication
// and rate limit, and above all what it must never hold. The secrets test runs the real auth
// module and the real SQLite store (a file, so that a second connection can read every secret
// column) and searches the document's text for each of them.

import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { DatabaseSync } from 'node:sqlite';
import { refundVictims } from '../../src/anticheat/refunds.js';
import { base32Decode, totp } from '../../src/security/totp.js';
import { enums } from '../../src/protocol/schema.js';
import { DETAIL_FIELDS, EXPORT_FORMAT, IP_KINDS, exportedEvent, exportFileName, refundsPerDay } from '../../src/http/routes/account-export.js';
import { linkIn, startTestServer } from './helpers/auth-fakes.js';
import { startReal } from './helpers/real-auth.js';

const { GameStatus, EndReason } = enums;
const PW = 'correct horse battery';
const EXPORT = '/api/v1/account/export';

// ---- the fake harness: answer, re-authentication, rate limit -------------------------------------

test('the document: format, account, sections, Content-Disposition; account_exported recorded', async (t) => {
    const s = await startTestServer({ env: { SERVER_NAME: 'Club', SERVER_PUBLIC_HOST: 'chess.example.org' } });
    t.after(s.close);
    const u = await s.createUser({ username: 'alice', password: PW });
    const bob = await s.createUser({ username: 'bob', password: PW });
    const { token } = await s.login('alice', PW, { clientLabel: 'Scacelith 1.4 (Windows)' });
    s.store.ratings._set(u.id, [{ category: '3+2', rating: 1612, games: 42, wins: 20, draws: 5, losses: 17, peak: 1650, provisional: false,
        rated: true, countedGames: 42, updatedAt: 1234 }]);
    const g = (id, whiteId, blackId, status, extra = {}) => ({ id, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId, blackId,
        whiteName: whiteId === u.id ? 'alice' : 'bob', blackName: blackId === u.id ? 'alice' : 'bob', whiteRating: 1500, blackRating: 1500,
        whiteBefore: 1500, whiteAfter: 1510, blackBefore: 1500, blackAfter: 1490, status, reason: EndReason.Resignation, plies: 20,
        startedAt: s.now() - 3600000, endedAt: s.now() - 1800000, ...extra });
    s.store._raw.games.push(g(11, u.id, bob.id, GameStatus.WhiteWins), g(12, bob.id, u.id, GameStatus.WhiteWins), g(13, u.id, bob.id, GameStatus.Draw));
    s.store._raw.conduct.push({ userId: u.id, kind: 'abort', at: s.now() - 5000 });
    s.store._raw.reports.push({ id: 1, reporterId: u.id, reportedId: bob.id, gameId: 12, category: 'cheating', comment: 'too strong', createdAt: s.now() - 100, status: 'open' });
    s.store._raw.refunds.push({ id: 1, gameId: 12, victimId: u.id, cheaterId: bob.id, category: '3+2', points: 9, createdAt: s.now() - 50, createdBy: 'mod_a', cheaterName: 'bob' });
    s.auth.events.flush();

    const r = await s.request('POST', EXPORT, { token, body: { password: PW } });
    assert.equal(r.status, 200);
    assert.match(r.headers['content-type'], /^application\/json/);
    assert.equal(r.headers['content-disposition'], 'attachment; filename="scacelith-account-alice.json"');
    assert.equal(r.headers['cache-control'], 'no-store');
    const d = r.json;
    assert.equal(d.format, EXPORT_FORMAT);
    assert.equal(d.version, 1);
    assert.equal(d.exportedAt, s.now());
    assert.deepEqual(d.server, { name: 'Club', host: 'chess.example.org' });
    assert.ok(Array.isArray(d.notes) && d.notes.length >= 4 && d.notes.every((n) => typeof n === 'string'));
    assert.ok(d.notes.some((n) => n.includes('password') && n.includes('recovery codes')));
    assert.ok(d.notes.some((n) => n.includes('anti-cheat') && n.includes('reports other players made about you')));
    assert.deepEqual(d.account, {
        id: u.id, username: 'alice', email: 'alice@example.com', emailVerified: true, pendingEmail: null, mfaEnabled: false, googleLinked: false,
        googleEmail: null, hasPassword: true, acceptChallenges: 'all', createdAt: s.now(), lastLoginAt: s.now(),
    });
    assert.deepEqual(d.ratings, [{ category: '3+2', rating: 1612, games: 42, wins: 20, draws: 5, losses: 17, peak: 1650, provisional: false,
        rated: true, countedGames: 42, updatedAt: 1234 }]);
    assert.deepEqual(d.ratingRefunds, [{ day: Math.floor((s.now() - 50) / 86400000) * 86400000, category: '3+2', points: 9 }]);
    assert.equal(d.sessions.length, 1);
    assert.deepEqual(Object.keys(d.sessions[0]).sort(), ['clientLabel', 'createdAt', 'expiresAt', 'id', 'ip', 'lastSeenAt', 'revokedAt']);
    assert.equal(d.sessions[0].clientLabel, 'Scacelith 1.4 (Windows)');
    assert.ok(d.securityEvents.some((e) => e.kind === 'login' && e.detail && e.detail.method === 'password'));
    assert.deepEqual(d.conduct, [{ kind: 'abort', at: s.now() - 5000 }]);
    assert.deepEqual(d.reportsFiled, [{ gameId: 12, reported: 'bob', category: 'cheating', comment: 'too strong', createdAt: s.now() - 100, status: 'open' }]);
    assert.equal(d.games.total, 3);
    assert.deepEqual(d.games.list.map((x) => [x.id, x.outcome, x.color]), [[13, 'draw', 'white'], [12, 'loss', 'black'], [11, 'win', 'white']]);
    assert.equal(d.games.list[0].baseMs, 180000);

    s.auth.events.flush();
    const ev = s.store._raw.securityEvents.filter((e) => e.kind === 'account_exported');
    assert.equal(ev.length, 1);
    assert.equal(ev[0].userId, u.id);
});

test('re-authentication like deletion; 401 without a session', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'alice', password: PW });
    const { token } = await s.login('alice', PW);
    let r = await s.request('POST', EXPORT, { token, body: { password: 'nope nope nope' } });
    assert.deepEqual([r.status, r.json.error], [403, 'invalid_password']);
    r = await s.request('POST', EXPORT, { token, body: {} });
    assert.deepEqual([r.status, r.json.error], [400, 'invalid_request']);
    r = await s.request('POST', EXPORT, { body: { password: PW } });
    assert.equal(r.status, 401);

    const setup = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } });
    const secret = base32Decode(setup.json.secret);
    const en = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: totp(secret, s.now()) } });
    s.now.advance(30000);
    r = await s.request('POST', EXPORT, { token, body: { password: PW } });
    assert.deepEqual([r.status, r.json.error], [403, 'mfa_code_required']);
    r = await s.request('POST', EXPORT, { token, body: { password: PW, code: '000000' } });
    assert.deepEqual([r.status, r.json.error], [403, 'invalid_code']);
    s.now.advance(3600000);       // the five exports of the hour (account_export) are used by now
    r = await s.request('POST', EXPORT, { token, body: { password: PW, code: totp(secret, s.now()) } });
    assert.equal(r.status, 200);
    assert.equal(r.json.account.mfaEnabled, true);
    r = await s.request('POST', EXPORT, { token, body: { password: PW, recoveryCode: en.json.recoveryCodes[3] } });
    assert.equal(r.status, 200);
});

test('rate: 5 exports per hour and player', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'alice', password: PW });
    await s.createUser({ username: 'bob', password: PW });
    const a = await s.login('alice', PW);
    const b = await s.login('bob', PW);
    for (let i = 0; i < 5; i++) assert.equal((await s.request('POST', EXPORT, { token: a.token, body: { password: PW } })).status, 200);
    s.now.advance(60000);
    const r = await s.request('POST', EXPORT, { token: a.token, body: { password: PW } });
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited']);
    assert.ok(Number(r.headers['retry-after']) > 0);
    assert.equal((await s.request('POST', EXPORT, { token: b.token, body: { password: PW } })).status, 200, 'per player');
    s.now.advance(13 * 60000);
    assert.equal((await s.request('POST', EXPORT, { token: a.token, body: { password: PW } })).status, 200, 'one more after 12 minutes');
});

test('a long history is exported whole, newest first, in pages', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const u = await s.createUser({ username: 'alice', password: PW });
    const bob = await s.createUser({ username: 'bob', password: PW });
    const { token } = await s.login('alice', PW);
    const N = 1203;
    let calls = 0;
    for (let i = 1; i <= N; i++) {
        const white = i % 2 ? u.id : bob.id;
        s.store._raw.games.push({ id: i, category: i % 3 ? '3+2' : 'custom', rated: i % 3 !== 0, baseMs: 180000, incMs: 2000, whiteId: white,
            blackId: white === u.id ? bob.id : u.id, whiteName: white === u.id ? 'alice' : 'bob', blackName: white === u.id ? 'bob' : 'alice',
            status: i % 5 ? GameStatus.WhiteWins : GameStatus.Aborted, reason: EndReason.Resignation, plies: 10, startedAt: i, endedAt: i + 1 });
    }
    const list = s.store.games.listForUser;
    s.store.games.listForUser = (...a) => { calls++; return list(...a); };
    const r = await s.request('POST', EXPORT, { token, body: { password: PW } });
    assert.equal(r.status, 200);
    assert.equal(r.json.games.total, N);
    assert.equal(r.json.games.list.length, N);
    assert.deepEqual(r.json.games.list.slice(0, 3).map((g) => g.id), [1203, 1202, 1201]);
    assert.equal(r.json.games.list.at(-1).id, 1);
    assert.equal(new Set(r.json.games.list.map((g) => g.id)).size, N);
    assert.equal(calls, 3, 'pages of 500');
    assert.ok(r.json.games.list.some((g) => g.outcome === 'aborted'));
});

test('security event details: only the listed fields; moderator actions reduced or left out', () => {
    assert.deepEqual(exportedEvent({ kind: 'login', at: 1, ip: '192.0.2.1', detail: { method: 'password', extra: 'x' } }),
        { kind: 'login', at: 1, ip: '192.0.2.1', detail: { method: 'password' } });
    assert.deepEqual(exportedEvent({ kind: 'some_future_kind', at: 1, detail: { secret: 'x' } }), { kind: 'some_future_kind', at: 1, ip: null, detail: null });
    assert.deepEqual(exportedEvent({ kind: 'login', at: 1, detail: '{"method":"google","x":1}' }).detail, { method: 'google' }, 'detail stored as JSON text');
    assert.equal(exportedEvent({ kind: 'login', at: 1, detail: 'not json' }).detail, null);
    assert.deepEqual(exportedEvent({ kind: 'moderator_action', at: 2, detail: { action: 'ban', moderator: 'mod_x', reason: 'r', hours: 24 } }),
        { kind: 'moderator_action', at: 2, ip: null, detail: { action: 'ban' } });
    assert.equal(exportedEvent({ kind: 'moderator_action', at: 2, detail: { action: 'integrity_confirm', previousLevel: 'suspected', score: 0.8 } }), null);
    assert.equal(exportedEvent({ kind: 'moderator_action', at: 2, detail: null }), null);
    // A refund's event would name its game (and so the cheater): ratingRefunds has its points.
    assert.equal(exportedEvent({ kind: 'rating_refund', at: 3, detail: { refundId: 1, gameId: 9, cheaterId: 4, category: '3+2', points: 7, by: 'mod' } }), null);
    assert.equal(DETAIL_FIELDS.rating_refund, undefined);
    // The IP only for what the account holder did (IP_KINDS); a future kind has none.
    assert.equal(exportedEvent({ kind: 'login_failed', at: 4, ip: '198.51.100.7', detail: { failures: 2 } }).ip, null);
    assert.equal(exportedEvent({ kind: 'password_reset_requested', at: 4, ip: '198.51.100.7' }).ip, null);
    assert.equal(exportedEvent({ kind: 'some_future_kind', at: 4, ip: '198.51.100.7' }).ip, null);
    assert.equal(exportedEvent({ kind: 'password_changed', at: 4, ip: '192.0.2.1' }).ip, '192.0.2.1');
    assert.ok(IP_KINDS.includes('login') && !IP_KINDS.includes('login_failed') && !IP_KINDS.includes('register_existing_email'));
    assert.deepEqual(refundsPerDay([
        { category: '3+2', points: 4, createdAt: 86400000 * 3 + 5 }, { category: '3+2', points: 6, createdAt: 86400000 * 4 - 1 },
        { category: '1+0', points: 2, createdAt: 86400000 * 3 }, { category: '3+2', points: 1, createdAt: 86400000 * 9 },
    ]), [{ day: 86400000 * 9, category: '3+2', points: 1 }, { day: 86400000 * 3, category: '1+0', points: 2 }, { day: 86400000 * 3, category: '3+2', points: 10 }]);
    assert.equal(exportFileName('Al_ice-9'), 'scacelith-account-Al_ice-9.json');
    assert.equal(exportFileName('a"b/c'), 'scacelith-account-a_b_c.json');
});

test('IP addresses: those of the account holder only; the IP of someone who typed the address or name is left out', async (t) => {
    const s = await startTestServer({ env: { AUTH_FAILURES_PER_ACCOUNT: '3' } });
    t.after(s.close);
    const u = await s.createUser({ username: 'alice', email: 'alice@example.com', password: PW });
    const BOB = '198.51.100.77', ALICE = '203.0.113.7';
    // Bob uses Alice's address to register, asks for a reset of her password, guesses it, and
    // gets her password right but not her second factor.
    let r = await s.request('POST', '/api/v1/auth/register', { ip: BOB, body: { username: 'bobby', email: 'alice@example.com', password: 'another fine passphrase' } });
    assert.equal(r.status, 202);
    r = await s.request('POST', '/api/v1/auth/password/forgot', { ip: BOB, body: { email: 'alice@example.com' } });
    assert.equal(r.status, 202);
    for (let i = 0; i < 3; i++) {
        r = await s.request('POST', '/api/v1/auth/login', { ip: BOB, body: { login: 'alice', password: 'not her password' } });
        assert.equal(r.status, 401);
    }
    s.now.advance(3600000);
    const signIn = await s.request('POST', '/api/v1/auth/login', { ip: ALICE, body: { login: 'alice', password: PW } });
    assert.equal(signIn.status, 200);
    const token = signIn.json.token;
    const setup = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, ip: ALICE, body: { password: PW } });
    const secret = base32Decode(setup.json.secret);
    assert.equal((await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, ip: ALICE, body: { code: totp(secret, s.now()) } })).status, 200);
    r = await s.request('POST', '/api/v1/auth/login', { ip: BOB, body: { login: 'alice', password: PW } });
    assert.ok(r.json.mfaToken);
    assert.equal((await s.request('POST', '/api/v1/auth/login/mfa', { ip: BOB, body: { mfaToken: r.json.mfaToken, code: '000000' } })).status, 401);
    s.now.advance(30000);
    s.auth.events.flush();
    const stored = s.store._raw.securityEvents.filter((e) => e.userId === u.id && e.ip === BOB).map((e) => e.kind).sort();
    assert.deepEqual([...new Set(stored)], ['login_failed', 'login_lockout', 'mfa_failed', 'password_reset_requested', 'register_existing_email']);

    r = await s.request('POST', EXPORT, { token, ip: ALICE, body: { password: PW, code: totp(secret, s.now()) } });
    assert.equal(r.status, 200);
    assert.ok(!r.text.includes(BOB), 'no IP address of another person');
    const ev = r.json.securityEvents;
    for (const kind of ['register_existing_email', 'password_reset_requested', 'login_failed', 'login_lockout', 'mfa_failed']) {
        const e = ev.find((x) => x.kind === kind);
        assert.ok(e, kind);
        assert.equal(e.ip, null, kind);
    }
    assert.deepEqual(ev.find((x) => x.kind === 'login_failed').detail, { failures: 3 }, 'the rest of the event stays');
    for (const kind of ['login', 'mfa_setup_started', 'mfa_enabled']) assert.equal(ev.find((x) => x.kind === kind).ip, ALICE, kind);
    assert.ok(r.json.sessions.every((x) => x.ip === ALICE));
    assert.ok(r.json.notes.some((n) => n.includes('IP address')));
});

// ---- the real store: no secret, no anti-cheat data, no moderator identity -------------------------

test('an export never holds a secret, the anti-cheat\'s data, reports against the player or a moderator\'s identity', async (t) => {
    const s = await startReal(t);
    const { store } = s;
    const mk = async (username, email, password = PW) => store.users.create({ username, email, passwordHash: await s.hasher.hash(password), emailVerified: true });
    const uid = await mk('Alice', 'alice@example.org');
    const cheater = await mk('Mallory', 'mallory@example.org');
    const rita = await mk('Rita_Reporter', 'rita@example.org');
    const victim = await mk('Victor', 'victor@example.org');

    // Sessions: three logins, one of them signed out.
    const login = async (label) => {
        const r = await s.request('POST', '/api/v1/auth/login', { body: { login: 'Alice', password: PW, clientLabel: label } });
        assert.equal(r.status, 200, r.text);
        return r.json.token;
    };
    const token = await login('desk');
    const laptop = await login('laptop');
    const phone = await login('phone');
    assert.equal((await s.request('POST', '/api/v1/auth/logout', { token: phone })).status, 200);

    // MFA with its secret and recovery codes.
    const setup = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } });
    const secretB32 = setup.json.secret;
    const en = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: totp(base32Decode(secretB32), s.now()) } });
    assert.equal(en.status, 200);
    const recoveryCodes = en.json.recoveryCodes;
    s.now.advance(30000);

    // A password reset link and a pending e-mail change link, both live.
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.org' } });
    await s.mailer.idle();
    const resetToken = new URL(linkIn(s.mailer.sent.find((m) => m.subject.includes('Reset')).text)).searchParams.get('token');
    let r = await s.request('POST', '/api/v1/account/email', { token, body: { newEmail: 'alice.new@example.org', password: PW, recoveryCode: recoveryCodes[0] } });
    assert.equal(r.status, 202);
    await s.mailer.idle();
    const changeToken = new URL(linkIn(s.mailer.sent.find((m) => m.to === 'alice.new@example.org').text)).searchParams.get('token');

    // Games: Alice loses a rated game to the cheater, then the cheater's victims are refunded by a moderator.
    let gid = 7_000_000_000_000;
    const game = (white, black, status, extra = {}) => ({
        id: ++gid, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: white.id, blackId: black.id, whiteName: white.name,
        blackName: black.name, whiteRating: 1500, blackRating: 1500, startedAt: Date.now() - 7200000, endedAt: Date.now() - 3600000, status,
        reason: EndReason.Resignation, moves: Uint16Array.from([12 | (28 << 6), 52 | (36 << 6)]), spentMs: Uint32Array.from([0, 0]),
        clockMs: Uint32Array.from([180000, 180000]), rematchOf: 0, flags: 0, ...extra,
    });
    const A = { id: uid, name: 'Alice' }, M = { id: cheater, name: 'Mallory' }, V = { id: victim, name: 'Victor' };
    const lost = game(M, A, GameStatus.WhiteWins);
    store.games.finishBatch([lost, game(A, V, GameStatus.Draw), game(V, M, GameStatus.BlackWins)]);
    const sanctionId = store.sanctions.create({ userId: cheater, kind: 'ban', reason: 'confirmed: engine', createdBy: 'mod_banhammer' });
    refundVictims(store, { cheaterId: cheater, since: 0, now: Date.now(), sanctionId, source: 'moderator', by: 'mod_refunder' });

    // Sanctions of Alice (one lifted), with moderator identities.
    store.sanctions.create({ userId: uid, kind: 'mm_block', reason: 'abandons', createdBy: 'mod_morgana', startsAt: Date.now() - 1000, endsAt: Date.now() + 3600000 });
    const lifted = store.sanctions.create({ userId: uid, kind: 'warning', reason: 'chat', createdBy: 'mod_morgana' });
    store.sanctions.lift(lifted, 'mod_lifterson');
    store.conduct.record(uid, 'abandon', Date.now() - 5000);

    // Security events written by moderators and the anti-cheat about Alice.
    store.security.insertBatch([
        { kind: 'moderator_action', userId: uid, ip: null, at: Date.now() - 4000, detail: { action: 'reset_mfa', moderator: 'mod_morgana', revokedSessions: 2 } },
        { kind: 'moderator_action', userId: uid, ip: null, at: Date.now() - 3000, detail: { action: 'integrity_clear', moderator: 'mod_morgana', reason: 'INTEGRITY-CLEAR-REASON', previousLevel: 'high_confidence', score: 0.914273, reportsDismissed: 3 } },
        { kind: 'sanction_auto', userId: uid, ip: null, at: Date.now() - 2000, detail: { kind: 'mm_block', gameId: lost.id, until: Date.now() + 1000, signal: 'AUTO-SIGNAL-DETAIL' } },
    ]);

    // Reports: one filed by Alice, one received.
    store.reports.create({ reporterId: uid, reportedId: cheater, gameId: lost.id, category: 'cheating', comment: 'engine moves', weight: 0.7317 });
    store.reports.create({ reporterId: rita, reportedId: uid, gameId: lost.id, category: 'other', comment: 'RECEIVED-REPORT-COMMENT-42', weight: 0.6193 });

    // The anti-cheat's own data about Alice.
    store.integrity.set(uid, { level: 'high_confidence', score: 0.873311, evidence: { why: 'EVIDENCE-TEXT-77' }, note: 'MODNOTE-55', reviewedBy: 'mod_reviewer' });
    store.anomalies.insertBatch([{ userId: uid, gameId: lost.id, kind: 'move_time_uniformity_TEST', severity: 'suspicious', at: Date.now(), detail: { z: 'ANOMALY-DETAIL-31' } }]);
    store.integrity.updatePopulation('3+2|1500|POPMETRIC_TEST', [1, 2, 3]);
    s.auth.events.flush();

    // The export.
    r = await s.request('POST', EXPORT, { token: laptop, body: { password: PW, code: totp(base32Decode(secretB32), s.now()) } });
    assert.equal(r.status, 200, r.text);
    const text = r.text;
    const d = r.json;

    // What it holds.
    assert.equal(d.account.username, 'Alice');
    assert.equal(d.account.mfaEnabled, true);
    assert.equal(d.account.pendingEmail, 'alice.new@example.org');
    assert.equal(d.sessions.length, 3);
    assert.equal(d.sessions.filter((x) => x.revokedAt).length, 1);
    assert.deepEqual(d.sessions.map((x) => x.clientLabel).sort(), ['desk', 'laptop', 'phone']);
    assert.ok(d.sessions.every((x) => x.ip === '127.0.0.1'));
    assert.equal(d.games.total, 2);
    assert.deepEqual(d.games.list.map((x) => x.outcome).sort(), ['draw', 'loss']);
    assert.equal(d.ratingRefunds.length, 1);
    assert.deepEqual(Object.keys(d.ratingRefunds[0]).sort(), ['category', 'day', 'points']);
    assert.equal(d.ratingRefunds[0].points, 10);
    assert.equal(d.sanctions.length, 2);
    assert.ok(d.sanctions.some((x) => x.liftedAt));
    assert.ok(d.sanctions.every((x) => !('createdBy' in x) && !('liftedBy' in x)));
    assert.deepEqual(d.conduct.map((x) => x.kind), ['abandon']);
    assert.deepEqual(d.reportsFiled, [{ gameId: lost.id, reported: 'Mallory', category: 'cheating', comment: 'engine moves', createdAt: d.reportsFiled[0].createdAt, status: 'open' }]);
    const kinds = d.securityEvents.map((e) => e.kind);
    for (const k of ['login', 'mfa_enabled', 'recovery_code_used', 'email_change_requested', 'sanction_auto', 'moderator_action']) {
        assert.ok(kinds.includes(k), `security event ${k}`);
    }
    assert.ok(!kinds.includes('rating_refund'), 'a refund is in ratingRefunds, without its game');
    assert.deepEqual(d.securityEvents.filter((e) => e.kind === 'moderator_action').map((e) => e.detail), [{ action: 'reset_mfa' }]);
    assert.deepEqual(d.securityEvents.find((e) => e.kind === 'login').detail, { method: 'password' }, 'the auth events\' JSON text detail');
    assert.deepEqual(d.securityEvents.find((e) => e.kind === 'recovery_code_used').detail, { remaining: 9 });
    assert.deepEqual(Object.keys(d.securityEvents.find((e) => e.kind === 'sanction_auto').detail).sort(), ['gameId', 'kind', 'until']);

    // What it must never hold: every secret column of the database, in every encoding...
    const raw = new DatabaseSync(s.file, { readOnly: true });
    t.after(() => raw.close());
    const forbidden = [];
    const add = (label, v) => {
        if (v === null || v === undefined) return;
        if (Buffer.isBuffer(v) || v instanceof Uint8Array) {
            const b = Buffer.from(v);
            forbidden.push([label + ' (hex)', b.toString('hex')], [label + ' (base64)', b.toString('base64')], [label + ' (base64url)', b.toString('base64url')]);
        } else forbidden.push([label, String(v)]);
    };
    const user = raw.prepare('SELECT password_hash, mfa_secret_enc, mfa_pending_secret_enc FROM users WHERE id = ?').get(uid);
    assert.ok(user.password_hash && user.mfa_secret_enc, 'the account has a password hash and an MFA secret');
    add('password hash', user.password_hash);
    add('MFA secret (encrypted)', user.mfa_secret_enc);
    add('MFA pending secret', user.mfa_pending_secret_enc);
    const codes = raw.prepare('SELECT code_hash FROM mfa_recovery_codes WHERE user_id = ?').all(uid);
    assert.equal(codes.length, 9);
    for (const c of codes) add('recovery code hash', c.code_hash);
    const sessions = raw.prepare('SELECT token_hash FROM sessions WHERE user_id = ?').all(uid);
    assert.equal(sessions.length, 3);
    for (const x of sessions) add('session token hash', x.token_hash);
    const tokens = raw.prepare('SELECT kind, token_hash FROM tokens WHERE user_id = ?').all(uid);
    assert.deepEqual(tokens.map((x) => x.kind).sort(), ['email_change', 'password_reset']);
    for (const x of tokens) add(`${x.kind} token hash`, x.token_hash);
    // ...the secrets the player was given...
    for (const tok of [token, laptop, phone]) add('session token', tok);
    add('reset token', resetToken);
    add('e-mail change token', changeToken);
    add('reset token hash', crypto.createHash('sha256').update(resetToken).digest('hex'));
    add('MFA secret (base32)', secretB32);
    add('MFA secret (hex)', base32Decode(secretB32).toString('hex'));
    for (const c of recoveryCodes) add('recovery code', c);
    add('password', PW);
    // ...the anti-cheat's data and the reports against Alice...
    const integrity = raw.prepare('SELECT level, score, evidence, note, reviewed_by FROM player_integrity WHERE user_id = ?').get(uid);
    assert.equal(integrity.level, 'high_confidence');
    for (const v of ['high_confidence', '0.873311', 'EVIDENCE-TEXT-77', 'MODNOTE-55', 'mod_reviewer', 'integrity_clear', 'INTEGRITY-CLEAR-REASON', '0.914273',
        'move_time_uniformity_TEST', 'ANOMALY-DETAIL-31', 'suspicious', 'POPMETRIC_TEST', 'RECEIVED-REPORT-COMMENT-42', 'Rita_Reporter', '0.6193', '0.7317',
        'AUTO-SIGNAL-DETAIL', 'reportsDismissed', 'previousLevel', 'cheaterId', 'refundId']) add('anti-cheat / report', v);
    // ...and the moderators.
    for (const v of ['mod_morgana', 'mod_lifterson', 'mod_refunder', 'mod_banhammer']) add('moderator', v);
    // And no field that would carry one (as JSON keys: the notes name some of these things in words).
    for (const v of ['passwordHash', 'password_hash', 'tokenHash', 'token_hash', 'mfaSecret', 'secretEnc', 'recoveryCodes', 'codeHash', 'integrity',
        'level', 'score', 'evidence', 'weight', 'anomalies', 'features', 'createdBy', 'liftedBy', 'resolvedBy', 'reviewedBy', 'moderator', 'by']) {
        add('field', `"${v}":`);
    }

    assert.ok(forbidden.length > 40);
    const found = forbidden.filter(([, v]) => v.length >= 4 && text.includes(v));
    assert.deepEqual(found, [], 'forbidden data in the export');

    // The pieces it leaves out are really in the database (the search above is not vacuous).
    assert.equal(raw.prepare('SELECT count(*) AS n FROM anomalies WHERE user_id = ?').get(uid).n, 1);
    assert.equal(raw.prepare('SELECT count(*) AS n FROM reports WHERE reported_id = ?').get(uid).n, 1);
    assert.ok(raw.prepare("SELECT count(*) AS n FROM security_events WHERE user_id = ? AND kind = 'moderator_action'").get(uid).n === 2);
});

test('rating refunds and the outcomes of reports never tell which opponent was sanctioned', async (t) => {
    const s = await startReal(t);
    const { store } = s;
    const mk = async (username) => store.users.create({ username, email: `${username.toLowerCase()}@example.org`, passwordHash: await s.hasher.hash(PW), emailVerified: true });
    const uid = await mk('Alice'), mallory = await mk('Mallory'), victor = await mk('Victor');
    const login = await s.request('POST', '/api/v1/auth/login', { body: { login: 'Alice', password: PW } });
    assert.equal(login.status, 200, login.text);
    let gid = 7_100_000_000_000;
    const game = (white, black, status) => ({
        id: ++gid, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: white.id, blackId: black.id, whiteName: white.name,
        blackName: black.name, whiteRating: 1500, blackRating: 1500, startedAt: Date.now() - 7200000, endedAt: Date.now() - 3600000, status,
        reason: EndReason.Resignation, moves: Uint16Array.from([12 | (28 << 6), 52 | (36 << 6)]), spentMs: Uint32Array.from([0, 0]),
        clockMs: Uint32Array.from([180000, 180000]), rematchOf: 0, flags: 0,
    });
    const A = { id: uid, name: 'Alice' }, M = { id: mallory, name: 'Mallory' }, V = { id: victor, name: 'Victor' };
    const lostToM = game(M, A, GameStatus.WhiteWins), lostToV = game(V, A, GameStatus.WhiteWins), lostToM2 = game(A, M, GameStatus.BlackWins);
    store.games.finishBatch([lostToM, lostToV, lostToM2]);
    // Mallory is banned for cheating: Alice gets back the points of her two games against Mallory.
    const at = Date.now();
    const sanctionId = store.sanctions.create({ userId: mallory, kind: 'ban', reason: 'confirmed: engine', createdBy: 'mod_x' });
    const given = refundVictims(store, { cheaterId: mallory, since: 0, now: at, sanctionId, source: 'moderator', by: 'mod_x' });
    assert.deepEqual(given.filter((g) => g.victimId === uid).map((g) => g.gameId).sort(), [lostToM.id, lostToM2.id]);
    // Alice's reports: the one on Mallory was actioned, the one on Victor dismissed.
    store.reports.resolve(store.reports.create({ reporterId: uid, reportedId: mallory, gameId: lostToM.id, category: 'cheating', comment: 'engine', weight: 0.5 }), 'actioned', 'mod_x');
    store.reports.resolve(store.reports.create({ reporterId: uid, reportedId: victor, gameId: lostToV.id, category: 'cheating', comment: 'fast', weight: 0.5 }), 'dismissed', 'mod_x');
    s.auth.events.flush();

    const r = await s.request('POST', EXPORT, { token: login.json.token, body: { password: PW } });
    assert.equal(r.status, 200, r.text);
    const d = r.json;
    assert.equal(d.games.total, 3);
    // The points given back, added up per UTC day and category (as the game's notice gives them).
    assert.deepEqual(d.ratingRefunds, [{ day: at - (at % 86400000), category: '3+2', points: 20 }]);
    // No refund field, and nothing outside the game list and Alice's own reports, names a game.
    const ids = [lostToM.id, lostToV.id, lostToM2.id].map(String);
    const rest = JSON.stringify({ ...d, games: null, reportsFiled: null });
    assert.deepEqual(ids.filter((id) => rest.includes(id)), [], 'no refunded game in the refunds or the events');
    assert.ok(!d.securityEvents.some((e) => e.kind === 'rating_refund'), 'refund events are in ratingRefunds');
    // A filed report is open or closed; whether the reported player was sanctioned is not said.
    assert.deepEqual(d.reportsFiled.map((x) => [x.reported, x.status]).sort(), [['Mallory', 'closed'], ['Victor', 'closed']]);
    assert.ok(!r.text.includes('actioned') && !r.text.includes('dismissed'));
});
