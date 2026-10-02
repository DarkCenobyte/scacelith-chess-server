import test from 'node:test';
import assert from 'node:assert/strict';
import { sha256Hex } from '../../src/security/keys.js';
import { SESSION_CACHE_TTL_MS } from '../../src/auth/sessions.js';
import { createFakePrimary, linkIn, startTestServer } from './helpers/auth-fakes.js';
import { startReal } from './helpers/real-auth.js';

const PW = 'correct horse battery';
const NEW = 'a brand new passphrase';
const DAY = 86400000;

// The `session.revoked` broadcasts (auth/sessions.js header): the hashes of the revoked sessions
// close their connections, no list (null) closes the user's connection, [] closes nothing.
const revokedCalls = (primary) => primary.calls.filter((c) => c.type === 'session.revoked').map((c) => c.payload);

async function setup(env) {
    const s = await startTestServer({ env });
    const u = await s.createUser({ username: 'alice', password: PW });
    return { s, u };
}

test('validateToken: shape, format check, unknown tokens', async (t) => {
    const { s, u } = await setup();
    t.after(s.close);
    const { token } = await s.login('alice', PW);
    const v = s.auth.validateToken(token);
    assert.deepEqual(v, { userId: u.id, username: 'alice', sessionId: 1, emailVerified: true, tokenHash: sha256Hex(token) });
    for (const bad of [null, '', 'sct_short', token + 'x', token.replace('sct_', 'sxt_'), 'sct_' + '!'.repeat(43)]) assert.equal(s.auth.validateToken(bad), null);
    assert.equal(s.auth.validateToken('sct_' + 'A'.repeat(43)), null);
});

test('sessions list and revocation of another session', async (t) => {
    const { s } = await setup();
    t.after(s.close);
    const a = await s.login('alice', PW, { clientLabel: 'Laptop' });
    s.now.advance(1000);
    const b = await s.login('alice', PW, { clientLabel: 'Desktop' });
    const r = await s.request('GET', '/api/v1/auth/sessions', { token: a.token });
    assert.equal(r.status, 200);
    assert.equal(r.json.sessions.length, 2);
    const mine = r.json.sessions.find((x) => x.current);
    assert.equal(mine.clientLabel, 'Laptop');
    const other = r.json.sessions.find((x) => !x.current);
    assert.deepEqual(Object.keys(other).sort(), ['clientLabel', 'createdAt', 'current', 'expiresAt', 'id', 'lastSeenAt']);
    assert.equal((await s.request('DELETE', `/api/v1/auth/sessions/${other.id}`, { token: a.token })).status, 200);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: b.token })).status, 401);
    assert.equal((await s.request('DELETE', `/api/v1/auth/sessions/${other.id}`, { token: a.token })).status, 404);
    assert.equal((await s.request('DELETE', '/api/v1/auth/sessions/9999', { token: a.token })).status, 404);
    // Another user's session cannot be revoked.
    await s.createUser({ username: 'bob', password: PW });
    const c = await s.login('bob', PW);
    const bobs = (await s.request('GET', '/api/v1/auth/sessions', { token: c.token })).json.sessions[0];
    assert.equal((await s.request('DELETE', `/api/v1/auth/sessions/${bobs.id}`, { token: a.token })).status, 404);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: c.token })).status, 200);
});

test('logout revokes the session everywhere (broadcast through the primary)', async (t) => {
    const { s, u } = await setup();
    t.after(s.close);
    const { token } = await s.login('alice', PW);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token })).status, 200);
    const r = await s.request('POST', '/api/v1/auth/logout', { token });
    assert.deepEqual([r.status, r.json], [200, { status: 'logged_out' }]);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token })).status, 401, 'the local cache was dropped at once');
    const call = s.primary.calls.find((c) => c.type === 'session.revoked');
    assert.deepEqual(call.payload, { userId: u.id, tokenHashes: [sha256Hex(token)] });
    assert.equal((await s.request('POST', '/api/v1/auth/logout', { token })).status, 401);
});

test('revoking another session broadcasts its token hash (its connection is closed)', async (t) => {
    const { s, u } = await setup();
    t.after(s.close);
    const a = await s.login('alice', PW, { clientLabel: 'Laptop' });
    const b = await s.login('alice', PW, { clientLabel: 'Desktop' });
    const other = (await s.request('GET', '/api/v1/auth/sessions', { token: a.token })).json.sessions.find((x) => !x.current);
    assert.equal((await s.request('DELETE', `/api/v1/auth/sessions/${other.id}`, { token: a.token })).status, 200);
    assert.deepEqual(revokedCalls(s.primary), [{ userId: u.id, tokenHashes: [sha256Hex(b.token)] }]);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: b.token })).status, 401);
});

test('logout-all, a password reset and the deletion revoke every session: the broadcast has no list', async (t) => {
    const { s, u } = await setup();
    t.after(s.close);
    let { token } = await s.login('alice', PW);
    assert.equal((await s.request('POST', '/api/v1/auth/logout-all', { token })).status, 200);
    assert.deepEqual(revokedCalls(s.primary), [{ userId: u.id, tokenHashes: null }]);

    await s.login('alice', PW);
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    await s.mailer.idle();
    const reset = new URL(linkIn(s.mailer.sent.at(-1).text)).searchParams.get('token');
    assert.equal((await s.request('POST', '/api/v1/auth/password/reset', { body: { token: reset, newPassword: NEW } })).status, 200);
    assert.deepEqual(revokedCalls(s.primary).slice(1), [{ userId: u.id, tokenHashes: null }]);

    ({ token } = await s.login('alice', NEW));
    assert.equal((await s.request('POST', '/api/v1/account/delete', { token, body: { password: NEW } })).status, 200);
    assert.deepEqual(revokedCalls(s.primary).slice(2), [{ userId: u.id, tokenHashes: null }]);
});

test('on the real store: revoking one session broadcasts its hash, logout-all no list', async (t) => {
    const primary = createFakePrimary();
    const s = await startReal(t, {}, { primary });
    const id = s.store.users.create({ username: 'alice', email: 'alice@example.org', passwordHash: await s.hasher.hash(PW), emailVerified: true });
    const login = async () => (await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW } })).json.token;
    const a = await login(), b = await login();
    const other = (await s.request('GET', '/api/v1/auth/sessions', { token: a })).json.sessions.find((x) => !x.current);
    assert.equal((await s.request('DELETE', `/api/v1/auth/sessions/${other.id}`, { token: a })).status, 200);
    assert.deepEqual(revokedCalls(primary), [{ userId: id, tokenHashes: [sha256Hex(b)] }]);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: a })).status, 200);
    assert.equal((await s.request('POST', '/api/v1/auth/logout-all', { token: a })).status, 200);
    assert.deepEqual(revokedCalls(primary).slice(1), [{ userId: id, tokenHashes: null }]);
});

test('logout-all', async (t) => {
    const { s } = await setup();
    t.after(s.close);
    const a = await s.login('alice', PW);
    const b = await s.login('alice', PW);
    assert.equal((await s.request('POST', '/api/v1/auth/logout-all', { token: a.token })).status, 200);
    for (const tok of [a.token, b.token]) assert.equal((await s.request('GET', '/api/v1/account/me', { token: tok })).status, 401);
});

test('idle expiry, sliding with use, and the absolute maximum', async (t) => {
    const { s } = await setup({ SESSION_IDLE_DAYS: '2', SESSION_MAX_DAYS: '5' });
    t.after(s.close);
    const idle = await s.login('alice', PW);
    s.now.advance(2 * DAY + 1);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: idle.token })).status, 401, 'unused for 2 days');

    const used = await s.login('alice', PW);
    for (let d = 0; d < 4; d++) {
        s.now.advance(DAY + 3600000);
        assert.equal((await s.request('GET', '/api/v1/account/me', { token: used.token })).status, 200, `day ${d + 1}`);
    }
    s.now.advance(DAY);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: used.token })).status, 401, 'older than SESSION_MAX_DAYS');
});

test('lastSeen is written at most every 5 minutes', async (t) => {
    const { s } = await setup();
    t.after(s.close);
    const { token } = await s.login('alice', PW);
    const before = s.store._raw.calls.touch;
    for (let i = 0; i < 10; i++) { s.auth.validateToken(token); s.now.advance(20000); }
    assert.equal(s.store._raw.calls.touch, before, '200 s of use: no write');
    s.now.advance(120000);
    s.auth.validateToken(token);
    assert.equal(s.store._raw.calls.touch, before + 1);
    s.auth.validateToken(token);
    assert.equal(s.store._raw.calls.touch, before + 1);
});

test('the 30 s cache: a revocation made elsewhere is seen within 30 s, invalidate() drops it at once', async (t) => {
    const { s, u } = await setup();
    t.after(s.close);
    const { token } = await s.login('alice', PW);
    assert.ok(s.auth.validateToken(token));
    s.store.sessions.revokeAllForUser(u.id);         // another process, broadcast lost
    assert.ok(s.auth.validateToken(token), 'still cached');
    s.now.advance(SESSION_CACHE_TTL_MS);
    assert.equal(s.auth.validateToken(token), null, 'reloaded after 30 s');

    const b = await s.login('alice', PW);
    assert.ok(s.auth.validateToken(b.token));
    s.store.sessions.revokeAllForUser(u.id);
    s.auth.invalidate({ userId: u.id, tokenHashes: [sha256Hex(b.token)] });
    assert.equal(s.auth.validateToken(b.token), null, 'dropped by hash');

    const c = await s.login('alice', PW);
    assert.ok(s.auth.validateToken(c.token));
    s.store.sessions.revokeAllForUser(u.id);
    s.auth.invalidate({ userId: u.id, tokenHashes: [] });
    assert.equal(s.auth.validateToken(c.token), null, 'dropped for the whole user');
});

test('a deleted or disabled user has no valid session', async (t) => {
    const { s, u } = await setup();
    t.after(s.close);
    const { token } = await s.login('alice', PW);
    s.store.users.update(u.id, { status: 'deleted' });
    s.now.advance(SESSION_CACHE_TTL_MS);
    assert.equal(s.auth.validateToken(token), null);
});
