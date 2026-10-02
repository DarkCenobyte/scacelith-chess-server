import test from 'node:test';
import assert from 'node:assert/strict';
import { publicBaseUrl } from '../../src/auth/index.js';
import { createPasswordHasher } from '../../src/security/password.js';
import { solvePow } from '../../src/security/pow.js';
import { testConfig } from '../../src/config.js';
import { startTestServer } from './helpers/auth-fakes.js';

const LOGIN = '/api/v1/auth/login';

test('login by username or e-mail (case-insensitive): session token, user view', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const u = await s.createUser({ username: 'Alice', password: 'correct horse battery' });
    for (const login of ['Alice', 'alice', 'ALICE@EXAMPLE.COM', ' alice@example.com ']) {
        const r = await s.request('POST', LOGIN, { body: { login, password: 'correct horse battery', clientLabel: 'Windows 11' } });
        assert.equal(r.status, 200, login);
        assert.match(r.json.token, /^sct_[A-Za-z0-9_-]{43}$/);
        assert.equal(r.json.expiresAt, s.now() + 90 * 86400000);
        assert.deepEqual(r.json.user, { id: u.id, username: 'Alice', email: 'alice@example.com', emailVerified: true, mfaEnabled: false, googleLinked: false, hasPassword: true, acceptChallenges: 'all', createdAt: s.now(), lastLoginAt: s.now(), pendingEmail: null });
    }
    const rows = [...s.store._raw.sessions.values()];
    assert.equal(rows[0].clientLabel, 'Windows 11');
    assert.match(rows[0].tokenHash, /^[0-9a-f]{64}$/);
    assert.equal(s.store.users.byId(u.id).lastLoginAt, s.now());
});

test('unknown user, wrong password and password-less account: identical answers', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'alice' });
    await s.createUser({ username: 'googler', password: null });
    const answers = [];
    for (const [login, password] of [['nobody', 'whatever pass'], ['alice', 'wrong password'], ['googler', 'anything at all'], ['nobody@example.com', 'x']]) {
        const r = await s.request('POST', LOGIN, { body: { login, password } });
        answers.push({ status: r.status, body: r.json });
    }
    for (const a of answers) assert.deepEqual(a, { status: 401, body: { error: 'invalid_credentials', message: 'Wrong user name, e-mail or password.' } });
});

test('timing: unknown user and wrong password take similar time', async (t) => {
    const s = await startTestServer({ scryptLogN: 13, env: { AUTH_FAILURES_PER_ACCOUNT: '1000' } });
    t.after(s.close);
    await s.createUser({ username: 'alice' });
    await s.hasher.warmUp();
    const time = async (login) => {
        const t0 = process.hrtime.bigint();
        const r = await s.request('POST', LOGIN, { body: { login, password: 'not the password' } });
        assert.equal(r.status, 401);
        return Number(process.hrtime.bigint() - t0) / 1e6;
    };
    const known = [], unknown = [];
    // Interleaved samples and medians, so that load from other processes affects both sides alike.
    for (let i = 0; i < 15; i++) { known.push(await time('alice')); unknown.push(await time(`ghost${i}`)); }
    const med = (a) => a.sort((x, y) => x - y)[7];
    const ratio = med(known) / med(unknown);
    assert.ok(ratio > 0.6 && ratio < 1.67, `median known ${med(known).toFixed(1)} ms, unknown ${med(unknown).toFixed(1)} ms`);
});

test('email_unverified and banned only after the password matched', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const u = await s.createUser({ username: 'banned1' });
    s.store.sanctions.create({ userId: u.id, kind: 'ban', reason: 'cheating', source: 'auto', startsAt: s.now() - 1000, endsAt: s.now() + 3600000 });
    let r = await s.request('POST', LOGIN, { body: { login: 'banned1', password: 'wrong password' } });
    assert.equal(r.json.error, 'invalid_credentials');
    r = await s.request('POST', LOGIN, { body: { login: 'banned1', password: 'correct horse battery' } });
    assert.deepEqual([r.status, r.json.error, r.json.until], [403, 'banned', s.now() + 3600000]);
    s.now.advance(3600001);
    assert.equal((await s.request('POST', LOGIN, { body: { login: 'banned1', password: 'correct horse battery' } })).status, 200, 'ban over');
    await s.createUser({ username: 'unverified', verified: false });
    r = await s.request('POST', LOGIN, { body: { login: 'unverified', password: 'correct horse battery' } });
    assert.deepEqual([r.status, r.json.error], [403, 'email_unverified']);
});

test('per-account failure counter: exponential delay, identical for unknown accounts', async (t) => {
    const s = await startTestServer({ env: { AUTH_FAILURES_PER_ACCOUNT: '3' } });
    t.after(s.close);
    await s.createUser({ username: 'alice' });
    const attempt = (login, password = 'wrong password') => s.request('POST', LOGIN, { body: { login, password } });
    for (const login of ['alice', 'nobody']) {
        for (let i = 0; i < 3; i++) assert.equal((await attempt(login)).status, 401);
    }
    const a = await attempt('alice', 'correct horse battery');
    const b = await attempt('nobody');
    assert.deepEqual([a.status, a.json], [429, { error: 'too_many_attempts', message: 'Too many attempts; wait before trying again.', retryAfter: 2 }]);
    assert.deepEqual([b.status, b.json], [a.status, a.json], 'unknown accounts are throttled the same way');
    assert.equal(a.headers['retry-after'], '2');
    s.now.advance(2000);
    assert.equal((await attempt('alice')).status, 401);
    assert.equal((await attempt('alice')).json.retryAfter, 4, 'doubled');
    s.now.advance(4000);
    assert.equal((await attempt('alice', 'correct horse battery')).status, 200, 'the right password after the delay');
    assert.equal((await attempt('alice')).status, 401, 'counter reset by the success');
    s.auth.events.flush();
    const kinds = s.store._raw.securityEvents.map((e) => e.kind);
    assert.ok(kinds.includes('login_failed') && kinds.includes('login_lockout') && kinds.includes('login_throttled') && kinds.includes('login'));
    const failed = s.store._raw.securityEvents.find((e) => e.kind === 'login_failed');
    assert.equal(failed.ip, '127.0.0.1');
    assert.equal(typeof failed.detail, 'string');
});

test('a credential-stuffing wave turns on the login proof of work', async (t) => {
    const s = await startTestServer({ env: { POW_LOGIN_BITS: '6', POW_LOGIN_TRIGGER_PER_MIN: '4' } });
    t.after(s.close);
    await s.createUser({ username: 'alice' });
    for (let i = 0; i < 4; i++) {
        const r = await s.request('POST', LOGIN, { body: { login: `victim${i}`, password: 'guess guess' }, ip: `198.51.100.${i}` });
        assert.equal(r.status, 401);
    }
    await new Promise((r) => setImmediate(r));
    assert.equal(s.auth.loginPowActive(), true);
    const take = s.primary.calls.filter((c) => c.type === 'ratelimit.take' && c.payload.key === 'auth:login-failures');
    assert.equal(take.length, 4);
    assert.deepEqual(take[0].payload, { key: 'auth:login-failures', limit: 4, windowMs: 60000, cost: 1 });
    let r = await s.request('POST', LOGIN, { body: { login: 'alice', password: 'correct horse battery' } });
    assert.equal(r.status, 428);
    assert.equal(r.json.pow.bits, 6);
    const pow = { challenge: r.json.pow.challenge, nonce: solvePow(r.json.pow.challenge, 6) };
    r = await s.request('POST', LOGIN, { body: { login: 'alice', password: 'correct horse battery', pow } });
    assert.equal(r.status, 200);
    r = await s.request('POST', LOGIN, { body: { login: 'alice', password: 'correct horse battery', pow } });
    assert.deepEqual([r.status, r.json.reason], [428, 'replayed']);
    s.now.advance(5 * 60000 + 1);
    assert.equal(s.auth.loginPowActive(), false, 'the wave is over');
    assert.equal((await s.request('POST', LOGIN, { body: { login: 'alice', password: 'correct horse battery' } })).status, 200);
});

test('the primary\'s server-wide failure count alone also turns the proof of work on', async (t) => {
    const s = await startTestServer({ env: { POW_LOGIN_BITS: '6', POW_LOGIN_TRIGGER_PER_MIN: '300' } });
    t.after(s.close);
    s.primary.refuse = (type, p) => (type === 'ratelimit.take' && p.key === 'auth:login-failures' ? { allowed: false, retryAfterMs: 1000, count: 300 } : undefined);
    await s.request('POST', LOGIN, { body: { login: 'x', password: 'y' } });
    await new Promise((r) => setImmediate(r));
    assert.equal(s.auth.loginPowActive(), true);
});

test('outdated hashes are upgraded at login', async (t) => {
    const s = await startTestServer({ scryptLogN: 10 });
    t.after(s.close);
    const old = createPasswordHasher({ scrypt: { logN: 9 }, argon2: false });
    const id = s.store.users.create({ username: 'legacy', email: 'legacy@example.com', passwordHash: await old.hash('legacy passphrase 1'), emailVerified: true });
    await s.login('legacy', 'legacy passphrase 1');
    assert.match(s.store.users.byId(id).passwordHash, /^scrypt\$10\$/);
    await s.login('legacy', 'legacy passphrase 1');
});

test('MAX_SESSIONS_PER_USER: the oldest session is revoked and dropped from every cache', async (t) => {
    const s = await startTestServer({ env: { MAX_SESSIONS_PER_USER: '2' } });
    t.after(s.close);
    await s.createUser({ username: 'alice' });
    const a = await s.login('alice', 'correct horse battery');
    s.now.advance(1000);
    const b = await s.login('alice', 'correct horse battery');
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: a.token })).status, 200, 'cached');
    s.now.advance(1000);
    const c = await s.login('alice', 'correct horse battery');
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: a.token })).status, 401);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: b.token })).status, 200);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: c.token })).status, 200);
    const revoked = s.primary.calls.filter((x) => x.type === 'session.revoked');
    assert.equal(revoked.length, 1);
    assert.equal(revoked[0].payload.tokenHashes.length, 1);
});

test('request validation of the login body', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    for (const body of [{}, { login: 'a' }, { login: 'a', password: 'b', admin: true }, { login: '', password: 'x' }, { login: 'a', password: 'x'.repeat(1025) }]) {
        const r = await s.request('POST', LOGIN, { body });
        assert.equal(r.status, 400, JSON.stringify(body).slice(0, 60));
    }
});

test('public base URL of e-mail links', () => {
    assert.equal(publicBaseUrl(testConfig({ SERVER_PUBLIC_HOST: 'h.example', API_PORT: '443' })), 'http://h.example:443');
    const native = testConfig({ SERVER_PUBLIC_HOST: 'h.example', API_PORT: '443', TLS_MODE: 'native', TLS_CERT_FILE: '/c', TLS_KEY_FILE: '/k' });
    assert.equal(publicBaseUrl(native), 'https://h.example');
    assert.equal(publicBaseUrl(testConfig({ SERVER_PUBLIC_HOST: '::1', API_PORT: '8443', TLS_MODE: 'proxy' })), 'https://[::1]:8443');
});
