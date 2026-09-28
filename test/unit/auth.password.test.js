import test from 'node:test';
import assert from 'node:assert/strict';
import { base32Decode, totp } from '../../src/security/totp.js';
import { linkIn, startTestServer } from './helpers/auth-fakes.js';

const PW = 'correct horse battery';
const NEW = 'a brand new passphrase';
const FORM = 'application/x-www-form-urlencoded';

async function resetToken(s, email = 'alice@example.com') {
    await s.mailer.idle();
    const mail = [...s.mailer.sent].reverse().find((m) => m.to === email && /Reset your/.test(m.subject));
    const url = new URL(linkIn(mail.text));
    assert.equal(url.pathname, '/reset-password');
    return url.searchParams.get('token');
}

test('forgot: 202 whatever the address, a reset e-mail only for accounts, throttled per address', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'alice' });
    const known = await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'ALICE@example.com' } });
    const unknown = await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'ghost@example.com' } });
    const invalid = await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'not-an-email' } });
    for (const r of [known, unknown, invalid]) assert.deepEqual([r.status, r.json], [202, { status: 'accepted' }]);
    await s.mailer.idle();
    assert.equal(s.mailer.sent.length, 1);
    assert.equal(s.mailer.sent[0].to, 'alice@example.com');
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    await s.mailer.idle();
    assert.equal(s.mailer.sent.length, 1, 'one e-mail per address per 5 minutes');
    const once = s.primary.calls.filter((c) => c.type === 'once.consume' && c.payload.key.startsWith('mail:reset:'));
    assert.equal(once.length, 4, 'the throttle is asked for unknown addresses too (same work)');
    assert.ok(once.every((c) => !c.payload.key.includes('@')), 'no address in the key');
    s.now.advance(5 * 60000 + 1);
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    await s.mailer.idle();
    assert.equal(s.mailer.sent.length, 2);
});

test('reset through the API: policy, single use, every session revoked, MFA untouched', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const u = await s.createUser({ username: 'alice', password: PW, verified: false });
    s.store.users.update(u.id, { emailVerified: true });
    const { token: session } = await s.login('alice', PW);
    // MFA on
    const setup = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token: session, body: { password: PW } });
    const secret = base32Decode(setup.json.secret);
    await s.request('POST', '/api/v1/account/mfa/totp/enable', { token: session, body: { code: totp(secret, s.now()) } });
    s.store.users.update(u.id, { emailVerified: false });

    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    const token = await resetToken(s);
    let r = await s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: 'short' } });
    assert.deepEqual([r.status, r.json.error, r.json.reason], [400, 'weak_password', 'too_short']);
    r = await s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: 'alice is my name' } });
    assert.equal(r.json.reason, 'contains_username');
    r = await s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: NEW } });
    assert.deepEqual([r.status, r.json], [200, { status: 'password_reset' }]);
    r = await s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: 'another new passphrase' } });
    assert.deepEqual([r.status, r.json.error], [400, 'invalid_token']);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: session })).status, 401, 'sessions revoked');
    const row = s.store.users.byId(u.id);
    assert.equal(row.mfaEnabled, true, 'MFA stays enabled');
    assert.equal(row.emailVerified, true, 'the link proved the address');
    const l = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: NEW } });
    assert.equal(l.json.mfaRequired, true);
    assert.equal((await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW } })).status, 401);
    await s.mailer.idle();
    assert.ok(s.mailer.sent.some((m) => /password was changed/.test(m.subject) && /reset with an e-mail link/.test(m.text)));
});

test('reset tokens expire after an hour', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'alice' });
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    const token = await resetToken(s);
    s.now.advance(3600001);
    const r = await s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: NEW } });
    assert.equal(r.json.error, 'invalid_token');
    assert.equal((await s.request('POST', '/api/v1/auth/password/reset', { body: { token: 'garbage', newPassword: NEW } })).json.error, 'invalid_token');
});

test('the reset page: form, mismatch, weak password, success, used link', async (t) => {
    const s = await startTestServer({ env: { SERVER_NAME: 'Club <Test>' } });
    t.after(s.close);
    await s.createUser({ username: 'alice' });
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    const token = await resetToken(s);
    let r = await s.request('GET', `/reset-password?token=${token}`);
    assert.equal(r.status, 200);
    assert.match(r.text, /<input type="hidden" name="token" value="[A-Za-z0-9_-]{43}">/);
    assert.match(r.text, /Club &lt;Test&gt;/, 'escaped');
    assert.doesNotMatch(r.text, /<script/i);
    r = await s.request('POST', '/reset-password', { raw: `token=${token}&newPassword=${encodeURIComponent(NEW)}&confirmPassword=different`, contentType: FORM });
    assert.equal(r.status, 400);
    assert.match(r.text, /The two passwords are different/);
    r = await s.request('POST', '/reset-password', { raw: `token=${token}&newPassword=qwertyuiop&confirmPassword=qwertyuiop`, contentType: FORM });
    assert.match(r.text, /too common/);
    r = await s.request('POST', '/reset-password', { raw: `token=${token}&newPassword=${encodeURIComponent(NEW)}&confirmPassword=${encodeURIComponent(NEW)}`, contentType: FORM });
    assert.equal(r.status, 200);
    assert.match(r.text, /Password changed/);
    assert.doesNotMatch(r.text, /sct_/);
    r = await s.request('GET', `/reset-password?token=${token}`);
    assert.equal(r.status, 400);
    assert.match(r.text, /invalid or expired/);
    await s.login('alice', NEW);
});

test('change password: current password required, other sessions revoked, current kept', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'alice', password: PW });
    const a = await s.login('alice', PW);
    const b = await s.login('alice', PW);
    let r = await s.request('POST', '/api/v1/account/password', { token: a.token, body: { currentPassword: 'wrong one!', newPassword: NEW } });
    assert.deepEqual([r.status, r.json.error], [403, 'invalid_password']);
    r = await s.request('POST', '/api/v1/account/password', { token: a.token, body: { currentPassword: PW, newPassword: 'password123' } });
    assert.equal(r.json.error, 'weak_password');
    r = await s.request('POST', '/api/v1/account/password', { token: a.token, body: { currentPassword: PW, newPassword: NEW } });
    assert.deepEqual([r.status, r.json], [200, { status: 'password_changed' }]);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: a.token })).status, 200);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: b.token })).status, 401);
    await s.login('alice', NEW);
    await s.mailer.idle();
    assert.ok(s.mailer.sent.some((m) => /password was changed/.test(m.subject)));
});

test('resend verification: 202 always, e-mail only to unconfirmed accounts', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'pending', verified: false });
    await s.createUser({ username: 'done' });
    for (const email of ['pending@example.com', 'done@example.com', 'ghost@example.com']) {
        const r = await s.request('POST', '/api/v1/auth/verify-email/resend', { body: { email } });
        assert.deepEqual([r.status, r.json], [202, { status: 'accepted' }]);
    }
    await s.mailer.idle();
    assert.deepEqual(s.mailer.sent.map((m) => m.to), ['pending@example.com']);
    const url = new URL(linkIn(s.mailer.sent[0].text));
    const r = await s.request('POST', '/verify-email', { raw: `token=${url.searchParams.get('token')}`, contentType: FORM });
    assert.equal(r.status, 200);
    assert.equal(s.store.users.byUsername('pending').emailVerified, true);
});
