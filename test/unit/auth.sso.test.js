import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { base32Decode, totp } from '../../src/security/totp.js';
import { startTestServer } from './helpers/auth-fakes.js';
import { startFakeOidc } from './helpers/auth-oidc.js';

const CLIENT_ID = '1234-abc.apps.googleusercontent.com';
const CLIENT_SECRET = 'GOCSPX-test-secret';
const PW = 'correct horse battery';

async function setup(env = {}) {
    const cfgEnv = {
        SSO_GOOGLE_ENABLED: '1', GOOGLE_CLIENT_ID: CLIENT_ID,
        // config.js decodes secrets as base64; see the report about SSO/SMTP secrets.
        GOOGLE_CLIENT_SECRET: CLIENT_SECRET, ...env,
    };
    // The provider signs with the server's (fake) clock, read once the server exists.
    let s = null;
    const idp = await startFakeOidc({ clientId: CLIENT_ID, clientSecret: CLIENT_SECRET,
        redirectUri: 'https://chess.example.org:8443/auth/sso/google/callback', now: () => s.now() });
    s = await startTestServer({ env: cfgEnv, oidcEndpoints: idp.endpoints });
    const pkce = () => {
        const verifier = crypto.randomBytes(32).toString('base64url');
        return { verifier, challenge: crypto.createHash('sha256').update(verifier).digest('base64url') };
    };
    async function start() {
        const p = pkce();
        const r = await s.request('POST', '/api/v1/auth/sso/google/start', { body: { codeChallenge: p.challenge } });
        assert.equal(r.status, 200, r.text);
        return { ...p, ...r.json };
    }
    async function callback(q) {
        return s.request('GET', `/auth/sso/google/callback?${new URLSearchParams(q)}`);
    }
    const poll = (a, verifier = a.verifier) => s.request('POST', '/api/v1/auth/sso/google/poll', { body: { attemptId: a.attemptId, codeVerifier: verifier } });
    /** start -> consent -> callback -> poll */
    async function signIn(claims) {
        const a = await start();
        const cb = await callback(idp.authorize(a.authUrl, claims));
        return { a, cb, r: await poll(a) };
    }
    return { s, idp, start, callback, poll, signIn, close: async () => { await s.close(); await idp.close(); } };
}

const claims = (over = {}) => ({ sub: '1098765', email: 'Magnus@Gmail.com', email_verified: true, name: 'Magnus Hansen', given_name: 'Magnus', ...over });

test('disabled unless SSO_GOOGLE_ENABLED', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const r = await s.request('POST', '/api/v1/auth/sso/google/start', { body: { codeChallenge: 'a'.repeat(43) } });
    assert.deepEqual([r.status, r.json.error], [404, 'sso_disabled']);
    const page = await s.request('GET', '/auth/sso/google/callback?code=x&state=y');
    assert.equal(page.status, 400);
    assert.match(page.text, /not enabled/);
});

test('first Google sign-in: authorization request, callback page, verifier-bound poll, username, account', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s, idp } = x;
    const a = await x.start();
    assert.match(a.attemptId, /^sso_[A-Za-z0-9_-]{43}$/);
    assert.deepEqual([a.pollMs, a.expiresIn], [2000, 600]);
    const u = new URL(a.authUrl);
    assert.equal(`${u.origin}${u.pathname}`, 'https://accounts.google.com/o/oauth2/v2/auth');
    const q = Object.fromEntries(u.searchParams);
    assert.equal(q.client_id, CLIENT_ID);
    assert.equal(q.redirect_uri, s.config.googleRedirectUri);
    assert.equal(q.response_type, 'code');
    assert.equal(q.scope, 'openid email profile');
    assert.equal(q.code_challenge_method, 'S256');
    assert.notEqual(q.code_challenge, a.challenge, 'the server uses its own PKCE pair with Google');
    assert.match(q.state, /^[A-Za-z0-9_-]{43}$/);
    assert.match(q.nonce, /^[A-Za-z0-9_-]{43}$/);

    assert.deepEqual((await x.poll(a)).json, { status: 'pending' });
    const cb = await x.callback(idp.authorize(a.authUrl, claims()));
    assert.equal(cb.status, 200);
    assert.match(cb.text, /You can go back to Scacelith/);
    assert.doesNotMatch(cb.text, /sct_|sso_|mfa_/);
    assert.match(cb.headers['content-security-policy'], /default-src 'none'/);
    assert.equal(idp.state.tokenCalls[0].code_verifier.length, 43);

    const wrong = await x.poll(a, crypto.randomBytes(32).toString('base64url'));
    assert.deepEqual([wrong.status, wrong.json.error], [403, 'invalid_verifier'], 'the attempt id alone is useless');
    const r = await x.poll(a);
    assert.equal(r.status, 200);
    assert.equal(r.json.needsUsername, true);
    assert.match(r.json.ssoTicket, /^sso_/);
    assert.equal(r.json.suggestedUsername, 'Magnus');
    assert.equal((await x.poll(a)).status, 410, 'the result is delivered once');

    await s.createUser({ username: 'Magnus', email: 'other@example.com' });
    let c = await s.request('POST', '/api/v1/auth/sso/complete', { body: { ssoTicket: r.json.ssoTicket, username: 'magnus' } });
    assert.deepEqual([c.status, c.json.error], [409, 'username_taken']);
    c = await s.request('POST', '/api/v1/auth/sso/complete', { body: { ssoTicket: r.json.ssoTicket, username: '_x' } });
    assert.equal(c.json.error, 'invalid_username');
    c = await s.request('POST', '/api/v1/auth/sso/complete', { body: { ssoTicket: r.json.ssoTicket, username: 'MagnusH' } });
    assert.equal(c.status, 200);
    assert.match(c.json.token, /^sct_/);
    assert.deepEqual([c.json.user.username, c.json.user.email, c.json.user.emailVerified, c.json.user.googleLinked, c.json.user.hasPassword],
        ['MagnusH', 'magnus@gmail.com', true, true, false]);
    assert.equal((await s.request('POST', '/api/v1/auth/sso/complete', { body: { ssoTicket: r.json.ssoTicket, username: 'Other1' } })).status, 410);

    // The next sign-in finds the link and logs in directly.
    const again = await x.signIn(claims({ email: 'changed@gmail.com' }));
    assert.equal(again.r.status, 200);
    assert.equal(again.r.json.user.username, 'MagnusH');
    assert.equal(idp.state.jwksFetches, 1, 'JWKS cached per Cache-Control');
});

test('an existing account with the same confirmed address is linked', async (t) => {
    const x = await setup();
    t.after(x.close);
    const u = await x.s.createUser({ username: 'magnus', email: 'magnus@gmail.com' });
    const { r } = await x.signIn(claims());
    assert.equal(r.status, 200);
    assert.equal(r.json.user.id, u.id);
    assert.equal(r.json.user.googleLinked, true);
    assert.deepEqual(x.s.store.sso.find('google', '1098765'), { userId: u.id });
});

test('an unconfirmed local account with that address is refused with a clear page', async (t) => {
    const x = await setup();
    t.after(x.close);
    await x.s.createUser({ username: 'magnus', email: 'magnus@gmail.com', verified: false });
    const { cb, r } = await x.signIn(claims());
    assert.equal(cb.status, 400);
    assert.match(cb.text, /not confirmed/);
    assert.deepEqual([r.status, r.json.error], [409, 'sso_account_unverified']);
    assert.equal(x.s.store.sso.find('google', '1098765'), null);
});

test('Google accounts without a confirmed address are refused', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { r } = await x.signIn(claims({ email_verified: false }));
    assert.deepEqual([r.status, r.json.error], [403, 'sso_email_unverified']);
});

test('an MFA-enabled account still needs its TOTP code', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s } = x;
    await s.createUser({ username: 'magnus', email: 'magnus@gmail.com', password: PW });
    const { token } = await s.login('magnus', PW);
    const setup1 = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } });
    const secret = base32Decode(setup1.json.secret);
    await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: totp(secret, s.now()) } });
    s.now.advance(30000);
    const { r } = await x.signIn(claims());
    assert.equal(r.status, 200);
    assert.equal(r.json.mfaRequired, true);
    assert.equal(r.json.token, undefined);
    const m = await s.request('POST', '/api/v1/auth/login/mfa', { body: { mfaToken: r.json.mfaToken, code: totp(secret, s.now()) } });
    assert.equal(m.status, 200);
    assert.match(m.json.token, /^sct_/);
});

test('ID token checks: signature, audience, issuer, nonce, expiry, algorithm', async (t) => {
    const x = await setup();
    t.after(x.close);
    const other = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
    const cases = {
        signature: ({ claims: c, kid }) => ({ claims: c, signKey: other.privateKey, kid }),
        audience: ({ claims: c, signKey, kid }) => ({ claims: { ...c, aud: 'someone-else', azp: 'someone-else' }, signKey, kid }),
        issuer: ({ claims: c, signKey, kid }) => ({ claims: { ...c, iss: 'https://evil.example' }, signKey, kid }),
        nonce: ({ claims: c, signKey, kid }) => ({ claims: { ...c, nonce: 'x'.repeat(43) }, signKey, kid }),
        expired: ({ claims: c, signKey, kid }) => ({ claims: { ...c, exp: c.iat - 3600, iat: c.iat - 7200 }, signKey, kid }),
        future: ({ claims: c, signKey, kid }) => ({ claims: { ...c, iat: c.iat + 3600 }, signKey, kid }),
        unknownKid: ({ claims: c, signKey }) => ({ claims: c, signKey, kid: 'nope' }),
        algNone: ({ claims: c, signKey, kid }) => ({ claims: c, signKey, kid, alg: 'none' }),
    };
    for (const [name, tamper] of Object.entries(cases)) {
        x.idp.state.tamper = tamper;
        const { cb, r } = await x.signIn(claims());
        assert.equal(cb.status, 400, name);
        assert.match(cb.text, /could not be completed/, name);
        assert.deepEqual([r.status, r.json.error], [502, 'sso_failed'], name);
    }
    x.idp.state.tamper = null;
    assert.equal((await x.signIn(claims())).r.json.needsUsername, true);
    x.s.auth.events.flush();
    assert.ok(x.s.store._raw.securityEvents.filter((e) => e.kind === 'sso_failed').length >= 8);
});

test('key rotation: an unknown key id refreshes the JWKS', async (t) => {
    const x = await setup();
    t.after(x.close);
    await x.signIn(claims());
    assert.equal(x.idp.state.jwksFetches, 1);
    x.idp.rotateKey('k2');
    x.s.now.advance(61000);
    const { r } = await x.signIn(claims({ sub: '222', email: 'other@gmail.com' }));
    assert.equal(r.json.needsUsername, true);
    assert.equal(x.idp.state.jwksFetches, 2);
});

test('state is single use; cancelled and expired attempts', async (t) => {
    const x = await setup();
    t.after(x.close);
    const a = await x.start();
    const q = x.idp.authorize(a.authUrl, claims());
    assert.equal((await x.callback(q)).status, 200);
    const replay = await x.callback(q);
    assert.equal(replay.status, 400);
    assert.match(replay.text, /expired or was already completed/);
    assert.equal((await x.callback({ code: 'x', state: 'short' })).status, 400);

    const b = await x.start();
    const st = new URL(b.authUrl).searchParams.get('state');
    const cancelled = await x.callback({ error: 'access_denied', state: st });
    assert.match(cancelled.text, /cancelled/);
    const r = await x.poll(b);
    assert.deepEqual([r.status, r.json.error], [409, 'sso_cancelled']);

    const c = await x.start();
    x.s.now.advance(10 * 60000 + 1);
    assert.equal((await x.poll(c)).status, 410);
    const late = await x.callback(x.idp.authorize(c.authUrl, claims()));
    assert.equal(late.status, 400);
    assert.equal((await x.s.request('POST', '/api/v1/auth/sso/google/poll', { body: { attemptId: 'sso_' + 'z'.repeat(43), codeVerifier: 'v'.repeat(43) } })).status, 410);
});

test('banned accounts and closed registration', async (t) => {
    const x = await setup({ REGISTRATION: 'closed' });
    t.after(x.close);
    const u = await x.s.createUser({ username: 'magnus', email: 'magnus@gmail.com' });
    x.s.store.sanctions.create({ userId: u.id, kind: 'ban', startsAt: x.s.now() - 1, endsAt: x.s.now() + 1000 });
    let { r } = await x.signIn(claims());
    assert.deepEqual([r.status, r.json.error], [403, 'banned']);
    ({ r } = await x.signIn(claims({ sub: '999', email: 'new@gmail.com' })));
    assert.deepEqual([r.status, r.json.error], [403, 'registration_closed']);
});

test('start and poll validate their input', async (t) => {
    const x = await setup();
    t.after(x.close);
    assert.equal((await x.s.request('POST', '/api/v1/auth/sso/google/start', { body: { codeChallenge: 'short' } })).status, 400);
    assert.equal((await x.s.request('POST', '/api/v1/auth/sso/google/start', { body: {} })).status, 400);
    const a = await x.start();
    assert.equal((await x.s.request('POST', '/api/v1/auth/sso/google/poll', { body: { attemptId: a.attemptId, codeVerifier: 'short' } })).status, 400);
});
