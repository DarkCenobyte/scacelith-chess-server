import test from 'node:test';
import assert from 'node:assert/strict';
import { solvePow, POW_TTL_MS } from '../../src/security/pow.js';
import { linkIn, startTestServer } from './helpers/auth-fakes.js';

const REG = '/api/v1/auth/register';
const good = (over = {}) => ({ username: 'Alice_1', email: 'Alice@Example.com', password: 'ivory rook takes e5', ...over });

async function verifyFromMail(s, mail) {
    const url = new URL(linkIn(mail.text));
    assert.equal(url.origin, 'http://chess.example.org:8443', 'TLS_MODE=off in tests: http');
    assert.equal(url.pathname, '/verify-email');
    const token = url.searchParams.get('token');
    const page = await s.request('GET', `/verify-email?token=${token}`);
    assert.equal(page.status, 200);
    assert.match(page.text, /<form method="post" action="\/verify-email">/);
    assert.match(page.headers['content-security-policy'], /default-src 'none'/);
    const done = await s.request('POST', '/verify-email', { raw: `token=${token}`, contentType: 'application/x-www-form-urlencoded' });
    assert.equal(done.status, 200);
    assert.match(done.text, /confirmed/);
    return token;
}

test('register: 202, verification e-mail, confirmation page, then login', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const r = await s.request('POST', REG, { body: good() });
    assert.equal(r.status, 202);
    assert.deepEqual(r.json, { status: 'verification_sent' });
    const u = s.store.users.byUsername('alice_1');
    assert.equal(u.email, 'alice@example.com', 'stored in lower case');
    assert.equal(u.emailVerified, false);
    assert.match(u.passwordHash, /^scrypt\$10\$/);
    await s.mailer.idle();
    assert.equal(s.mailer.sent.length, 1);
    assert.equal(s.mailer.sent[0].to, 'alice@example.com');
    assert.match(s.mailer.sent[0].subject, /Confirm your e-mail address/);

    // Before confirmation: a correct password is answered email_unverified, a wrong one invalid_credentials.
    let l = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice_1', password: 'ivory rook takes e5' } });
    assert.deepEqual([l.status, l.json.error], [403, 'email_unverified']);
    l = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice_1', password: 'wrong password!' } });
    assert.deepEqual([l.status, l.json.error], [401, 'invalid_credentials']);

    const token = await verifyFromMail(s, s.mailer.sent[0]);
    assert.equal(s.store.users.byUsername('Alice_1').emailVerified, true);
    const again = await s.request('POST', '/verify-email', { raw: `token=${token}`, contentType: 'application/x-www-form-urlencoded' });
    assert.equal(again.status, 400, 'single use');
    assert.equal((await s.request('GET', `/verify-email?token=${token}`)).status, 400);
    const ok = await s.login('ALICE@example.com', 'ivory rook takes e5');
    assert.match(ok.token, /^sct_[A-Za-z0-9_-]{43}$/);
    assert.equal(ok.user.username, 'Alice_1');
});

test('the GET confirmation page does not consume the token (link scanners)', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.request('POST', REG, { body: good() });
    await s.mailer.idle();
    const token = new URL(linkIn(s.mailer.sent[0].text)).searchParams.get('token');
    for (let i = 0; i < 3; i++) assert.equal((await s.request('GET', `/verify-email?token=${token}`)).status, 200);
    assert.equal(s.store.users.byUsername('Alice_1').emailVerified, false);
    s.now.advance(24 * 3600000 + 1);
    assert.equal((await s.request('POST', '/verify-email', { raw: `token=${token}`, contentType: 'application/x-www-form-urlencoded' })).status, 400, 'expired after 24 h');
});

test('username rules', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'bob' });
    const cases = [
        ['ab', 'invalid_username'], ['a'.repeat(21), 'invalid_username'], ['_alice', 'invalid_username'], ['-x-y', 'invalid_username'],
        ['al ice', 'invalid_username'], ['alicé', 'invalid_username'], ['admin', 'invalid_username'], ['Admin_Joe', 'invalid_username'],
        ['Stockfish', 'invalid_username'], ['system', 'invalid_username'], ['moderator2', 'invalid_username'], ['SCACELITH', 'invalid_username'],
        ['BOB', 'username_taken'],
    ];
    for (const [username, code] of cases) {
        const r = await s.request('POST', REG, { body: good({ username, email: `${Math.random().toString(36).slice(2)}@example.com` }) });
        assert.equal(r.json.error, code, username);
        assert.equal(r.status, code === 'username_taken' ? 409 : 400, username);
    }
    for (const username of ['abc', '9lives', 'a_b-c', 'x'.repeat(20)]) {
        const r = await s.request('POST', REG, { body: good({ username, email: `${username}@example.com` }) });
        assert.equal(r.status, 202, username);
    }
});

test('e-mail and password checks', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    for (const email of ['nope', 'a@b', 'a@@example.com', 'a b@example.com', `${'x'.repeat(250)}@e.com`, 'a@example..com']) {
        const r = await s.request('POST', REG, { body: good({ email }) });
        assert.equal(r.json.error, email.length > 254 ? 'invalid_request' : 'invalid_email', email);
    }
    const reasons = [['short', 'too_short'], ['qwertyuiop', 'too_common'], ['my alice_1 password', 'contains_username'], ['xx alice xx yy', 'contains_email']];
    for (const [password, reason] of reasons) {
        const r = await s.request('POST', REG, { body: good({ password }) });
        assert.deepEqual([r.status, r.json.error, r.json.reason], [400, 'weak_password', reason], password);
    }
});

test('an existing e-mail gets the same answer and its owner a notice', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.createUser({ username: 'owner', email: 'taken@example.com' });
    const r = await s.request('POST', REG, { body: good({ username: 'newcomer', email: 'TAKEN@example.com' }) });
    const fresh = await s.request('POST', REG, { body: good({ username: 'another', email: 'free@example.com' }) });
    assert.equal(r.status, fresh.status);
    assert.deepEqual(r.json, fresh.json);
    assert.equal(s.store.users.byUsername('newcomer'), null);
    await s.mailer.idle();
    const notice = s.mailer.sent.find((m) => m.to === 'taken@example.com');
    assert.match(notice.subject, /Someone tried to register/);
    assert.doesNotMatch(notice.text, /verify-email/);
    // A second attempt within the hour does not mail the owner again.
    await s.request('POST', REG, { body: good({ username: 'newcomer2', email: 'taken@example.com' }) });
    await s.mailer.idle();
    assert.equal(s.mailer.sent.filter((m) => m.to === 'taken@example.com').length, 1);
    s.auth.events.flush();
    assert.ok(s.store._raw.securityEvents.some((e) => e.kind === 'register_existing_email'));
});

test('registration closed: 403', async (t) => {
    const s = await startTestServer({ env: { REGISTRATION: 'closed' } });
    t.after(s.close);
    const r = await s.request('POST', REG, { body: good() });
    assert.deepEqual([r.status, r.json.error], [403, 'registration_closed']);
});

test('without e-mail confirmation the account is ready at once', async (t) => {
    const s = await startTestServer({ env: { REQUIRE_EMAIL_VERIFICATION: '0' } });
    t.after(s.close);
    const r = await s.request('POST', REG, { body: good() });
    assert.deepEqual([r.status, r.json], [201, { status: 'ready' }]);
    await s.mailer.idle();
    assert.equal(s.mailer.sent.length, 0);
    await s.login('Alice_1', 'ivory rook takes e5');
    const dup = await s.request('POST', REG, { body: good({ username: 'other' }) });
    assert.deepEqual([dup.status, dup.json.error], [409, 'email_taken']);
});

test('proof of work on registration: required, verified, single use, expiring, bound to the client', async (t) => {
    const s = await startTestServer({ env: { POW_REGISTER_BITS: '8' } });
    t.after(s.close);
    const ip = '203.0.113.9';
    let r = await s.request('POST', REG, { body: good(), ip });
    assert.equal(r.status, 428);
    assert.equal(r.json.error, 'pow_required');
    const { challenge, bits, expiresAt } = r.json.pow;
    assert.equal(bits, 8);
    assert.equal(expiresAt, s.now() + POW_TTL_MS);
    const nonce = solvePow(challenge, bits);

    r = await s.request('POST', REG, { body: good({ pow: { challenge, nonce } }), ip: '198.51.100.20' });
    assert.deepEqual([r.status, r.json.reason], [428, 'network'], 'another client cannot use it');
    r = await s.request('POST', REG, { body: good({ pow: { challenge, nonce: String(+nonce + 1) } }), ip });
    assert.equal(r.status, 428);
    r = await s.request('POST', REG, { body: good({ pow: { challenge, nonce } }), ip });
    assert.equal(r.status, 202);
    r = await s.request('POST', REG, { body: good({ username: 'Bob_2', email: 'bob@example.com', pow: { challenge, nonce } }), ip });
    assert.deepEqual([r.status, r.json.reason], [428, 'replayed']);
    assert.ok(s.primary.calls.some((c) => c.type === 'once.consume' && c.payload.key.startsWith('pow:')));

    r = await s.request('POST', REG, { body: good({ username: 'Carol', email: 'carol@example.com' }), ip });
    const c2 = r.json.pow;
    s.now.advance(POW_TTL_MS + 1);
    r = await s.request('POST', REG, { body: good({ username: 'Carol', email: 'carol@example.com', pow: { challenge: c2.challenge, nonce: solvePow(c2.challenge, 8) } }), ip });
    assert.deepEqual([r.status, r.json.reason], [428, 'expired']);
    assert.ok(r.json.pow.challenge, 'a fresh challenge comes with the refusal');

    // Cheap checks come before the work: an invalid username is answered without a challenge.
    r = await s.request('POST', REG, { body: good({ username: '_bad' }), ip });
    assert.equal(r.status, 400);
});

test('per-IP limit on the auth endpoints (shared through the primary)', async (t) => {
    const s = await startTestServer({ env: { AUTH_RATE_PER_IP: '3' } });
    t.after(s.close);
    for (let i = 0; i < 3; i++) {
        const r = await s.request('POST', REG, { body: good({ username: `user${i}`, email: `u${i}@example.com` }), ip: '192.0.2.50' });
        assert.equal(r.status, 202);
    }
    const r = await s.request('POST', REG, { body: good({ username: 'user9', email: 'u9@example.com' }), ip: '192.0.2.50' });
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited']);
    assert.ok(r.json.retryAfter > 0);
    assert.ok(s.primary.calls.some((c) => c.type === 'ratelimit.take' && c.payload.key === 'auth:192.0.2.50' && c.payload.windowMs === 600000));
    assert.equal((await s.request('POST', REG, { body: good({ username: 'user8', email: 'u8@example.com' }), ip: '192.0.2.51' })).status, 202);
});
