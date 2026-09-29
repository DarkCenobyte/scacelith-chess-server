import test from 'node:test';
import assert from 'node:assert/strict';
import { base32Decode, hotp, totp, totpStep } from '../../src/security/totp.js';
import { linkIn, startTestServer } from './helpers/auth-fakes.js';

const PW = 'correct horse battery';

/** A server with a logged-in user who has enrolled TOTP. */
async function enrolled(env = {}) {
    const s = await startTestServer({ env });
    const u = await s.createUser({ username: 'alice', password: PW });
    const { token } = await s.login('alice', PW);
    const setup = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } });
    assert.equal(setup.status, 200);
    const secret = base32Decode(setup.json.secret);
    const enable = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: totp(secret, s.now()) } });
    assert.equal(enable.status, 200);
    s.now.advance(30000);      // the next code is a new step
    return { s, u, token, secret, setup: setup.json, recoveryCodes: enable.json.recoveryCodes };
}

async function mfaStep(s) {
    const r = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW } });
    assert.equal(r.status, 200);
    assert.equal(r.json.mfaRequired, true);
    assert.match(r.json.mfaToken, /^mfa_[A-Za-z0-9_-]{43}$/);
    assert.equal(r.json.expiresIn, 300);
    assert.equal(r.json.token, undefined, 'no session before the second factor');
    return r.json.mfaToken;
}

test('setup: password re-authentication, secret, otpauth URI; pending until enabled', async (t) => {
    const s = await startTestServer({ env: { SERVER_NAME: 'Scacelith Club' } });
    t.after(s.close);
    const u = await s.createUser({ username: 'alice', password: PW });
    const { token } = await s.login('alice', PW);
    let r = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: 'wrong password' } });
    assert.deepEqual([r.status, r.json.error], [403, 'invalid_password']);
    r = await s.request('POST', '/api/v1/account/mfa/totp/setup', { body: { password: PW } });
    assert.equal(r.status, 401, 'needs a session');
    r = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } });
    assert.equal(r.status, 200);
    assert.match(r.json.secret, /^[A-Z2-7]{32}$/);
    assert.equal(r.json.uri, `otpauth://totp/Scacelith%20Club:alice?secret=${r.json.secret}&issuer=Scacelith%20Club&algorithm=SHA1&digits=6&period=30`);
    const row = s.store.users.byId(u.id);
    assert.equal(row.mfaEnabled, false);
    assert.match(row.pendingMfaSecretEnc, /^v1\./);
    assert.ok(!row.pendingMfaSecretEnc.includes(r.json.secret), 'encrypted at rest');
    assert.equal((await s.login('alice', PW)).mfaRequired, undefined, 'a pending secret does not change the login');
    r = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: '000000' } });
    assert.deepEqual([r.status, r.json.error], [403, 'invalid_code']);
    r = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: 'abcdef' } });
    assert.equal(r.status, 400);
});

test('enable: 10 recovery codes, stored hashed; login then needs the second factor', async (t) => {
    const { s, u, recoveryCodes, secret, token } = await enrolled();
    t.after(s.close);
    assert.equal(recoveryCodes.length, 10);
    for (const c of recoveryCodes) assert.match(c, /^[0-9a-z]{4}-[0-9a-z]{4}-[0-9a-z]{2}$/);
    const hashes = [...s.store._raw.codes.get(u.id)];
    assert.equal(hashes.length, 10);
    assert.ok(hashes.every((h) => /^[0-9a-f]{64}$/.test(h)));
    const row = s.store.users.byId(u.id);
    assert.equal(row.mfaEnabled, true);
    assert.equal(row.pendingMfaSecretEnc, null);
    assert.equal((await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } })).json.error, 'mfa_already_enabled');
    const mfaToken = await mfaStep(s);
    const r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken, code: totp(secret, s.now()) } });
    assert.equal(r.status, 200);
    assert.match(r.json.token, /^sct_/);
    assert.equal(r.json.user.mfaEnabled, true);
    const again = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken, code: totp(secret, s.now()) } });
    assert.deepEqual([again.status, again.json.error], [401, 'invalid_mfa_token'], 'the MFA token is single use');
});

test('TOTP window and replay protection', async (t) => {
    const { s, secret } = await enrolled();
    t.after(s.close);
    // The step before the current one was consumed by the enable call: it is a replay.
    const r0 = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaStep(s), code: hotp(secret, totpStep(s.now()) - 1) } });
    assert.equal(r0.status, 401);
    s.now.advance(30000);
    const step = totpStep(s.now());
    // A code of the previous step is still accepted...
    let r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaStep(s), code: hotp(secret, step - 1) } });
    assert.equal(r.status, 200);
    // ...but not twice, nor any older step once a newer one was used.
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaStep(s), code: hotp(secret, step) } });
    assert.equal(r.status, 200);
    const t2 = await mfaStep(s);
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: t2, code: hotp(secret, step) } });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_code'], 'replay');
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: t2, code: hotp(secret, step - 1) } });
    assert.equal(r.status, 401, 'older step');
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: t2, code: hotp(secret, step + 2) } });
    assert.equal(r.status, 401, 'outside the window');
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: t2, code: hotp(secret, step + 1) } });
    assert.equal(r.status, 200, 'the next step, within the window');
});

test('recovery codes are single use (in code or recoveryCode), in any case and spacing', async (t) => {
    const { s, recoveryCodes } = await enrolled();
    t.after(s.close);
    let r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaStep(s), recoveryCode: recoveryCodes[0].toUpperCase().replace(/-/g, ' ') } });
    assert.equal(r.status, 200);
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaStep(s), recoveryCode: recoveryCodes[0] } });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_code']);
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaStep(s), code: recoveryCodes[1] } });
    assert.equal(r.status, 200, 'the client may send a recovery code as "code"');
    assert.equal(s.store.mfa.countRecoveryCodes(1), 8);
    s.auth.events.flush();
    assert.equal(s.store._raw.securityEvents.filter((e) => e.kind === 'recovery_code_used').length, 2);
});

test('the MFA token: 5 wrong codes, expiry after 5 minutes, per-account delay', async (t) => {
    const { s, secret } = await enrolled();
    t.after(s.close);
    const tok = await mfaStep(s);
    for (let i = 0; i < 5; i++) {
        const r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: tok, code: String(100000 + i) } });
        assert.ok([401, 429].includes(r.status));
        if (r.status === 429) { assert.equal(r.json.error, 'too_many_attempts'); s.now.advance(r.json.retryAfter * 1000); }
    }
    let r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: tok, code: totp(secret, s.now()) } });
    assert.equal(r.json.error, 'invalid_mfa_token', 'burned after 5 wrong codes');
    s.now.advance(15 * 60000);
    const tok2 = await mfaStep(s);
    s.now.advance(5 * 60000 + 1);
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: tok2, code: totp(secret, s.now()) } });
    assert.equal(r.json.error, 'invalid_mfa_token', 'expired');
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: 'mfa_' + 'x'.repeat(43), code: '123456' } });
    assert.equal(r.json.error, 'invalid_mfa_token');
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaStep(s) } });
    assert.equal(r.status, 400);
});

test('a ban decided between the two steps is enforced at the second', async (t) => {
    const { s, u, secret } = await enrolled();
    t.after(s.close);
    const tok = await mfaStep(s);
    s.store.sanctions.create({ userId: u.id, kind: 'ban', startsAt: s.now() - 1, endsAt: null });
    const r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: tok, code: totp(secret, s.now()) } });
    assert.deepEqual([r.status, r.json.error, r.json.until], [403, 'banned', null]);
});

test('disable: password + code or recovery code; notice e-mail', async (t) => {
    const { s, u, token, secret, recoveryCodes } = await enrolled();
    t.after(s.close);
    let r = await s.request('POST', '/api/v1/account/mfa/totp/disable', { token, body: { password: PW } });
    assert.deepEqual([r.status, r.json.error], [403, 'mfa_code_required']);
    r = await s.request('POST', '/api/v1/account/mfa/totp/disable', { token, body: { password: 'bad password', code: totp(secret, s.now()) } });
    assert.equal(r.json.error, 'invalid_password');
    r = await s.request('POST', '/api/v1/account/mfa/totp/disable', { token, body: { password: PW, code: '000000' } });
    assert.equal(r.json.error, 'invalid_code');
    r = await s.request('POST', '/api/v1/account/mfa/totp/disable', { token, body: { password: PW, recoveryCode: recoveryCodes[3] } });
    assert.deepEqual([r.status, r.json], [200, { status: 'mfa_disabled' }]);
    const row = s.store.users.byId(u.id);
    assert.equal(row.mfaEnabled, false);
    assert.equal(row.mfaSecretEnc, null);
    assert.equal(s.store.mfa.countRecoveryCodes(u.id), 0);
    await s.mailer.idle();
    assert.ok(s.mailer.sent.some((m) => /Two-step verification was turned off/.test(m.subject)));
    assert.equal((await s.login('alice', PW)).mfaRequired, undefined);
    r = await s.request('POST', '/api/v1/account/mfa/totp/disable', { token, body: { password: PW, code: '123456' } });
    assert.deepEqual([r.status, r.json.error], [409, 'mfa_not_enabled']);
});

test('recovery codes can be regenerated with the password and a TOTP code (not a recovery code)', async (t) => {
    const { s, token, secret, recoveryCodes } = await enrolled();
    t.after(s.close);
    let r = await s.request('POST', '/api/v1/account/mfa/recovery-codes', { token, body: { password: PW, code: recoveryCodes[0] } });
    assert.deepEqual([r.status, r.json.error], [403, 'invalid_code']);
    r = await s.request('POST', '/api/v1/account/mfa/recovery-codes', { token, body: { password: PW, code: totp(secret, s.now()) } });
    assert.equal(r.status, 200);
    assert.equal(r.json.recoveryCodes.length, 10);
    assert.notDeepEqual(r.json.recoveryCodes, recoveryCodes);
    const old = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaStep(s), recoveryCode: recoveryCodes[5] } });
    assert.equal(old.status, 401, 'the old codes no longer work');
    const fresh = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: await mfaStep(s), recoveryCode: r.json.recoveryCodes[5] } });
    assert.equal(fresh.status, 200);
});

test('re-authentication failures are throttled per account', async (t) => {
    const s = await startTestServer({ env: { AUTH_FAILURES_PER_ACCOUNT: '2' } });
    t.after(s.close);
    await s.createUser({ username: 'alice', password: PW });
    const { token } = await s.login('alice', PW);
    for (let i = 0; i < 2; i++) assert.equal((await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: 'nope nope' } })).status, 403);
    const r = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } });
    assert.deepEqual([r.status, r.json.error], [429, 'too_many_attempts']);
    s.now.advance(r.json.retryAfter * 1000);
    assert.equal((await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } })).status, 200);
});

test('enable without setup', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'alice', password: PW });
    const { token } = await s.login('alice', PW);
    const r = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: '123456' } });
    assert.deepEqual([r.status, r.json.error], [409, 'mfa_setup_required']);
});

// A password reset or change between the two steps of a login ends the MFA step: no session opens
// with a password that was replaced (DESIGN 8, "a password reset always wins").
const NEW = 'a brand new passphrase';
const liveSessions = (s, userId) => s.store.sessions.listForUser(userId).filter((x) => !x.revokedAt).length;

test('a password reset between the two steps of a login ends the MFA step', async (t) => {
    const { s, u, secret, recoveryCodes } = await enrolled();
    t.after(s.close);
    const step = await mfaStep(s);                  // someone who has the old password
    const step2 = await mfaStep(s);
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    await s.mailer.idle();
    const mail = [...s.mailer.sent].reverse().find((m) => /Reset your/.test(m.subject));
    const resetToken = new URL(linkIn(mail.text)).searchParams.get('token');
    assert.equal((await s.request('POST', '/api/v1/auth/password/reset', { body: { token: resetToken, newPassword: NEW } })).status, 200);
    assert.equal(liveSessions(s, u.id), 0);
    let r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: step, code: totp(secret, s.now()) } });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_mfa_token'], r.text);
    assert.equal(liveSessions(s, u.id), 0, 'no session opened with the old password');
    // Refused before the code is checked: no recovery code is spent on a dead step.
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: step2, recoveryCode: recoveryCodes[0] } });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_mfa_token']);
    assert.equal(s.store.mfa.countRecoveryCodes(u.id), 10);
    // The step is spent.
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: step, code: totp(secret, s.now()) } });
    assert.equal(r.json.error, 'invalid_mfa_token');
    // A login with the new password goes through both steps.
    const fresh = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: NEW } });
    assert.equal(fresh.json.mfaRequired, true);
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: fresh.json.mfaToken, recoveryCode: recoveryCodes[0] } });
    assert.equal(r.status, 200, r.text);
});

test('a password change (POST /account/password) between the two steps of a login ends the MFA step', async (t) => {
    const { s, u, token, secret } = await enrolled();
    t.after(s.close);
    const step = await mfaStep(s);
    const r0 = await s.request('POST', '/api/v1/account/password', { token, body: { currentPassword: PW, newPassword: NEW } });
    assert.equal(r0.status, 200, r0.text);
    const before = liveSessions(s, u.id);
    const r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: step, code: totp(secret, s.now()) } });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_mfa_token'], r.text);
    assert.equal(liveSessions(s, u.id), before, 'no session opened with the old password');
});

test('a password change that lands while the MFA step checks the code (another process) still wins', async (t) => {
    const { s, u, secret } = await enrolled();
    t.after(s.close);
    const step = await mfaStep(s);
    const advance = s.store.users.advanceMfaStep;
    t.after(() => { s.store.users.advanceMfaStep = advance; });
    // The code check stores the TOTP step; another worker's reset writes a new hash meanwhile.
    s.store.users.advanceMfaStep = (id, st) => {
        s.store.users.update(id, { passwordHash: 'scrypt$10$8$1$AAAAAAAAAAAAAAAAAAAAAA$BBBBBBBBBBBBBBBBBBBBBBBBBBBB' });
        return advance(id, st);
    };
    const before = liveSessions(s, u.id);
    const r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: step, code: totp(secret, s.now()) } });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_mfa_token'], r.text);
    assert.equal(liveSessions(s, u.id), before, 'no session opened');
});

test('the MFA step keeps a digest of the password hash; a step written by an earlier build (without it) still completes', async (t) => {
    const { s, u, secret } = await enrolled();
    t.after(s.close);
    const step = await mfaStep(s);
    const row = [...s.store._raw.tokens.values()].find((x) => x.kind === 'mfa_login' && x.userId === u.id && !x.usedAt);
    const data = JSON.parse(row.data);
    assert.match(data.pwh, /^[0-9a-f]{64}$/);
    assert.ok(!row.data.includes(s.store.users.byId(u.id).passwordHash), 'not the stored hash itself');
    // The row as an earlier build wrote it: { attempts, clientLabel, method }.
    delete data.pwh;
    row.data = JSON.stringify(data);
    const r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: step, code: totp(secret, s.now()) } });
    assert.equal(r.status, 200, r.text);
});
