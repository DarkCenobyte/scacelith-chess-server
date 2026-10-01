// The stricter limits of the auth family (abuse design 3.5 and the password recovery limits):
// registration, resend and forgot per address with their IPv6 /48 ceilings, the reset form,
// second factors per account (two workers sharing one primary), re-authentication per account,
// the Google sign-in's /48 limits; and checkRates giving back the earlier tokens of a refused
// request.

import test from 'node:test';
import assert from 'node:assert/strict';
import { base32Decode, totp } from '../../src/security/totp.js';
import { register as registerSso } from '../../src/http/routes/sso.js';
import { register as registerAuth } from '../../src/http/routes/auth.js';
import { register as registerAccount } from '../../src/http/routes/account.js';
import { register as registerExport } from '../../src/http/routes/account-export.js';
import { testConfig } from '../../src/config.js';
import { startTestServer, TEST_DEFAULTS } from './helpers/auth-fakes.js';

const PW = 'correct horse battery';
const REG = '/api/v1/auth/register';
const FORGOT = '/api/v1/auth/password/forgot';
const HOUR = 3600000;
const reg = (i) => ({ username: `player${i}`, email: `p${i}@example.com`, password: 'ivory rook takes e5' });
const takes = (s, key) => s.primary.calls.filter((c) => c.type === 'ratelimit.take' && c.payload.key === key);

/** A route table of a module, as { 'METHOD path': opts }. */
function routesOf(registerFn, deps) {
    const routes = {};
    const add = (method) => (path, handler, opts) => { routes[`${method} ${path}`] = opts; };
    registerFn({ get: add('GET'), post: add('POST'), put: add('PUT'), delete: add('DELETE'), page: (m, p, h, o) => { routes[`PAGE ${m} ${p}`] = o; } }, deps);
    return routes;
}

test('the rates of the auth family: keys, windows, /48 ceilings, shared, refusal weight', () => {
    const config = testConfig();
    const auth = routesOf(registerAuth, { config, auth: {} });
    const keys = (opts) => [].concat(opts.rate).map((r) => `${r.key}:${r.limit}/${r.windowMs}${r.prefixLimit ? `,48:${r.prefixLimit}` : ''}${r.shared ? ',shared' : ''}`);
    assert.deepEqual(keys(auth['POST /auth/register']), ['auth:20/600000,48:100,shared', 'auth_register:10/3600000,48:30,shared']);
    assert.deepEqual(keys(auth['POST /auth/verify-email/resend']), ['auth:20/600000,48:100,shared', 'auth_mail:10/3600000,48:30,shared']);
    assert.deepEqual(keys(auth['POST /auth/password/forgot']),
        ['auth:20/600000,48:100,shared', 'auth_forgot:3/3600000,48:9,shared', 'auth_forgot_day:10/86400000,48:30,shared']);
    assert.deepEqual(keys(auth['POST /auth/password/reset']), ['auth:20/600000,48:100,shared', 'auth_reset:10/3600000,48:30,shared']);
    assert.deepEqual(keys(auth['PAGE POST /reset-password']), keys(auth['POST /auth/password/reset']), 'the reset form counts like the API');
    assert.deepEqual(keys(auth['POST /auth/login']), ['auth:20/600000,48:100,shared']);
    assert.deepEqual(keys(auth['PAGE POST /verify-email']), ['auth:20/600000,48:100,shared']);
    for (const [name, opts] of Object.entries(auth)) {
        for (const r of [].concat(opts.rate || [])) {
            if (r.key.startsWith('auth')) assert.equal(r.abuseWeight, 5, `${name} ${r.key}: refusals weigh 5 toward a block`);
        }
    }

    const sso = routesOf(registerSso, { config, auth: {} });
    assert.deepEqual(keys(sso['POST /auth/sso/google/start']), ['sso_start:30/600000,48:90,shared']);
    assert.deepEqual(keys(sso['POST /auth/sso/complete']), ['auth:20/600000,48:100,shared'], 'the auth bucket, its /48 included');

    const account = routesOf(registerAccount, { config, auth: {} });
    for (const p of ['/account/password', '/account/mfa/totp/setup', '/account/mfa/totp/enable', '/account/mfa/totp/disable',
        '/account/mfa/recovery-codes', '/account/delete', '/account/email']) {
        assert.deepEqual(keys(account[`POST ${p}`]), ['reauth:20/600000,48:100,shared', 'reauth_user:10/600000,shared'], p);
        assert.equal([].concat(account[`POST ${p}`].rate)[1].by, 'user', p);
    }
    const exp = routesOf(registerExport, { config, auth: {}, store: {} });
    assert.deepEqual(keys(exp['POST /account/export']), ['account_export:5/3600000,shared', 'reauth:20/600000,48:100,shared', 'reauth_user:10/600000,shared']);
});

test('registration: AUTH_REGISTER_PER_HOUR per address, 3 times that per IPv6 /48; the auth tokens of a refused one are given back', async (t) => {
    const s = await startTestServer({ env: { AUTH_REGISTER_PER_HOUR: '2', AUTH_RATE_PER_IP: '5' } });
    t.after(s.close);
    let n = 0;
    for (let i = 0; i < 2; i++) assert.equal((await s.request('POST', REG, { body: reg(n++), ip: '192.0.2.10' })).status, 202);
    for (let i = 0; i < 4; i++) {
        const r = await s.request('POST', REG, { body: reg(n++), ip: '192.0.2.10' });
        assert.deepEqual([r.status, r.json.error], [429, 'rate_limited']);
        assert.ok(r.json.retryAfter > 0 && r.headers['retry-after'] === String(r.json.retryAfter));
    }
    assert.equal(takes(s, 'auth_register:192.0.2.10').length, 2, 'shared through the primary (the refusals came from the local stage)');
    // auth (5 per 10 minutes): 2 spent by the registrations; the 4 refused ones gave theirs back.
    for (let i = 0; i < 3; i++) {
        const r = await s.request('POST', '/api/v1/auth/login', { body: { login: 'nobody', password: 'wrong password' }, ip: '192.0.2.10' });
        assert.equal(r.status, 401, `login ${i}: the auth bucket still has its tokens`);
    }
    assert.equal((await s.request('POST', '/api/v1/auth/login', { body: { login: 'nobody', password: 'x' }, ip: '192.0.2.10' })).status, 429);
    assert.ok(s.primary.calls.some((c) => c.type === 'ratelimit.refund' && c.payload.key === 'auth:192.0.2.10'), 'shared token refunded too');
    assert.equal((await s.request('POST', REG, { body: reg(n++), ip: '192.0.2.11' })).status, 202, 'another address');

    // IPv6: 2 per /64, 6 per /48.
    for (let net = 1; net <= 3; net++) {
        for (let i = 0; i < 2; i++) assert.equal((await s.request('POST', REG, { body: reg(n++), ip: `2001:db8:7:${net}::1` })).status, 202);
        assert.equal((await s.request('POST', REG, { body: reg(n++), ip: `2001:db8:7:${net}::2` })).status, 429, `/64 ${net}`);
    }
    const r = await s.request('POST', REG, { body: reg(n++), ip: '2001:db8:7:4::1' });
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited'], 'a fourth /64 of the same /48');
    assert.equal((await s.request('POST', REG, { body: reg(n++), ip: '2001:db8:8:1::1' })).status, 202, 'another /48');
});

test('password recovery: 3 per hour and 10 per day per address, the same answers for known and unknown addresses', async (t) => {
    const env = { ...Object.fromEntries(['AUTH_FORGOT_PER_HOUR', 'AUTH_FORGOT_PER_DAY'].map((k) => [k, ''])) };
    const s = await startTestServer({ env });       // the defaults: 3 / hour, 10 / day
    t.after(s.close);
    assert.deepEqual([s.config.authForgotPerHour, s.config.authForgotPerDay], [3, 10]);
    await s.createUser({ username: 'alice', email: 'alice@example.com' });
    const ip = '198.51.100.20';
    const forgot = (email, from = ip) => s.request('POST', FORGOT, { body: { email }, ip: from });
    const answers = [];
    for (const email of ['alice@example.com', 'ghost@example.com', 'alice@example.com']) answers.push(await forgot(email));
    for (const r of answers) assert.deepEqual([r.status, r.json], [202, { status: 'accepted' }]);
    const known = await forgot('alice@example.com'), unknown = await forgot('ghost@example.com');
    assert.deepEqual([known.status, known.json.error], [429, 'rate_limited'], 'the fourth of the hour');
    assert.deepEqual([unknown.status, unknown.json.error], [429, 'rate_limited']);
    assert.equal(known.text.replace(/"retryAfter":\d+/, ''), unknown.text.replace(/"retryAfter":\d+/, ''), 'a refusal says nothing about the address');
    assert.ok(known.json.retryAfter > 60, 'most of an hour');
    assert.equal((await forgot('ghost@example.com', '198.51.100.21')).status, 202, 'another address');

    // The hour slides by; the day does not: 3 + 3 + 3 + 1 = 10, then the day's limit.
    for (const hours of [2, 2]) {
        s.now.advance(hours * HOUR);
        for (let i = 0; i < 3; i++) assert.equal((await forgot(`x${i}@example.com`)).status, 202);
        assert.equal((await forgot('y@example.com')).status, 429);
    }
    s.now.advance(2 * HOUR);
    assert.equal((await forgot('z@example.com')).status, 202, 'the tenth of the day');
    const day = await forgot('z2@example.com');
    assert.deepEqual([day.status, day.json.error], [429, 'rate_limited'], 'the eleventh of the day');
    assert.ok(day.json.retryAfter > 3600, `the day's window (${day.json.retryAfter} s)`);
    assert.ok(takes(s, `auth_forgot_day:${ip}`).length >= 10);
    // The refusals by the day's limit gave the hour's tokens back: tomorrow, 3 again.
    s.now.advance(30 * HOUR);
    for (let i = 0; i < 3; i++) assert.equal((await forgot(`n${i}@example.com`)).status, 202, `next day ${i}`);
    assert.equal((await forgot('n3@example.com')).status, 429);
    // One reset e-mail to alice per 5 minutes (the per-address throttle) on top of it all.
    const mails = s.mailer.sent.filter((m) => m.to === 'alice@example.com');
    assert.equal(mails.length, 1, 'two accepted requests within 5 minutes, one e-mail');
});

test('password recovery: an IPv6 /48 gets 3 times the limits of one /64', async (t) => {
    const s = await startTestServer({ env: { AUTH_FORGOT_PER_HOUR: '3', AUTH_FORGOT_PER_DAY: '10' } });
    t.after(s.close);
    let accepted = 0;
    for (let net = 1; net <= 4; net++) {
        for (let i = 0; i < 3; i++) {
            const r = await s.request('POST', FORGOT, { body: { email: `a${net}${i}@example.com` }, ip: `2001:db8:42:${net}::9` });
            if (r.status === 202) accepted++; else assert.equal(r.status, 429);
        }
    }
    assert.equal(accepted, 9, '3 per hour per /64, 9 per hour for the /48');
    assert.ok(takes(s, 'auth_forgot/48:2001:db8:42::/48').length >= 9);
});

test('resend of the confirmation: AUTH_MAIL_PER_HOUR per address; reset submissions: AUTH_RESET_PER_HOUR, the form included', async (t) => {
    const s = await startTestServer({ env: { AUTH_MAIL_PER_HOUR: '2', AUTH_RESET_PER_HOUR: '2' } });
    t.after(s.close);
    await s.createUser({ username: 'alice', email: 'alice@example.com', verified: false });
    const resend = (email) => s.request('POST', '/api/v1/auth/verify-email/resend', { body: { email }, ip: '203.0.113.5' });
    assert.equal((await resend('alice@example.com')).status, 202);
    assert.equal((await resend('ghost@example.com')).status, 202);
    let r = await resend('alice@example.com');
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited']);
    assert.ok(takes(s, 'auth_mail:203.0.113.5').length >= 2);

    const reset = (ip) => s.request('POST', '/api/v1/auth/password/reset', { body: { token: 'x'.repeat(43), newPassword: 'a new long password' }, ip });
    assert.equal((await reset('203.0.113.6')).status, 400, 'invalid token');
    const form = await s.request('POST', '/reset-password', {
        raw: `token=${'x'.repeat(43)}&newPassword=aaaaaaaaaaaa&confirmPassword=aaaaaaaaaaaa`, contentType: 'application/x-www-form-urlencoded', ip: '203.0.113.6',
    });
    assert.equal(form.status, 400);
    r = await reset('203.0.113.6');
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited'], 'the API and the form share auth_reset');
    const page = await s.request('POST', '/reset-password', {
        raw: `token=${'x'.repeat(43)}&newPassword=aaaaaaaaaaaa&confirmPassword=aaaaaaaaaaaa`, contentType: 'application/x-www-form-urlencoded', ip: '203.0.113.6',
    });
    assert.equal(page.status, 429);
    assert.match(page.headers['content-type'], /text\/html/);
    assert.equal((await reset('203.0.113.7')).status, 400, 'another address');
});

/** Two workers (two API handlers and auth services) of one server: one primary, one store, one clock. */
async function twoWorkers(env) {
    const a = await startTestServer({ env });
    const b = await startTestServer({ env, primary: a.primary, store: a.store, now: a.now, hasher: a.hasher });
    return { a, b, close: async () => { await b.close(); await a.close(); } };
}

async function enroll(s, username) {
    const u = await s.createUser({ username, password: PW });
    const { token } = await s.login(username, PW);
    const setup = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } });
    const secret = base32Decode(setup.json.secret);
    const en = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: totp(secret, s.now()) } });
    assert.equal(en.status, 200);
    s.now.advance(30000);
    return { u, token, secret, recoveryCodes: en.json.recoveryCodes };
}

test('second factors: AUTH_MFA_PER_ACCOUNT per 15 minutes for the account, whole server, refused before the code is checked', async (t) => {
    const w = await twoWorkers({ AUTH_MFA_PER_ACCOUNT: '3' });
    t.after(w.close);
    const { a, b } = w;
    const alice = await enroll(a, 'alice');
    const mfaToken = async (s) => (await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW } })).json.mfaToken;
    // Three wrong codes from three addresses, on both workers.
    let r = await a.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaToken(a), code: '000000' }, ip: '192.0.2.1' });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_code']);
    r = await b.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaToken(b), code: '000001' }, ip: '192.0.2.2' });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_code']);
    r = await b.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaToken(b), code: '000002' }, ip: '192.0.2.3' });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_code']);
    // The fourth: refused before the code is looked at, on either worker; the recovery code stays.
    const codes = a.store.mfa.countRecoveryCodes(alice.u.id);
    const token = await mfaToken(a);
    r = await a.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: token, recoveryCode: alice.recoveryCodes[0] }, ip: '192.0.2.4' });
    assert.deepEqual([r.status, r.json.error], [429, 'too_many_attempts']);
    assert.ok(r.json.retryAfter > 0 && r.json.retryAfter <= 900);
    assert.equal(a.store.mfa.countRecoveryCodes(alice.u.id), codes, 'no recovery code spent');
    r = await b.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: token, code: totp(alice.secret, a.now()) }, ip: '192.0.2.5' });
    assert.deepEqual([r.status, r.json.error], [429, 'too_many_attempts'], 'the other worker too');
    // Re-authentication checks the second factor through the same limit.
    r = await a.request('POST', '/api/v1/account/mfa/totp/disable', { token: alice.token, body: { password: PW, code: totp(alice.secret, a.now()) } });
    assert.deepEqual([r.status, r.json.error], [429, 'too_many_attempts']);
    assert.ok(a.primary.calls.filter((c) => c.type === 'ratelimit.take' && c.payload.key === `mfa:u${alice.u.id}`).every((c) => c.payload.windowMs === 900000));
    // Another account is not concerned; 15 minutes later alice signs in with a recovery code.
    const bob = await enroll(a, 'bob');
    const bt = (await a.request('POST', '/api/v1/auth/login', { body: { login: 'bob', password: PW } })).json.mfaToken;
    assert.equal((await a.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: bt, code: totp(bob.secret, a.now()) } })).status, 200);
    a.now.advance(30 * 60000);
    r = await b.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaToken(b), recoveryCode: alice.recoveryCodes[0] } });
    assert.equal(r.status, 200);
    assert.equal(a.store.mfa.countRecoveryCodes(alice.u.id), codes - 1);
});

test('re-authentication: AUTH_REAUTH_PER_USER per 10 minutes for the account, whatever the address', async (t) => {
    const w = await twoWorkers({ AUTH_REAUTH_PER_USER: '2' });
    t.after(w.close);
    const { a, b } = w;
    await a.createUser({ username: 'alice', password: PW });
    await a.createUser({ username: 'bob', password: PW });
    const alice = (await a.login('alice', PW)).token, bob = (await a.login('bob', PW)).token;
    const change = (s, token, ip) => s.request('POST', '/api/v1/account/password', { token, body: { currentPassword: 'guess guess guess', newPassword: 'whatever new one' }, ip });
    assert.equal((await change(a, alice, '192.0.2.1')).status, 403);
    assert.equal((await change(b, alice, '198.51.100.1')).status, 403);
    const r = await change(a, alice, '203.0.113.1');
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited'], 'a third address, the other worker: the account is limited');
    assert.ok(takes(a, 'reauth_user:u1').length >= 2);
    const del = await b.request('POST', '/api/v1/account/delete', { token: alice, body: { password: PW }, ip: '203.0.113.2' });
    assert.equal(del.status, 429, 'every route that asks for the password');
    assert.equal((await change(a, bob, '203.0.113.1')).status, 403, 'another account from the same address');
    a.now.advance(21 * 60000);
    assert.equal((await change(b, alice, '203.0.113.1')).status, 403, 'once the 10-minute window has passed');
});

test('Google sign-in completion: the auth limit with its IPv6 /48 ceiling', async (t) => {
    const s = await startTestServer({ env: { AUTH_RATE_PER_IP: '2', AUTH_RATE_PER_PREFIX: '3' } });
    t.after(s.close);
    const complete = (ip) => s.request('POST', '/api/v1/auth/sso/complete', { body: { ssoTicket: 'sso_x', username: 'someone' }, ip });
    assert.notEqual((await complete('2001:db8:5:1::1')).status, 429);
    assert.notEqual((await complete('2001:db8:5:1::1')).status, 429);
    assert.equal((await complete('2001:db8:5:1::1')).status, 429, 'the /64');
    assert.notEqual((await complete('2001:db8:5:2::1')).status, 429);
    assert.equal((await complete('2001:db8:5:3::1')).status, 429, 'the /48: 3');
});

test('the test environments raise every new auth limit', () => {
    for (const k of ['AUTH_REGISTER_PER_HOUR', 'AUTH_MAIL_PER_HOUR', 'AUTH_FORGOT_PER_HOUR', 'AUTH_FORGOT_PER_DAY', 'AUTH_RESET_PER_HOUR',
        'AUTH_MFA_PER_ACCOUNT', 'AUTH_REAUTH_PER_USER', 'USER_RATE_PER_MIN']) {
        assert.ok(Number(TEST_DEFAULTS[k]) >= 10000, k);
    }
});
