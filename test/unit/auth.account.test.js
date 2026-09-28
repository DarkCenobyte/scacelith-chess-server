import test from 'node:test';
import assert from 'node:assert/strict';
import { base32Decode, totp } from '../../src/security/totp.js';
import { PROTOCOL_MIN, PROTOCOL_VERSION, SCHEMA_HASH, WS_SUBPROTOCOL } from '../../src/protocol/index.js';
import { startTestServer } from './helpers/auth-fakes.js';

const PW = 'correct horse battery';

test('GET /info', async (t) => {
    const s = await startTestServer({ env: { SERVER_NAME: 'Club', SERVER_MOTD: 'Welcome', WS_PORT: '9444', RATED_CATEGORIES: '3+2,10+0', POW_REGISTER_BITS: '16' } });
    t.after(s.close);
    const r = await s.request('GET', '/api/v1/info');
    assert.equal(r.status, 200);
    assert.deepEqual(r.json, {
        name: 'Club', serverId: 'test-server-id', motd: 'Welcome',
        protocol: { min: PROTOCOL_MIN, max: PROTOCOL_VERSION, schema: SCHEMA_HASH, subprotocol: WS_SUBPROTOCOL },
        wsPort: 9444, wsPath: '/ws', registration: 'open', emailVerification: true, sso: { google: false }, mfa: true,
        pow: { register: 16 },
        categories: [{ id: '3+2', baseSec: 180, incSec: 2 }, { id: '10+0', baseSec: 600, incSec: 0 }],
        limits: { usernameMin: 3, usernameMax: 20, usernamePattern: '^[A-Za-z0-9][A-Za-z0-9_-]*$', passwordMinLength: 10, passwordMaxBytes: 256,
            customTimeControls: true, reportsPerDay: 5, wsMaxMessageBytes: 512 },
    });
});

test('GET /account/me: account, ratings, active sanctions, never the integrity level', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const u = await s.createUser({ username: 'alice', password: PW });
    s.store.ratings._set(u.id, [
        { category: '3+2', rating: 1612, games: 42, wins: 20, draws: 5, losses: 17, peak: 1650, reachedSenior: false },
        { category: '10+0', rating: 1500, games: 3, wins: 1, draws: 1, losses: 1, peak: 1510, reachedSenior: false },
    ]);
    s.store.sanctions.create({ userId: u.id, kind: 'mm_block', reason: 'abandons', startsAt: s.now() - 1000, endsAt: s.now() + 60000 });
    s.store.sanctions.create({ userId: u.id, kind: 'warning', reason: 'old', startsAt: s.now() - 9000, endsAt: s.now() - 1000 });
    const { token } = await s.login('alice', PW);
    const r = await s.request('GET', '/api/v1/account/me', { token });
    assert.equal(r.status, 200);
    assert.equal(r.json.user.username, 'alice');
    assert.equal(r.json.user.email, 'alice@example.com');
    assert.deepEqual(r.json.ratings, [
        { category: '3+2', rating: 1612, games: 42, wins: 20, draws: 5, losses: 17, peak: 1650, provisional: false },
        { category: '10+0', rating: 1500, games: 3, wins: 1, draws: 1, losses: 1, peak: 1510, provisional: true },
    ]);
    assert.deepEqual(r.json.sanctions, [{ kind: 'mm_block', reason: 'abandons', startsAt: s.now() - 1000, endsAt: s.now() + 60000 }]);
    assert.equal(r.json.ban, null);
    assert.ok(!JSON.stringify(r.json).includes('integrity'));
    assert.ok(!JSON.stringify(r.json).includes('passwordHash'));
    assert.equal((await s.request('GET', '/api/v1/account/me')).status, 401);
});

test('preferences', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const u = await s.createUser({ username: 'alice', password: PW });
    const { token } = await s.login('alice', PW);
    let r = await s.request('PUT', '/api/v1/account/preferences', { token, body: { acceptChallenges: 'none' } });
    assert.deepEqual([r.status, r.json], [200, { preferences: { acceptChallenges: 'none' } }]);
    assert.equal(s.store.users.byId(u.id).acceptChallenges, 'none');
    r = await s.request('PUT', '/api/v1/account/preferences', { token, body: { acceptChallenges: 'friends' } });
    assert.equal(r.status, 400);
    r = await s.request('POST', '/api/v1/account/preferences', { token, body: { acceptChallenges: 'all' } });
    assert.equal(r.status, 405);
});

test('account deletion: password (+ second factor when enabled), anonymisation, sessions revoked', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const u = await s.createUser({ username: 'alice', password: PW });
    const { token } = await s.login('alice', PW);
    const other = await s.login('alice', PW);
    const setup = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } });
    const secret = base32Decode(setup.json.secret);
    await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: totp(secret, s.now()) } });
    s.now.advance(30000);
    let r = await s.request('POST', '/api/v1/account/delete', { token, body: { password: 'nope nope nope' } });
    assert.equal(r.json.error, 'invalid_password');
    r = await s.request('POST', '/api/v1/account/delete', { token, body: { password: PW } });
    assert.deepEqual([r.status, r.json.error], [403, 'mfa_code_required']);
    r = await s.request('POST', '/api/v1/account/delete', { token, body: { password: PW, code: totp(secret, s.now()) } });
    assert.deepEqual([r.status, r.json], [200, { status: 'deleted' }]);
    assert.equal(s.store._raw.users.get(u.id).status, 'deleted');
    for (const tok of [token, other.token]) assert.equal((await s.request('GET', '/api/v1/account/me', { token: tok })).status, 401);
    assert.equal((await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW } })).status, 401);
    s.auth.events.flush();
    assert.ok(s.store._raw.securityEvents.some((e) => e.kind === 'account_deleted' && e.userId === u.id));
});

test('an account without password (Google only) is told to set one first', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const u = await s.createUser({ username: 'googler', password: null });
    const created = s.auth._svc.sessions.create(s.store.users.byId(u.id), {});
    const r = await s.request('POST', '/api/v1/account/delete', { token: created.token, body: { password: 'anything goes here' } });
    assert.deepEqual([r.status, r.json.error], [400, 'password_not_set']);
    const me = await s.request('GET', '/api/v1/account/me', { token: created.token });
    assert.equal(me.json.user.hasPassword, false);
});
