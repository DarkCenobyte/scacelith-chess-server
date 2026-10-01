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
import { DETAIL_FIELDS, EXPORT_FORMAT, exportedEvent, exportFileName } from '../../src/http/routes/account-export.js';
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
    assert.deepEqual(d.ratingRefunds, [{ gameId: 12, category: '3+2', points: 9, at: s.now() - 50 }]);
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
    assert.deepEqual(exportedEvent({ kind: 'rating_refund', at: 3, detail: { refundId: 1, gameId: 9, cheaterId: 4, category: '3+2', points: 7, by: 'mod' } }).detail,
        { gameId: 9, category: '3+2', points: 7 });
    assert.deepEqual(DETAIL_FIELDS.rating_refund, ['gameId', 'category', 'points']);
    assert.equal(exportFileName('Al_ice-9'), 'scacelith-account-Al_ice-9.json');
    assert.equal(exportFileName('a"b/c'), 'scacelith-account-a_b_c.json');
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
    assert.deepEqual(Object.keys(d.ratingRefunds[0]).sort(), ['at', 'category', 'gameId', 'points']);
    assert.equal(d.ratingRefunds[0].gameId, lost.id);
    assert.equal(d.ratingRefunds[0].points, 10);
    assert.equal(d.sanctions.length, 2);
    assert.ok(d.sanctions.some((x) => x.liftedAt));
    assert.ok(d.sanctions.every((x) => !('createdBy' in x) && !('liftedBy' in x)));
    assert.deepEqual(d.conduct.map((x) => x.kind), ['abandon']);
    assert.deepEqual(d.reportsFiled, [{ gameId: lost.id, reported: 'Mallory', category: 'cheating', comment: 'engine moves', createdAt: d.reportsFiled[0].createdAt, status: 'open' }]);
    const kinds = d.securityEvents.map((e) => e.kind);
    for (const k of ['login', 'mfa_enabled', 'recovery_code_used', 'email_change_requested', 'rating_refund', 'sanction_auto', 'moderator_action']) {
        assert.ok(kinds.includes(k), `security event ${k}`);
    }
    assert.deepEqual(d.securityEvents.filter((e) => e.kind === 'moderator_action').map((e) => e.detail), [{ action: 'reset_mfa' }]);
    assert.deepEqual(d.securityEvents.find((e) => e.kind === 'login').detail, { method: 'password' }, 'the auth events\' JSON text detail');
    assert.deepEqual(d.securityEvents.find((e) => e.kind === 'recovery_code_used').detail, { remaining: 9 });
    assert.deepEqual(d.securityEvents.find((e) => e.kind === 'rating_refund').detail, { gameId: lost.id, category: '3+2', points: 10 });
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
