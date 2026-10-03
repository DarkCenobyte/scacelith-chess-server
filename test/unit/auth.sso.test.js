// Google sign-in (auth/sso.js, http/routes/sso.js; docs/API.md "Google sign-in"): Google sends the
// browser back to the game's own listener (http://127.0.0.1:<port>/oauth2/google/<the server's
// origin tag>), the game posts the code to finish, and an account that already has the Google
// address is linked only after its password, and its second factor when on, typed in the game.

import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import net from 'node:net';
import { createOidcClient, ssoOriginTag } from '../../src/auth/oidc.js';
import { createMailer } from '../../src/mail/index.js';
import { dataOf } from '../../src/auth/tokens.js';
import { sha256Hex } from '../../src/security/keys.js';
import { createPasswordHasher } from '../../src/security/password.js';
import { solvePow } from '../../src/security/pow.js';
import { base32Decode, totp } from '../../src/security/totp.js';
import { capturedLogs, linkIn, startTestServer } from './helpers/auth-fakes.js';
import { startFakeOidc } from './helpers/auth-oidc.js';
import { startReal } from './helpers/real-auth.js';

const CLIENT_ID = '1234-abc.apps.googleusercontent.com';
const CLIENT_SECRET = 'GOCSPX-test-secret';
const PW = 'correct horse battery';
const PORT = 50123;
const START = '/api/v1/auth/sso/google/start';
const FINISH = '/api/v1/auth/sso/google/finish';
const LINK = '/api/v1/auth/sso/google/link';
const COMPLETE = '/api/v1/auth/sso/complete';
const MFA = '/api/v1/auth/login/mfa';
const FORM = 'application/x-www-form-urlencoded';
const ATTACKER = '203.0.113.7', VICTIM = '198.51.100.9';
const SSO_ENV = { SSO_GOOGLE_ENABLED: '1', GOOGLE_CLIENT_ID: CLIENT_ID, GOOGLE_CLIENT_SECRET: CLIENT_SECRET };
// The test servers' origin (auth-fakes.js TEST_DEFAULTS: chess.example.org, API_PORT 8443).
const TAG = ssoOriginTag('chess.example.org:8443');
const REDIRECT = `http://127.0.0.1:${PORT}/oauth2/google/${TAG}`;

function pkce() {
    const verifier = crypto.randomBytes(32).toString('base64url');
    return { verifier, challenge: crypto.createHash('sha256').update(verifier).digest('base64url') };
}

/**
 * A test server with Google sign-in and a fake Google. `shared`: another setup() whose provider,
 * store and clock this server uses (one server, another configuration). `mailer`: as for
 * startTestServer.
 */
async function setup(env = {}, shared = null, { mailer } = {}) {
    let s = null;
    // The provider signs with the server's (fake) clock, read once the server exists.
    const idp = shared ? shared.idp : await startFakeOidc({ clientId: CLIENT_ID, clientSecret: CLIENT_SECRET, now: () => s.now() });
    s = await startTestServer({ env: { ...SSO_ENV, ...env }, oidcEndpoints: idp.endpoints, mailer,
        ...(shared ? { store: shared.s.store, now: shared.s.now } : {}) });
    const post = (p, body, opts = {}) => s.request('POST', p, { body, ...opts });
    /** The game's start, its listener on `port`. */
    async function start({ port = PORT, ip } = {}) {
        const p = pkce();
        const r = await post(START, { codeChallenge: p.challenge, redirectPort: port }, { ip });
        assert.equal(r.status, 200, r.text);
        return { ...p, ...r.json, port };
    }
    /** What the game posts once its listener got Google's redirect query `q` ({ code, state, iss }). */
    const finish = (a, q, over = {}, opts = {}) => post(FINISH, { attemptId: a.attemptId, codeVerifier: a.verifier, state: q.state, code: q.code, iss: q.iss, ...over }, opts);
    /** start, the consent at Google, finish. */
    async function signIn(c, opts = {}) {
        const a = await start(opts);
        const q = idp.authorize(a.authUrl, c);
        return { a, q, r: await finish(a, q, {}, opts) };
    }
    const link = (linkTicket, password, over = {}, opts = {}) => post(LINK, { linkTicket, password, ...over }, opts);
    const close = async () => { await s.close(); if (!shared) await idp.close(); };
    return { s, idp, post, start, finish, signIn, link, close };
}

const claims = (over = {}) => ({ sub: '1098765', email: 'Magnus@Gmail.com', email_verified: true, name: 'Magnus Hansen', given_name: 'Magnus', ...over });
const events = (s, kind) => { s.auth.events.flush(); return s.store._raw.securityEvents.filter((e) => e.kind === kind); };
const detail = (e) => JSON.parse(e.detail);
const googleLink = (s, sub = '1098765') => s.store.sso.find('google', sub);
const liveSessions = (s, userId) => [...s.store._raw.sessions.values()].filter((x) => x.userId === userId && !x.revokedAt).length;

/** Turns two-step verification on for a password account; returns its secret. */
async function enableMfa(s, username, password = PW) {
    const { token } = await s.login(username, password);
    const st = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password } });
    const secret = base32Decode(st.json.secret);
    const en = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: totp(secret, s.now()) } });
    assert.equal(en.status, 200, en.text);
    s.now.advance(30000);       // the next code is of a step not used yet
    return secret;
}

// ---- the loopback flow (decision 36a) ------------------------------------------------------------

test('disabled unless SSO_GOOGLE_ENABLED; the poll route and the callback page of the browser flow are gone', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const tok = 'sso_' + 'a'.repeat(43);
    for (const [p, body] of [[START, { codeChallenge: 'a'.repeat(43), redirectPort: PORT }],
        [FINISH, { attemptId: tok, codeVerifier: 'v'.repeat(43), state: 's'.repeat(43), code: 'c' }], [LINK, { linkTicket: tok, password: PW }]]) {
        const r = await s.request('POST', p, { body });
        assert.deepEqual([r.status, r.json.error], [404, 'sso_disabled'], p);
    }
    const x = await setup();
    t.after(x.close);
    assert.equal((await x.post('/api/v1/auth/sso/google/poll', { attemptId: tok, codeVerifier: 'v'.repeat(43) })).status, 404);
    assert.equal((await x.s.request('GET', '/auth/sso/google/callback?code=x&state=y')).status, 404);
});

test('start: Google returns to the posted loopback port under this server\'s origin tag; the answer carries the state', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s } = x;
    assert.deepEqual([s.config.ssoOrigin, s.config.ssoRedirectTag], ['chess.example.org:8443', TAG]);
    const p = pkce();
    const r = await x.post(START, { codeChallenge: p.challenge, redirectPort: PORT });
    assert.equal(r.status, 200, r.text);
    assert.deepEqual(Object.keys(r.json).sort(), ['attemptId', 'authUrl', 'expiresIn', 'state']);
    assert.match(r.json.attemptId, /^sso_[A-Za-z0-9_-]{43}$/);
    assert.match(r.json.state, /^[A-Za-z0-9_-]{43}$/);
    assert.equal(r.json.expiresIn, 600);
    const u = new URL(r.json.authUrl);
    assert.ok(r.json.authUrl.startsWith('https://accounts.google.com/o/oauth2/v2/auth?'));
    const q = u.searchParams;
    for (const k of ['client_id', 'redirect_uri', 'response_type', 'scope', 'state', 'nonce', 'code_challenge', 'code_challenge_method', 'prompt']) {
        assert.equal(q.getAll(k).length, 1, k);
    }
    assert.equal(q.get('client_id'), CLIENT_ID);
    assert.equal(q.get('redirect_uri'), REDIRECT);
    assert.equal(q.get('response_type'), 'code');
    assert.equal(q.get('scope'), 'openid email profile');
    assert.equal(q.get('state'), r.json.state);
    assert.match(q.get('nonce'), /^[A-Za-z0-9_-]{43}$/);
    assert.equal(q.get('code_challenge_method'), 'S256');
    assert.match(q.get('code_challenge'), /^[A-Za-z0-9_-]{43}$/);
    assert.notEqual(q.get('code_challenge'), p.challenge, 'the server uses its own PKCE pair with Google');
    assert.equal(q.get('prompt'), 'select_account');
    for (const port of [1024, 65535]) {
        const a = await x.start({ port });
        assert.equal(new URL(a.authUrl).searchParams.get('redirect_uri'), `http://127.0.0.1:${port}/oauth2/google/${TAG}`);
    }
});

test('the origin tags of the contract\'s vectors; the provider client takes only a loopback redirect URI', async () => {
    for (const [origin, tag] of [['play.scacelith.example:443', 'IhcScoV7eDOzTEcSnqPUPt'], ['localhost:8443', 'TFGx7zQ_8QlGZW5zpqznCr'],
        ['[::1]:8443', 'XToJm0DG5PjciEVmZa9Cho'], ['127.0.0.1:50443', '3r653wM5ZjYsHcAJljmCwY']]) {
        assert.equal(ssoOriginTag(origin), tag, origin);
    }
    let requests = 0;
    const oidc = createOidcClient({ clientId: CLIENT_ID, clientSecret: CLIENT_SECRET, request: async () => { requests++; return { status: 500, body: '' }; } });
    const url = (redirectUri) => oidc.authorizationUrl({ state: 's'.repeat(43), nonce: 'n'.repeat(43), codeChallenge: 'c'.repeat(43), redirectUri });
    assert.equal(new URL(url(REDIRECT)).searchParams.get('redirect_uri'), REDIRECT);
    const tagged = (hostPort, path = `/oauth2/google/${TAG}`, scheme = 'http') => `${scheme}://${hostPort}${path}`;
    for (const bad of [tagged(`localhost:${PORT}`), tagged(`[::1]:${PORT}`), tagged(`127.0.0.1:${PORT}`, undefined, 'https'),
        tagged('127.0.0.1:80'), tagged('127.0.0.1:1023'), tagged('127.0.0.1:65536'), tagged(`127.0.0.1:${PORT}`, `/oauth2/google/${TAG}x`),
        tagged(`127.0.0.1:${PORT}`, `/oauth2/google/${TAG}/`), tagged(`127.0.0.1:${PORT}`, '/callback'), undefined]) {
        assert.throws(() => url(bad), (e) => e.reason === 'bad_redirect_uri', String(bad));
        await assert.rejects(oidc.exchangeCode('code', 'v'.repeat(43), bad), (e) => e.reason === 'bad_redirect_uri', String(bad));
    }
    assert.equal(requests, 0, 'nothing sent to Google');
});

test('start and finish validate their body', async (t) => {
    const x = await setup();
    t.after(x.close);
    const challenge = pkce().challenge;
    for (const redirectPort of [undefined, null, 0, 80, 1023, 65536, 1.5, '50123']) {
        const r = await x.post(START, { codeChallenge: challenge, redirectPort });
        assert.deepEqual([r.status, r.json.error], [400, 'invalid_request'], String(redirectPort));
    }
    assert.equal((await x.post(START, { codeChallenge: 'short', redirectPort: PORT })).status, 400);
    for (const extra of [{ codeChallengeMethod: 'S256' }, { redirectUri: REDIRECT }]) {
        assert.equal((await x.post(START, { codeChallenge: challenge, redirectPort: PORT, ...extra })).status, 400, Object.keys(extra)[0]);
    }
    const a = await x.start();
    const good = { attemptId: a.attemptId, codeVerifier: a.verifier, state: a.state, code: 'abc' };
    for (const bad of [{ codeVerifier: 'short' }, { state: a.state.slice(1) }, { state: `${a.state.slice(1)}!` }, { code: '' }, { code: 'a b' },
        { code: 'x'.repeat(2049) }, { iss: '' }, { iss: 'x'.repeat(257) }, { clientLabel: 'x'.repeat(65) }, { redirectUri: REDIRECT }]) {
        const r = await x.post(FINISH, { ...good, ...bad });
        assert.equal(r.status, 400, JSON.stringify(bad));
    }
    assert.equal((await x.post(LINK, { linkTicket: 'sso_x', password: PW, extra: 1 })).status, 400);
});

test('first Google sign-in: finish with the game\'s verifier, then a username; the next sign-in finds the link', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s, idp } = x;
    const a = await x.start();
    const q = idp.authorize(a.authUrl, claims());
    assert.equal(q.iss, 'https://accounts.google.com');

    const wrong = await x.finish(a, q, { codeVerifier: pkce().verifier });
    assert.deepEqual([wrong.status, wrong.json.error], [403, 'invalid_verifier'], 'the attempt id alone is useless');
    assert.equal(idp.state.tokenCalls.length, 0, 'nothing exchanged, the attempt kept');
    const r = await x.finish(a, q, { clientLabel: 'Scacelith (test)' });
    assert.equal(r.status, 200, r.text);
    assert.deepEqual(Object.keys(r.json).sort(), ['needsUsername', 'ssoTicket', 'suggestedUsername']);
    assert.match(r.json.ssoTicket, /^sso_[A-Za-z0-9_-]{43}$/);
    assert.equal(r.json.suggestedUsername, 'Magnus');
    const call = idp.state.tokenCalls[0];
    assert.equal(call.redirect_uri, REDIRECT, 'the redirect URI of the attempt, byte for byte');
    assert.equal(call.code_verifier.length, 43);
    assert.notEqual(call.code_verifier, a.verifier, 'the server\'s own verifier');
    assert.equal((await x.finish(a, q)).status, 410, 'an attempt finishes once');

    await s.createUser({ username: 'Magnus', email: 'other@example.com' });
    let c = await x.post(COMPLETE, { ssoTicket: r.json.ssoTicket, username: 'magnus' });
    assert.deepEqual([c.status, c.json.error], [409, 'username_taken']);
    c = await x.post(COMPLETE, { ssoTicket: r.json.ssoTicket, username: '_x' });
    assert.equal(c.json.error, 'invalid_username');
    // A username held by the pending signup of another address is taken too.
    const signup = { username: 'Magnus_C', email: 'carlsen@example.com', password: 'ivory rook takes e5' };
    assert.equal((await x.post('/api/v1/auth/register', signup)).status, 202);
    c = await x.post(COMPLETE, { ssoTicket: r.json.ssoTicket, username: 'magnus_c' });
    assert.deepEqual([c.status, c.json.error], [409, 'username_taken']);
    c = await x.post(COMPLETE, { ssoTicket: r.json.ssoTicket, username: 'MagnusH' });
    assert.equal(c.status, 200);
    assert.match(c.json.token, /^sct_/);
    assert.deepEqual([c.json.user.username, c.json.user.email, c.json.user.emailVerified, c.json.user.googleLinked, c.json.user.hasPassword],
        ['MagnusH', 'magnus@gmail.com', true, true, false]);
    assert.equal((await x.post(COMPLETE, { ssoTicket: r.json.ssoTicket, username: 'Other1' })).status, 410);

    // The next sign-in finds the link (by the Google subject, whatever the address now) and logs in.
    const again = await x.signIn(claims({ email: 'changed@gmail.com' }));
    assert.equal(again.r.status, 200, again.r.text);
    assert.equal(again.r.json.user.username, 'MagnusH');
    assert.equal(detail(events(s, 'login').at(-1)).method, 'google');
    assert.equal(events(s, 'sso_login').length, 1);
    assert.equal(idp.state.jwksFetches, 1, 'JWKS cached per Cache-Control');
});

test('a linked account with two-step verification gets the MFA step, then a google+totp session', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s } = x;
    const u = await s.createUser({ username: 'magnus', email: 'magnus@gmail.com', password: PW });
    const secret = await enableMfa(s, 'magnus');
    s.store.sso.link(u.id, 'google', '1098765', 'magnus@gmail.com');
    const { r } = await x.signIn(claims());
    assert.equal(r.status, 200, r.text);
    assert.deepEqual([r.json.mfaRequired, r.json.token], [true, undefined]);
    const m = await x.post(MFA, { mfaToken: r.json.mfaToken, code: totp(secret, s.now()) });
    assert.equal(m.status, 200, m.text);
    assert.match(m.json.token, /^sct_/);
    assert.equal(detail(events(s, 'login').at(-1)).method, 'google+totp');
});

test('finish: a wrong state or issuer fails (502) and uses the attempt; expired, used, unknown and stale attempts are 410', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s, idp } = x;
    let a = await x.start();
    let q = idp.authorize(a.authUrl, claims());
    let r = await x.finish(a, q, { state: pkce().verifier });
    assert.deepEqual([r.status, r.json.error], [502, 'sso_failed']);
    assert.equal((await x.finish(a, q)).status, 410, 'the attempt was used');
    a = await x.start();
    q = idp.authorize(a.authUrl, claims());
    r = await x.finish(a, q, { iss: 'https://evil.example' });
    assert.deepEqual([r.status, r.json.error], [502, 'sso_failed']);
    assert.equal(idp.state.tokenCalls.length, 0, 'neither was exchanged');
    assert.deepEqual(events(s, 'sso_failed').map((e) => detail(e).reason), ['state_mismatch', 'bad_iss']);
    // No iss in the redirect (it is optional): accepted.
    a = await x.start();
    q = idp.authorize(a.authUrl, claims());
    r = await x.finish(a, { ...q, iss: undefined });
    assert.equal(r.json.needsUsername, true, r.text);

    a = await x.start();
    q = idp.authorize(a.authUrl, claims());
    s.now.advance(10 * 60000 + 1);
    assert.equal((await x.finish(a, q)).status, 410, 'expired');
    r = await x.post(FINISH, { attemptId: 'sso_' + 'z'.repeat(43), codeVerifier: 'v'.repeat(43), state: 's'.repeat(43), code: 'c' });
    assert.equal(r.status, 410, 'unknown');
    // An attempt row of the browser-callback flow (no stateHash, redirectUri or verifier).
    const stale = 'sso_' + crypto.randomBytes(32).toString('base64url');
    const p = pkce();
    s.store.tokens.create({ kind: 'sso_attempt', tokenHash: sha256Hex(stale), userId: null, data: { challenge: p.challenge, status: 'pending' }, expiresAt: s.now() + 600000 });
    r = await x.post(FINISH, { attemptId: stale, codeVerifier: p.verifier, state: 's'.repeat(43), code: 'c' });
    assert.equal(r.status, 410, 'stale');
});

test('a code minted for one attempt fails through another (502) and opens no session', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s, idp } = x;
    const u = await s.createUser({ username: 'magnus', email: 'magnus@gmail.com', password: PW });
    s.store.sso.link(u.id, 'google', '1098765', 'magnus@gmail.com');
    for (const port of [PORT, PORT + 1]) {
        const a = await x.start();
        const b = await x.start({ port });
        const qa = idp.authorize(a.authUrl, claims());
        const r = await x.finish(b, { ...qa, state: b.state });
        assert.deepEqual([r.status, r.json.error], [502, 'sso_failed'], `B on port ${port}`);
    }
    assert.equal(liveSessions(s, u.id), 0);
});

test('cross-device phishing: whoever started the attempt never gets the sign-in of the browser they sent to Google', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s, idp } = x;
    const victim = await s.createUser({ username: 'victor', email: 'victim@gmail.com', password: PW });
    s.store.sso.link(victim.id, 'google', 'v-sub', 'victim@gmail.com');
    // The attacker starts from a script and sends the genuine Google link to the victim.
    const a = await x.start({ ip: ATTACKER });
    // The victim signs in at Google; her browser goes to her own 127.0.0.1, with the code.
    const q = idp.authorize(a.authUrl, { sub: 'v-sub', email: 'victim@gmail.com', email_verified: true });
    assert.ok(new URL(a.authUrl).searchParams.get('redirect_uri').startsWith('http://127.0.0.1:'));
    // The routes that delivered the result to the attacker are gone.
    assert.equal((await x.post('/api/v1/auth/sso/google/poll', { attemptId: a.attemptId, codeVerifier: a.verifier }, { ip: ATTACKER })).status, 404);
    assert.equal((await s.request('GET', `/auth/sso/google/callback?${new URLSearchParams({ code: q.code, state: q.state })}`, { ip: VICTIM })).status, 404);
    // Without the code, the attacker's finish fails and uses the attempt.
    const r = await x.finish(a, { state: a.state, code: 'guessed-code' }, {}, { ip: ATTACKER });
    assert.deepEqual([r.status, r.json.error], [502, 'sso_failed']);
    assert.equal((await x.finish(a, q, {}, { ip: ATTACKER })).status, 410);
    assert.equal(liveSessions(s, victim.id), 0);
    // A first sign-in (no account yet) gives the attacker no ticket either.
    const b = await x.start({ ip: ATTACKER });
    idp.authorize(b.authUrl, { sub: 'new-sub', email: 'newcomer@gmail.com', email_verified: true });
    assert.equal((await x.finish(b, { state: b.state, code: 'guessed-code' }, {}, { ip: ATTACKER })).status, 502);
});

test('a relay through another server: its tag differs, and a code authorized for a rewritten redirect URI fails at the exchange', async (t) => {
    const x = await setup();
    t.after(x.close);
    const evil = await setup({ SERVER_PUBLIC_HOST: 'evil.example' });
    t.after(evil.close);
    assert.equal(evil.s.config.ssoRedirectTag, ssoOriginTag('evil.example:8443'));
    assert.notEqual(evil.s.config.ssoRedirectTag, TAG);
    // The hostile server forwards the game's start (its port) to the official one...
    const a = await x.start();
    const u = new URL(a.authUrl);
    assert.equal(u.searchParams.get('redirect_uri'), REDIRECT, 'the official server only hands out its own tag');
    // ... and rewrites the redirect URI to its own tag, which the game would accept.
    u.searchParams.set('redirect_uri', `http://127.0.0.1:${PORT}/oauth2/google/${evil.s.config.ssoRedirectTag}`);
    const q = x.idp.authorize(u.toString(), claims());
    const r = await x.finish(a, q);
    assert.deepEqual([r.status, r.json.error], [502, 'sso_failed']);
    assert.equal(x.idp.state.tokenCalls.at(-1).redirect_uri, REDIRECT, 'the exchange sends the stored redirect URI');
});

test('no secret leaks: a failed sign-in shows no provider text, code or token; every 410 has the same body', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s, idp } = x;
    const from = capturedLogs.length;
    const a = await x.start();
    const q = idp.authorize(a.authUrl, claims());
    // A code the provider refuses (invalid_grant), then an ID token with a bad signature.
    const refused = await x.finish(a, { ...q, code: 'never-issued-code' });
    const b = await x.start();
    const qb = idp.authorize(b.authUrl, claims());
    const other = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
    idp.state.tamper = ({ claims: c, kid }) => ({ claims: c, signKey: other.privateKey, kid });
    const forged = await x.finish(b, qb);
    idp.state.tamper = null;
    for (const r of [refused, forged]) {
        assert.equal(r.status, 502);
        assert.deepEqual(r.json, { error: 'sso_failed', message: 'Google sign-in could not be completed.' });
    }
    const secrets = [q.code, qb.code, 'never-issued-code', a.state, b.state, a.attemptId, b.attemptId, a.verifier, 'invalid_grant'];
    for (const v of secrets) {
        assert.ok(!refused.text.includes(v) && !forged.text.includes(v), v);
    }
    // Neither in the log (the token endpoint's error code, 60 characters at most, is).
    const logged = capturedLogs.slice(from).join('\n');
    for (const v of [q.code, qb.code, 'never-issued-code', a.state, b.state, a.verifier, b.verifier]) assert.ok(!logged.includes(v), `logged: ${v}`);

    const c = await x.start();
    s.now.advance(10 * 60000 + 1);
    const answers = [
        await x.finish(a, q),
        await x.finish(c, idp.authorize(c.authUrl, claims())),
        await x.post(FINISH, { attemptId: 'sso_' + 'z'.repeat(43), codeVerifier: 'v'.repeat(43), state: 's'.repeat(43), code: 'c' }),
        await x.post(FINISH, { attemptId: 'nope', codeVerifier: 'v'.repeat(43), state: 's'.repeat(43), code: 'c' }),
        await x.link('sso_' + 'z'.repeat(43), PW),
        await x.link('nope', PW),
        await x.post(COMPLETE, { ssoTicket: 'sso_' + 'z'.repeat(43), username: 'Someone' }),
    ];
    for (const r of answers) assert.deepEqual([r.status, r.json], [410, { error: 'sso_expired', message: 'This sign-in has expired; start again from Scacelith.' }]);
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
        const { r } = await x.signIn(claims());
        assert.deepEqual([r.status, r.json.error], [502, 'sso_failed'], name);
    }
    x.idp.state.tamper = null;
    assert.equal((await x.signIn(claims())).r.json.needsUsername, true);
    assert.ok(events(x.s, 'sso_failed').length >= 8);
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

test('banned accounts, closed registration and Google addresses Google has not confirmed', async (t) => {
    const x = await setup({ REGISTRATION: 'closed' });
    t.after(x.close);
    const u = await x.s.createUser({ username: 'magnus', email: 'magnus@gmail.com' });
    x.s.store.sso.link(u.id, 'google', '1098765', 'magnus@gmail.com');
    x.s.store.sanctions.create({ userId: u.id, kind: 'ban', startsAt: x.s.now() - 1, endsAt: x.s.now() + 1000 });
    let { r } = await x.signIn(claims());
    assert.deepEqual([r.status, r.json.error], [403, 'banned']);
    ({ r } = await x.signIn(claims({ sub: '999', email: 'new@gmail.com' })));
    assert.deepEqual([r.status, r.json.error], [403, 'registration_closed']);
    ({ r } = await x.signIn(claims({ sub: '998', email: 'unconfirmed@gmail.com', email_verified: false })));
    assert.deepEqual([r.status, r.json.error], [403, 'sso_email_unverified']);
});

// ---- an account with the Google address: its password first (decision 35A) ---------------------

for (const verification of ['1', '0']) {
    const env = { REQUIRE_EMAIL_VERIFICATION: verification };
    const mode = `REQUIRE_EMAIL_VERIFICATION=${verification === '1'}`;

    test(`${mode}: an account with the Google address is linked only after its password, typed in the game`, async (t) => {
        const x = await setup(env);
        t.after(x.close);
        const { s } = x;
        const u = await s.createUser({ username: 'Magnus', email: 'magnus@gmail.com', password: PW, verified: false });
        const { r } = await x.signIn(claims(), { ip: VICTIM });
        assert.equal(r.status, 200, r.text);
        assert.deepEqual(Object.keys(r.json).sort(), ['expiresIn', 'linkTicket', 'needsPassword', 'username']);
        assert.deepEqual([r.json.needsPassword, r.json.username, r.json.expiresIn], [true, 'Magnus', 600]);
        assert.match(r.json.linkTicket, /^sso_[A-Za-z0-9_-]{43}$/);
        assert.equal(googleLink(s), null, 'nothing linked before the password');
        assert.equal(liveSessions(s, u.id), 0);
        assert.deepEqual(events(s, 'sso_link_required').map((e) => [e.userId, e.ip]), [[u.id, null]]);
        // A wrong password: 401, the ticket stays, the account's login counter counts it.
        let l = await x.link(r.json.linkTicket, 'wrong password 1', {}, { ip: VICTIM });
        assert.deepEqual([l.status, l.json.error], [401, 'invalid_credentials']);
        assert.equal(s.auth._svc.login.failures.failures('l:magnus'), 1);
        assert.equal(detail(events(s, 'login_failed').at(-1)).method, 'google_link');
        assert.equal(googleLink(s), null);
        l = await x.link(r.json.linkTicket, PW, { clientLabel: 'Scacelith (test)' }, { ip: VICTIM });
        assert.equal(l.status, 200, l.text);
        assert.match(l.json.token, /^sct_/);
        assert.deepEqual([l.json.user.id, l.json.user.googleLinked, l.json.user.emailVerified], [u.id, true, true]);
        assert.deepEqual(googleLink(s), { userId: u.id });
        assert.equal(s.auth._svc.login.failures.failures('l:magnus'), 0, 'the right password resets the counter');
        const linked = events(s, 'sso_linked');
        assert.deepEqual(linked.map((e) => [e.userId, e.ip, detail(e)]), [[u.id, VICTIM, { provider: 'google', method: 'password' }]]);
        assert.equal(detail(events(s, 'login').at(-1)).method, 'google+password');
        assert.equal((await x.link(r.json.linkTicket, PW)).status, 410, 'the ticket is used');
        // The next Google sign-in needs no password.
        const again = await x.signIn(claims());
        assert.equal(again.r.status, 200, again.r.text);
        assert.equal(again.r.json.user.id, u.id);
    });

    test(`${mode}: the 5th wrong password ends the ticket, ten at once cost at most 5 checks, and /auth/login shares the lockout`, async (t) => {
        const x = await setup(env);
        t.after(x.close);
        const { s } = x;
        await s.createUser({ username: 'Magnus', email: 'magnus@gmail.com', password: PW });
        let { r } = await x.signIn(claims());
        for (let i = 1; i <= 4; i++) assert.equal((await x.link(r.json.linkTicket, `wrong password ${i}`)).status, 401, `try ${i}`);
        let l = await x.link(r.json.linkTicket, 'wrong password 5');
        assert.deepEqual([l.status, l.json.error], [410, 'sso_expired']);
        assert.equal((await x.link(r.json.linkTicket, PW)).status, 410, 'dead, even with the right password');
        assert.equal(googleLink(s), null);
        // AUTH_FAILURES_PER_ACCOUNT (5) link failures: the password login waits as well.
        l = await x.post('/api/v1/auth/login', { login: 'magnus', password: PW });
        assert.deepEqual([l.status, l.json.error], [429, 'too_many_attempts']);
        s.now.advance(2001);
        await s.login('magnus', PW);

        ({ r } = await x.signIn(claims()));
        const hasher = s.auth._svc.hasher;
        const check = hasher.checkPassword;
        let checks = 0;
        hasher.checkPassword = (...args) => { checks++; return check.apply(hasher, args); };
        let all;
        try {
            all = await Promise.all(Array.from({ length: 10 }, (_, i) => x.link(r.json.linkTicket, `parallel wrong ${i}`)));
        } finally { hasher.checkPassword = check; }
        const statuses = all.map((a) => a.status);
        assert.ok(checks <= 5, `${checks} password checks`);
        // Past the ticket's tries 410, or first 429 once the failures counted reach the account's wait.
        assert.ok(statuses.every((st) => st === 401 || st === 410 || st === 429), String(statuses));
        assert.ok(statuses.filter((st) => st === 401).length <= 4, String(statuses));
        assert.equal((await x.link(r.json.linkTicket, PW)).status, 410);
        assert.equal(googleLink(s), null);
    });

    test(`${mode}: a squatter's account with the address is not opened by the address owner's Google`, async (t) => {
        // Registered without e-mail confirmation (or before the operator turned it on).
        const off = await setup({ REQUIRE_EMAIL_VERIFICATION: '0' });
        t.after(off.close);
        const reg = await off.post('/api/v1/auth/register', { username: 'squatter', email: 'magnus@gmail.com', password: 'ivory rook takes e5' });
        assert.equal(reg.status, 201, reg.text);
        const x = verification === '0' ? off : await setup(env, off);
        if (x !== off) t.after(x.close);
        const squatter = x.s.store.users.byUsername('squatter');
        const { r } = await x.signIn(claims());
        assert.deepEqual([r.json.needsPassword, r.json.username], [true, 'squatter']);
        const l = await x.link(r.json.linkTicket, PW);
        assert.deepEqual([l.status, l.json.error], [401, 'invalid_credentials']);
        assert.equal(googleLink(x.s), null);
        assert.equal(liveSessions(x.s, squatter.id), 0);
    });

    test(`${mode}: with two-step verification, the link is stored only once the code passes`, async (t) => {
        const x = await setup(env);
        t.after(x.close);
        const { s } = x;
        const u = await s.createUser({ username: 'Magnus', email: 'magnus@gmail.com', password: PW });
        const secret = await enableMfa(s, 'Magnus');
        const step = async () => {
            const { r } = await x.signIn(claims());
            const l = await x.link(r.json.linkTicket, PW, {}, { ip: VICTIM });
            assert.equal(l.status, 200, l.text);
            assert.deepEqual([l.json.mfaRequired, l.json.token, l.json.expiresIn], [true, undefined, 300]);
            assert.equal(googleLink(s), null, 'not before the code');
            return l.json.mfaToken;
        };
        let m = await x.post(MFA, { mfaToken: await step(), code: totp(secret, s.now() - 3600000) });
        assert.deepEqual([m.status, m.json.error], [401, 'invalid_code']);
        assert.equal(googleLink(s), null);
        const late = await step();
        s.now.advance(301000);
        m = await x.post(MFA, { mfaToken: late, code: totp(secret, s.now()) });
        assert.deepEqual([m.status, m.json.error], [401, 'invalid_mfa_token']);
        assert.equal(googleLink(s), null);
        // The address changed between the password and the code: 410, no link.
        let token = await step();
        s.store.users.update(u.id, { email: 'elsewhere@example.com' });
        m = await x.post(MFA, { mfaToken: token, code: totp(secret, s.now()) });
        assert.deepEqual([m.status, m.json.error], [410, 'sso_expired']);
        assert.equal(googleLink(s), null);
        s.store.users.update(u.id, { email: 'magnus@gmail.com' });
        s.now.advance(30000);

        token = await step();
        m = await x.post(MFA, { mfaToken: token, code: totp(secret, s.now()) }, { ip: VICTIM });
        assert.equal(m.status, 200, m.text);
        assert.deepEqual([m.json.user.id, m.json.user.googleLinked], [u.id, true]);
        assert.deepEqual(googleLink(s), { userId: u.id });
        assert.equal(detail(events(s, 'login').at(-1)).method, 'google+totp');
        assert.deepEqual(events(s, 'sso_linked').map((e) => [e.ip, detail(e)]), [[VICTIM, { provider: 'google', method: 'password+totp' }]]);
    });

    test(`${mode}: a password change between the password and the code ends the step; a hash the link upgraded still passes`, async (t) => {
        const x = await setup(env);
        t.after(x.close);
        const { s } = x;
        const u = await s.createUser({ username: 'Magnus', email: 'magnus@gmail.com', password: PW });
        const secret = await enableMfa(s, 'Magnus');
        let { r } = await x.signIn(claims());
        let l = await x.link(r.json.linkTicket, PW);
        assert.equal(l.json.mfaRequired, true, l.text);
        s.store.users.update(u.id, { passwordHash: await s.hasher.hash('another password 12') });
        let m = await x.post(MFA, { mfaToken: l.json.mfaToken, code: totp(secret, s.now()) });
        assert.deepEqual([m.status, m.json.error], [401, 'invalid_mfa_token']);
        assert.equal(googleLink(s), null);
        // An outdated hash: the link's check upgrades it, and the step is bound to the new one.
        const old = createPasswordHasher({ scrypt: { logN: 9 }, argon2: false });
        s.store.users.update(u.id, { passwordHash: await old.hash(PW) });
        ({ r } = await x.signIn(claims()));
        l = await x.link(r.json.linkTicket, PW);
        assert.equal(l.json.mfaRequired, true, l.text);
        assert.match(s.store.users.byId(u.id).passwordHash, /^scrypt\$10\$/, 'upgraded');
        m = await x.post(MFA, { mfaToken: l.json.mfaToken, code: totp(secret, s.now()) });
        assert.equal(m.status, 200, m.text);
        assert.deepEqual(googleLink(s), { userId: u.id });
    });

    test(`${mode}: an account that changes after its password, or a Google identity linked elsewhere meanwhile, gets no link`, async (t) => {
        const x = await setup(env);
        t.after(x.close);
        const { s } = x;
        const u = await s.createUser({ username: 'Magnus', email: 'magnus@gmail.com', password: PW });
        const other = await s.createUser({ username: 'other', email: 'other@example.com' });
        const before = s.store.users.byId(u.id);
        const otherHash = await s.hasher.hash('another password 12');
        const consume = s.store.tokens.consume;
        // `change` runs once the password matched, just before the link is stored.
        async function race(change) {
            const { r } = await x.signIn(claims());
            assert.equal(r.json.needsPassword, true, r.text);
            s.store.tokens.consume = (kind, ...rest) => { if (kind === 'sso_link') change(); return consume(kind, ...rest); };
            try { return await x.link(r.json.linkTicket, PW); } finally { s.store.tokens.consume = consume; }
        }
        for (const [name, fields] of [['e-mail', { email: 'new@example.com' }], ['password', { passwordHash: otherHash }],
            ['status', { status: 'deleted' }], ['two-step verification switched on', { mfaEnabled: true }]]) {
            const l = await race(() => s.store.users.update(u.id, fields));
            assert.deepEqual([l.status, l.json.error], [410, 'sso_expired'], name);
            assert.equal(googleLink(s), null, name);
            assert.equal(liveSessions(s, u.id), 0, name);
            s.store.users.update(u.id, { email: before.email, passwordHash: before.passwordHash, status: 'active', mfaEnabled: false });
        }
        const l = await race(() => s.store.sso.link(other.id, 'google', '1098765', 'magnus@gmail.com'));
        assert.deepEqual([l.status, l.json.error], [409, 'sso_already_linked']);
        assert.deepEqual(googleLink(s), { userId: other.id });
        assert.equal(liveSessions(s, u.id), 0);
    });

    test(`${mode}: an account without a usable password (Google-only, bench) is not named: 409 sso_account_exists`, async (t) => {
        const x = await setup(env);
        t.after(x.close);
        const { s } = x;
        await s.createUser({ username: 'NoPassword', email: 'magnus@gmail.com', password: null });
        s.store.users.create({ username: 'bench0001', email: 'bench0001@bench.invalid', passwordHash: '!bench-account-no-password', emailVerified: true });
        for (const c of [claims(), claims({ sub: '77', email: 'bench0001@bench.invalid' })]) {
            const { r } = await x.signIn(c);
            assert.deepEqual([r.status, r.json.error], [409, 'sso_account_exists'], c.email);
            assert.ok(!/nopassword|bench0001/i.test(r.text), r.text);
            assert.equal(googleLink(s, c.sub), null);
        }
    });

    test(`${mode}: a banned account gets no link: 403 banned only after the right password`, async (t) => {
        const x = await setup(env);
        t.after(x.close);
        const { s } = x;
        const u = await s.createUser({ username: 'Magnus', email: 'magnus@gmail.com', password: PW });
        const until = s.now() + 3600000;
        s.store.sanctions.create({ userId: u.id, kind: 'ban', startsAt: s.now() - 1, endsAt: until });
        const { r } = await x.signIn(claims());
        let l = await x.link(r.json.linkTicket, 'wrong password 1');
        assert.deepEqual([l.status, l.json.error], [401, 'invalid_credentials']);
        l = await x.link(r.json.linkTicket, PW);
        assert.deepEqual([l.status, l.json.error, l.json.until], [403, 'banned', until]);
        assert.equal(googleLink(s), null);
    });

    test(`${mode}: the account's wait (429) and a login proof-of-work wave (428) apply to the link step and take none of its tries`, async (t) => {
        const x = await setup({ ...env, POW_LOGIN_BITS: '4' });
        t.after(x.close);
        const { s } = x;
        const u = await s.createUser({ username: 'Magnus', email: 'magnus@gmail.com', password: PW });
        for (let i = 1; i <= 5; i++) await x.post('/api/v1/auth/login', { login: 'magnus', password: `wrong password ${i}` });
        let { r } = await x.signIn(claims());
        for (let i = 0; i < 6; i++) {
            const l = await x.link(r.json.linkTicket, PW);
            assert.deepEqual([l.status, l.json.error], [429, 'too_many_attempts']);
        }
        assert.equal(dataOf(s.store.tokens.get('sso_link', sha256Hex(r.json.linkTicket))).tries, 0);
        s.now.advance(2001);
        assert.equal((await x.link(r.json.linkTicket, PW)).status, 200);
        assert.deepEqual(googleLink(s), { userId: u.id });

        const v = await s.createUser({ username: 'Hikaru', email: 'hikaru@gmail.com', password: PW });
        ({ r } = await x.signIn(claims({ sub: '2002', email: 'hikaru@gmail.com' })));
        s.auth._svc.login.activatePow('test');
        // As the game does: each password first without a proof, then again with the proof solved.
        const withPow = async (password) => {
            const l = await x.link(r.json.linkTicket, password);
            assert.deepEqual([l.status, l.json.error, l.json.pow.bits], [428, 'pow_required', 4]);
            return x.link(r.json.linkTicket, password, { pow: { challenge: l.json.pow.challenge, nonce: solvePow(l.json.pow.challenge, 4) } });
        };
        for (let i = 1; i <= 4; i++) assert.equal((await withPow(`wrong password ${i}`)).status, 401, `try ${i}`);
        const l = await withPow(PW);
        assert.equal(l.status, 200, l.text);
        assert.deepEqual(googleLink(s, '2002'), { userId: v.id });
    });
}

test('REQUIRE_EMAIL_VERIFICATION=true: an address confirmed for someone else\'s account (signup link, e-mail change) is not linked without the password', async (t) => {
    const x = await setup({ REQUIRE_EMAIL_VERIFICATION: '1' });
    t.after(x.close);
    const { s } = x;
    const tokenOf = async (path) => {
        await s.mailer.idle();
        const mail = s.mailer.sent.findLast((m) => new URL(linkIn(m.text) || 'http://x/').pathname === path);
        return new URL(linkIn(mail.text)).searchParams.get('token');
    };
    // A signup with the victim's address (decision 34a): the victim opens the link mailed to her.
    assert.equal((await x.post('/api/v1/auth/register', { username: 'deputy1', email: 'magnus@gmail.com', password: 'deputy password 1' })).status, 202);
    let token = await tokenOf('/verify-email');
    assert.equal((await s.request('POST', '/verify-email', { raw: `token=${token}`, contentType: FORM })).status, 200);
    let { r } = await x.signIn(claims());
    assert.deepEqual([r.json.needsPassword, r.json.username], [true, 'deputy1']);
    assert.equal(googleLink(s), null);
    // Another account's change to the victim's address, confirmed by the victim.
    await s.createUser({ username: 'deputy2', email: 'deputy2@example.com', password: 'deputy password 2' });
    const { token: session } = await s.login('deputy2', 'deputy password 2');
    const ch = await s.request('POST', '/api/v1/account/email', { token: session, body: { newEmail: 'victim2@gmail.com', password: 'deputy password 2' } });
    assert.equal(ch.status, 202, ch.text);
    token = await tokenOf('/confirm-email-change');
    assert.equal((await s.request('POST', '/confirm-email-change', { raw: `token=${token}`, contentType: FORM })).status, 200);
    ({ r } = await x.signIn(claims({ sub: '2002', email: 'victim2@gmail.com' })));
    assert.deepEqual([r.json.needsPassword, r.json.username], [true, 'deputy2']);
    assert.equal(googleLink(s, '2002'), null);
});

test('the SQLite store: the link step, its try counter in one UPDATE, and the next sign-in by the link', async (t) => {
    let srv = null;
    const idp = await startFakeOidc({ clientId: CLIENT_ID, clientSecret: CLIENT_SECRET, now: () => srv.now() });
    t.after(() => idp.close());
    srv = await startReal(t, { ...SSO_ENV, REQUIRE_EMAIL_VERIFICATION: '0', AUTH_FAILURES_PER_ACCOUNT: '20' }, { oidcEndpoints: idp.endpoints });
    const post = (p, body) => srv.request('POST', p, { body });
    assert.equal((await post('/api/v1/auth/register', { username: 'Magnus', email: 'magnus@gmail.com', password: PW })).status, 201);
    const signIn = async () => {
        const p = pkce();
        const a = (await post(START, { codeChallenge: p.challenge, redirectPort: PORT })).json;
        const q = idp.authorize(a.authUrl, claims());
        return post(FINISH, { attemptId: a.attemptId, codeVerifier: p.verifier, state: q.state, code: q.code, iss: q.iss });
    };
    let r = await signIn();
    assert.equal(r.json.needsPassword, true, r.text);
    const ticket = r.json.linkTicket;
    for (let i = 0; i < 2; i++) assert.equal((await post(LINK, { linkTicket: ticket, password: `wrong password ${i}` })).status, 401);
    assert.equal(srv.store.tokens.get('sso_link', sha256Hex(ticket)).data.tries, 2);
    r = await post(LINK, { linkTicket: ticket, password: PW });
    assert.equal(r.status, 200, r.text);
    const id = r.json.user.id;
    assert.equal(srv.store.sso.find('google', '1098765').userId, id);
    assert.ok(srv.store.tokens.get('sso_link', sha256Hex(ticket)).consumedAt);
    r = await signIn();
    assert.equal(r.status, 200, r.text);
    assert.equal(r.json.user.id, id);
});

// ---- security notices: a Google-made account, Google added to an account ----------------------

const CREATED = /^A .+ account was created with your Google account$/;
const ADDED = /^Google sign-in was added to your .+ account$/;
const notices = (s) => s.mailer.sent.filter((m) => CREATED.test(m.subject) || ADDED.test(m.subject));
/** The subjects of the notices sent to `to`, every queued mail handled first. */
async function noticesTo(s, to) {
    await s.mailer.idle();
    return notices(s).filter((m) => m.to === to).map((m) => m.subject);
}

test('notices: one mail when Google creates an account, one when Google is added after the password (and the code); none on a plain Google sign-in; no secret in them', async (t) => {
    const x = await setup();
    t.after(x.close);
    const { s, idp } = x;
    const name = s.config.serverName;
    const a = await x.start({ ip: VICTIM });
    const q = idp.authorize(a.authUrl, claims());
    const r = await x.finish(a, q, {}, { ip: VICTIM });
    assert.deepEqual(await noticesTo(s, 'magnus@gmail.com'), [], 'nothing before the account exists');
    const c = await x.post(COMPLETE, { ssoTicket: r.json.ssoTicket, username: 'MagnusH' }, { ip: VICTIM });
    assert.equal(c.status, 200, c.text);
    assert.deepEqual(await noticesTo(s, 'magnus@gmail.com'), [`A ${name} account was created with your Google account`]);
    const created = notices(s)[0].text;
    for (const part of ['Hello MagnusH', '"MagnusH"', new Date(s.now()).toUTCString(), '"Sign out everywhere"', `administrator of ${name}`]) {
        assert.ok(created.includes(part), part);
    }
    for (let i = 0; i < 2; i++) assert.equal((await x.signIn(claims())).r.status, 200);
    assert.equal((await noticesTo(s, 'magnus@gmail.com')).length, 1, 'a plain Google sign-in sends nothing');

    // Google added to a password account: once its password passes.
    await s.createUser({ username: 'Judit', email: 'judit@gmail.com', password: PW });
    const j = await x.signIn(claims({ sub: '2001', email: 'judit@gmail.com' }), { ip: VICTIM });
    assert.equal(j.r.json.needsPassword, true, j.r.text);
    assert.equal((await x.link(j.r.json.linkTicket, 'wrong password 1')).status, 401);
    assert.deepEqual(await noticesTo(s, 'judit@gmail.com'), [], 'not for a wrong password');
    const l = await x.link(j.r.json.linkTicket, PW, {}, { ip: VICTIM });
    assert.equal(l.status, 200, l.text);
    assert.deepEqual(await noticesTo(s, 'judit@gmail.com'), [`Google sign-in was added to your ${name} account`]);
    const added = notices(s).at(-1).text;
    for (const part of ['Hello Judit', '"Judit"', new Date(s.now()).toUTCString(), '"Forgot password"', '"Sign out everywhere"', `administrator of ${name}`]) {
        assert.ok(added.includes(part), part);
    }
    assert.equal((await x.signIn(claims({ sub: '2001', email: 'judit@gmail.com' }))).r.status, 200);
    assert.equal((await noticesTo(s, 'judit@gmail.com')).length, 1, 'the next Google sign-in sends nothing');

    // With two-step verification: only once the code passes.
    await s.createUser({ username: 'Hou', email: 'hou@gmail.com', password: PW });
    const secret = await enableMfa(s, 'Hou');
    const h = await x.signIn(claims({ sub: '2002', email: 'hou@gmail.com' }));
    const step = await x.link(h.r.json.linkTicket, PW);
    assert.equal(step.json.mfaRequired, true, step.text);
    assert.equal((await x.post(MFA, { mfaToken: step.json.mfaToken, code: totp(secret, s.now() - 3600000) })).status, 401);
    assert.deepEqual(await noticesTo(s, 'hou@gmail.com'), [], 'not before the code');
    const m = await x.post(MFA, { mfaToken: step.json.mfaToken, code: totp(secret, s.now()) });
    assert.equal(m.status, 200, m.text);
    assert.equal((await noticesTo(s, 'hou@gmail.com')).length, 1);

    // Never a code, state, ticket, token, password, PKCE verifier, link or IP address.
    const secrets = [q.code, q.state, a.attemptId, a.verifier, r.json.ssoTicket, c.json.token, j.r.json.linkTicket, l.json.token,
        step.json.mfaToken, m.json.token, PW, VICTIM, '198.51.100'];
    assert.equal(notices(s).length, 3);
    for (const mail of notices(s)) {
        for (const v of secrets) assert.ok(!mail.text.includes(v) && !mail.subject.includes(v), `${mail.subject}: ${v}`);
        assert.doesNotMatch(mail.text, /\b(sct|sso|mfa)_|https?:\/\/|\b\d{1,3}(\.\d{1,3}){3}\b/);
    }
});

test('notices: an SMTP failure never fails the Google sign-in; with MAIL_TRANSPORT=none nothing reaches the SMTP server', async (t) => {
    // An SMTP server that drops every connection.
    let connections = 0;
    const smtp = net.createServer((c) => { connections++; c.destroy(); });
    await new Promise((r) => smtp.listen(0, '127.0.0.1', r));
    t.after(() => new Promise((r) => smtp.close(r)));
    for (const transport of ['smtp', 'none']) {
        const env = { MAIL_TRANSPORT: transport, SMTP_HOST: '127.0.0.1', SMTP_PORT: String(smtp.address().port) };
        const x = await setup(env, null, { mailer: (config, log) => createMailer({ config, log }) });
        t.after(x.close);
        const { s } = x;
        const before = capturedLogs.length, connected = connections;
        const { r } = await x.signIn(claims());
        const c = await x.post(COMPLETE, { ssoTicket: r.json.ssoTicket, username: 'MagnusH' });
        assert.equal(c.status, 200, `${transport}: ${c.text}`);
        await s.createUser({ username: 'Judit', email: 'judit@gmail.com', password: PW });
        const j = await x.signIn(claims({ sub: '2001', email: 'judit@gmail.com' }));
        const l = await x.link(j.r.json.linkTicket, PW);
        assert.equal(l.status, 200, `${transport}: ${l.text}`);
        await s.mailer.idle();
        const failed = capturedLogs.slice(before).filter((line) => line.includes('e-mail not sent'));
        if (transport === 'smtp') {
            assert.ok(connections >= connected + 2, 'both notices went to the SMTP server');
            for (const tpl of ['ssoAccountCreated', 'ssoLinked']) assert.ok(failed.some((line) => line.includes(tpl)), tpl);
        } else {
            assert.equal(connections, connected, 'MAIL_TRANSPORT=none');
            assert.deepEqual(failed, []);
        }
    }
});
