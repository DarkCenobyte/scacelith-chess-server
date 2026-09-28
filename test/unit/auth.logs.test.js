import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { base32Decode, totp } from '../../src/security/totp.js';
import { solvePow } from '../../src/security/pow.js';
import { capturedLogs, linkIn, startTestServer } from './helpers/auth-fakes.js';
import { startFakeOidc } from './helpers/auth-oidc.js';

// Runs every flow of the module and checks that no password, token, TOTP secret, recovery code,
// PKCE verifier or e-mail address reached the logs (which are at debug level in the tests).
test('no credential, token, code or e-mail address appears in the logs', async (t) => {
    const secrets = [];
    const keep = (...xs) => {
        for (const x of xs) { assert.ok(typeof x === 'string' && x.length >= 6, 'every step of the flow produced its secret'); secrets.push(x); }
        return xs[0];
    };
    let s = null;
    const idp = await startFakeOidc({ clientId: 'cid.apps.googleusercontent.com', clientSecret: 'GOCSPX-log-test',
        redirectUri: 'https://chess.example.org:8443/auth/sso/google/callback', now: () => s.now() });
    s = await startTestServer({
        env: { POW_REGISTER_BITS: '4', POW_LOGIN_BITS: '4', POW_LOGIN_TRIGGER_PER_MIN: '3', SSO_GOOGLE_ENABLED: '1',
            GOOGLE_CLIENT_ID: 'cid.apps.googleusercontent.com', GOOGLE_CLIENT_SECRET: 'GOCSPX-log-test' },
        oidcEndpoints: idp.endpoints,
    });
    t.after(async () => { await s.close(); await idp.close(); });
    const start = capturedLogs.length;
    const PW = keep('Sup3r secret pass phrase');
    const NEW = keep('An0ther secret pass phrase');
    keep('GOCSPX-log-test', 'alice@example.com', 'ALICE@example.com', 'ghost@example.com', 'magnus@gmail.com');

    // Registration with proof of work, e-mail confirmation.
    let r = await s.request('POST', '/api/v1/auth/register', { body: { username: 'alice', email: 'ALICE@example.com', password: PW } });
    const pow = { challenge: r.json.pow.challenge, nonce: solvePow(r.json.pow.challenge, 4) };
    r = await s.request('POST', '/api/v1/auth/register', { body: { username: 'alice', email: 'ALICE@example.com', password: PW, pow } });
    assert.equal(r.status, 202);
    await s.mailer.idle();
    const verifyToken = keep(new URL(linkIn(s.mailer.sent[0].text)).searchParams.get('token'));
    await s.request('POST', '/verify-email', { raw: `token=${verifyToken}`, contentType: 'application/x-www-form-urlencoded' });

    // Failed logins (they turn the login proof of work on), a login, MFA enrolment.
    for (let i = 0; i < 3; i++) await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: keep(`wrong guess ${i}!`) } });
    r = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW } });
    const lp = { challenge: r.json.pow.challenge, nonce: solvePow(r.json.pow.challenge, 4) };
    r = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW, pow: lp } });
    const session = keep(r.json.token);
    s.now.advance(5 * 60000 + 1);
    r = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token: session, body: { password: PW } });
    keep(r.json.secret, r.json.uri);
    const secret = base32Decode(r.json.secret);
    r = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token: session, body: { code: keep(totp(secret, s.now())) } });
    const codes = r.json.recoveryCodes;
    keep(...codes, ...codes.map((c) => c.replace(/-/g, '')));
    s.now.advance(30000);

    // MFA logins with a TOTP code and a recovery code, a wrong code.
    r = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW } });
    let mfaToken = keep(r.json.mfaToken);
    await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken, code: keep('000111') } });
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken, code: keep(totp(secret, s.now())) } });
    keep(r.json.token);
    r = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW } });
    mfaToken = keep(r.json.mfaToken);
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken, recoveryCode: codes[0] } });
    keep(r.json.token);
    s.now.advance(30000);
    r = await s.request('POST', '/api/v1/account/mfa/recovery-codes', { token: session, body: { password: PW, code: totp(secret, s.now()) } });
    keep(...r.json.recoveryCodes);

    // Password reset, change; sessions; logout.
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'ghost@example.com' } });
    await s.mailer.idle();
    const resetToken = keep(new URL(linkIn(s.mailer.sent.at(-1).text)).searchParams.get('token'));
    await s.request('GET', `/reset-password?token=${resetToken}`);
    await s.request('POST', '/reset-password', { raw: `token=${resetToken}&newPassword=${encodeURIComponent(NEW)}&confirmPassword=${encodeURIComponent(NEW)}`, contentType: 'application/x-www-form-urlencoded' });
    r = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: NEW } });
    s.now.advance(30000);
    r = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: keep(r.json.mfaToken), code: totp(secret, s.now()) } });
    const s2 = keep(r.json.token);
    await s.request('POST', '/api/v1/account/password', { token: s2, body: { currentPassword: NEW, newPassword: keep('Third secret pass phrase') } });
    await s.request('GET', '/api/v1/auth/sessions', { token: s2 });
    await s.request('GET', '/api/v1/account/me', { token: keep('sct_' + 'Q'.repeat(43)) });
    await s.request('POST', '/api/v1/auth/logout', { token: s2 });

    // Google sign-in, first time.
    const verifier = keep(crypto.randomBytes(32).toString('base64url'));
    const challenge = crypto.createHash('sha256').update(verifier).digest('base64url');
    r = await s.request('POST', '/api/v1/auth/sso/google/start', { body: { codeChallenge: challenge } });
    const attemptId = keep(r.json.attemptId);
    const q = idp.authorize(r.json.authUrl, { sub: '42', email: 'magnus@gmail.com', email_verified: true });
    keep(q.code, q.state);
    await s.request('GET', `/auth/sso/google/callback?${new URLSearchParams(q)}`);
    r = await s.request('POST', '/api/v1/auth/sso/google/poll', { body: { attemptId, codeVerifier: verifier } });
    const ticket = keep(r.json.ssoTicket);
    r = await s.request('POST', '/api/v1/auth/sso/complete', { body: { ssoTicket: ticket, username: 'magnus' } });
    keep(r.json.token);
    for (const call of idp.state.tokenCalls) keep(call.code_verifier);

    s.auth.close();
    const logs = capturedLogs.slice(start).join('');
    assert.ok(logs.includes('"login_failed"') && logs.includes('"mfa_enabled"') && logs.includes('"password_reset"') && logs.includes('"sso_account_created"'),
        'security events are logged');
    assert.ok(logs.includes('"route":"POST /api/v1/auth/login"'), 'access log at debug level');
    for (const x of secrets) assert.ok(!logs.includes(x), `leaked into the logs: ${x.slice(0, 12)}...`);
    assert.ok(!/127\.0\.0\.1"/.test(logs), 'client addresses are truncated in logs');
    // The persisted security events carry no secret either.
    const stored = JSON.stringify(s.store._raw.securityEvents);
    for (const x of secrets) if (!x.includes('@')) assert.ok(!stored.includes(x), `leaked into security events: ${x.slice(0, 12)}...`);
});
