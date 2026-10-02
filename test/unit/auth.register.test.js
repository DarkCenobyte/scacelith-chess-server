import test from 'node:test';
import assert from 'node:assert/strict';
import { solvePow, POW_TTL_MS } from '../../src/security/pow.js';
import { StoreError } from '../../src/store/index.js';
import { linkIn, startTestServer } from './helpers/auth-fakes.js';
import { startReal } from './helpers/real-auth.js';

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

const confirmPost = (s, token) => s.request('POST', '/verify-email', { raw: `token=${token}`, contentType: 'application/x-www-form-urlencoded' });
const tokenOf = (mail) => new URL(linkIn(mail.text)).searchParams.get('token');

test('register: 202 and a link; the account is created only when the link is used, then login', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const r = await s.request('POST', REG, { body: good() });
    assert.equal(r.status, 202);
    assert.deepEqual(r.json, { status: 'verification_sent' });
    assert.equal(s.store.users.byUsername('alice_1'), null, 'no account before the link is used');
    const p = s.store.signups.byUsername('alice_1');
    assert.equal(p.email, 'alice@example.com', 'stored in lower case');
    assert.match(p.passwordHash, /^scrypt\$10\$/);
    assert.equal(p.expiresAt, s.now() + 24 * 3600000, 'the life of the link');
    await s.mailer.idle();
    assert.equal(s.mailer.sent.length, 1);
    assert.equal(s.mailer.sent[0].to, 'alice@example.com');
    assert.match(s.mailer.sent[0].subject, /Confirm your e-mail address/);

    // Before confirmation there is no account: the right password is answered as an unknown account.
    for (const password of ['ivory rook takes e5', 'wrong password!']) {
        const l = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice_1', password } });
        assert.deepEqual([l.status, l.json.error], [401, 'invalid_credentials'], password);
    }

    const token = await verifyFromMail(s, s.mailer.sent[0]);
    const u = s.store.users.byUsername('Alice_1');
    assert.deepEqual([u.username, u.email, u.emailVerified, u.passwordHash], ['Alice_1', 'alice@example.com', true, p.passwordHash]);
    assert.equal(s.store.signups.byUsername('alice_1'), null, 'the pending signup is gone');
    const again = await confirmPost(s, token);
    assert.equal(again.status, 400, 'single use');
    assert.equal((await s.request('GET', `/verify-email?token=${token}`)).status, 400);
    const ok = await s.login('ALICE@example.com', 'ivory rook takes e5');
    assert.match(ok.token, /^sct_[A-Za-z0-9_-]{43}$/);
    assert.equal(ok.user.username, 'Alice_1');
    s.auth.events.flush();
    assert.deepEqual(s.store._raw.securityEvents.filter((e) => e.kind === 'register').map((e) => e.userId), [u.id]);
});

test('the GET confirmation page does not consume the token (link scanners)', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.request('POST', REG, { body: good() });
    await s.mailer.idle();
    const token = tokenOf(s.mailer.sent[0]);
    for (let i = 0; i < 3; i++) assert.equal((await s.request('GET', `/verify-email?token=${token}`)).status, 200);
    assert.equal(s.store.users.byUsername('Alice_1'), null);
    s.now.advance(24 * 3600000 + 1);
    assert.equal((await s.request('GET', `/verify-email?token=${token}`)).status, 400);
    assert.equal((await confirmPost(s, token)).status, 400, 'expired after 24 h');
    assert.equal(s.store.users.byUsername('Alice_1'), null);
});

test('a busy store during the confirmation: 503, nothing changed, the same link works afterwards', async (t) => {
    const s = await startReal(t);
    assert.equal((await s.request('POST', REG, { body: good() })).status, 202);
    await s.mailer.idle();
    const token = tokenOf(s.mailer.sent.at(-1));
    // The write lock is lost past the busy timeout once the link was used up.
    const create = s.store.users.create;
    s.store.users.create = () => { s.store.users.create = create; throw new StoreError('busy', 'database is locked'); };
    const busy = await confirmPost(s, token);
    assert.deepEqual([busy.status, busy.headers['retry-after']], [503, '1']);
    assert.equal(s.store.users.byEmail('alice@example.com'), null);
    assert.ok(s.store.signups.byEmail('alice@example.com'), 'the pending signup is still there');
    const done = await confirmPost(s, token);
    assert.equal(done.status, 200);
    assert.match(done.text, /confirmed/);
    assert.equal(s.store.users.byEmail('alice@example.com').emailVerified, true);
    assert.equal(s.store.signups.byEmail('alice@example.com'), null);
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

test('an existing e-mail: the same answer, the same work and the same traces as a new one', async (t) => {
    // Two identical servers, except that one has an account with the address.
    const servers = [];
    for (const withAccount of [false, true]) {
        const s = await startTestServer();
        t.after(s.close);
        if (withAccount) await s.createUser({ username: 'owner', email: 'target@example.com' });
        servers.push(s);
    }
    const answers = [];
    for (const s of servers) {
        const hash = t.mock.method(s.hasher, 'hash');
        const users = s.store._raw.users.size;
        const r = await s.request('POST', REG, { body: good({ username: 'Prober', email: 'TARGET@example.com' }) });
        const { date, ...headers } = r.headers;
        answers.push({ status: r.status, text: r.text, headers });
        // The same work: one password hash and one throttle call to the primary in both cases.
        assert.equal(hash.mock.callCount(), 1);
        assert.equal(s.primary.calls.filter((c) => c.type === 'once.consume').length, 1);
        assert.equal(s.store._raw.users.size, users, 'no account created');
        assert.equal(s.store.signups.byUsername('prober').email, 'target@example.com', 'the username is held');
    }
    assert.deepEqual(answers[0], answers[1]);
    assert.equal(answers[0].status, 202);
    // What a prober could look at next answers the same on both servers.
    const probes = [];
    for (const s of servers) {
        const again = await s.request('POST', REG, { body: good({ username: 'prober', email: 'other@example.com' }) });
        const login = await s.request('POST', '/api/v1/auth/login', { body: { login: 'Prober', password: good().password } });
        const profile = await s.request('GET', '/api/v1/players/Prober');
        probes.push([again.status, again.json.error, login.status, login.json.error, profile.status]);
    }
    assert.deepEqual(probes[0], [409, 'username_taken', 401, 'invalid_credentials', 404]);
    assert.deepEqual(probes[1], probes[0]);
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
    assert.equal(s.store.signups.byEmail('taken@example.com').tokenHash, null, 'no link for an address with an account');
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

test('a pending signup holds its username for the life of its link; the purge deletes it once expired', async (t) => {
    const s = await startReal(t);
    assert.equal((await s.request('POST', REG, { body: good() })).status, 202);
    const other = good({ email: 'mallory@example.com' });
    let r = await s.request('POST', REG, { body: { ...other, username: 'ALICE_1' } });
    assert.deepEqual([r.status, r.json.error], [409, 'username_taken'], 'held, whatever the case');
    s.now.advance(24 * 3600000 - 1);
    assert.equal((await s.request('POST', REG, { body: other })).status, 409);
    s.now.advance(1);
    // Expired: the username is free at once, before the purge.
    r = await s.request('POST', REG, { body: other });
    assert.equal(r.status, 202);
    assert.equal(s.store.signups.byUsername('alice_1').email, 'mallory@example.com');
    assert.equal(s.store.signups.byEmail('alice@example.com'), null, 'the expired signup was replaced');
    // The retention purge deletes the expired ones and keeps the live ones.
    assert.equal((await s.request('POST', REG, { body: good({ username: 'Bob_2', email: 'bob@example.com' }) })).status, 202);
    s.now.advance(12 * 3600000);
    assert.equal((await s.request('POST', REG, { body: good({ username: 'Carol', email: 'carol@example.com' }) })).status, 202);
    s.now.advance(12 * 3600000);
    const counts = s.store.retention.run(s.now(), s.config);
    assert.equal(counts.tokens, 2, 'the two signups of the first day');
    assert.equal(s.store.signups.byUsername('alice_1'), null);
    assert.equal(s.store.signups.byUsername('bob_2'), null);
    assert.ok(s.store.signups.byUsername('carol'));
    assert.equal(s.store.users.byUsername('carol'), null);
});

test('a second signup with the same address replaces the pending one', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    assert.equal((await s.request('POST', REG, { body: good() })).status, 202);
    await s.mailer.idle();
    const first = tokenOf(s.mailer.sent.at(-1));
    // Within 5 minutes: replaced, but no second link mail (the address is anyone's).
    let r = await s.request('POST', REG, { body: good({ username: 'Alice_2' }) });
    assert.equal(r.status, 202);
    await s.mailer.idle();
    assert.equal(s.mailer.sent.length, 1);
    assert.equal(s.store.signups.byUsername('alice_1'), null, 'the first username is free again');
    assert.equal((await s.request('GET', `/verify-email?token=${first}`)).status, 400, 'the first link no longer works');
    assert.equal((await s.request('POST', REG, { body: good({ username: 'Alice_1', email: 'someone@example.com' }) })).status, 202);
    // Later: replaced again, with a new link.
    s.now.advance(5 * 60000);
    r = await s.request('POST', REG, { body: good({ username: 'Alice_3', password: 'another ivory rook' }) });
    assert.equal(r.status, 202);
    await s.mailer.idle();
    const mails = s.mailer.sent.filter((m) => m.to === 'alice@example.com');
    assert.equal(mails.length, 2);
    assert.match(mails[1].text, /Alice_3/);
    assert.equal((await confirmPost(s, tokenOf(mails[1]))).status, 200);
    assert.equal(s.store.users.byEmail('alice@example.com').username, 'Alice_3');
    assert.equal(s.store.users.byUsername('Alice_2'), null);
    await s.login('alice_3', 'another ivory rook');
});

test('resend renews the link of a pending signup; it says nothing about the address', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    assert.equal((await s.request('POST', REG, { body: good() })).status, 202);
    await s.mailer.idle();
    const first = tokenOf(s.mailer.sent.at(-1));
    s.now.advance(20 * 3600000);
    const resend = (email) => s.request('POST', '/api/v1/auth/verify-email/resend', { body: { email } });
    for (const email of ['alice@example.com', 'nobody@example.com']) assert.deepEqual((await resend(email)).json, { status: 'accepted' });
    await s.mailer.idle();
    assert.deepEqual(s.mailer.sent.map((m) => m.to), ['alice@example.com', 'alice@example.com']);
    const second = tokenOf(s.mailer.sent[1]);
    assert.equal((await s.request('GET', `/verify-email?token=${first}`)).status, 400, 'the new link replaces the first one');
    assert.equal(s.store.signups.byEmail('alice@example.com').expiresAt, s.now() + 24 * 3600000);
    s.now.advance(10 * 3600000);
    assert.equal((await confirmPost(s, second)).status, 200, 'valid 24 h from the resend');
    assert.equal(s.store.users.byUsername('Alice_1').emailVerified, true);
});

test('the link of a pending signup whose username or address another account took meanwhile', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    await s.request('POST', REG, { body: good() });
    await s.request('POST', REG, { body: good({ username: 'Bob_2', email: 'bob@example.com' }) });
    await s.mailer.idle();
    const [alice, bob] = s.mailer.sent.map(tokenOf);
    // Accounts made another way (a Google sign-in, for one) with that address, and that username.
    await s.createUser({ username: 'Alice_9', email: 'alice@example.com' });
    await s.createUser({ username: 'bob_2', email: 'robert@example.com' });
    for (const token of [alice, bob]) {
        const r = await confirmPost(s, token);
        assert.equal(r.status, 409);
        assert.match(r.text, /Another account took this username or this e-mail address/);
        assert.equal((await confirmPost(s, token)).status, 400, 'the signup is dropped');
    }
    assert.equal(s.store.users.byUsername('Alice_1'), null);
    assert.equal(s.store.users.byEmail('bob@example.com'), null);
});

test('accounts created unconfirmed before pending signups keep their links and answers', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const old = await s.createUser({ username: 'oldtimer', email: 'old@example.com', verified: false });
    let l = await s.request('POST', '/api/v1/auth/login', { body: { login: 'oldtimer', password: old.password } });
    assert.deepEqual([l.status, l.json.error], [403, 'email_unverified']);
    // A signup with its address is the "existing address" case: a held username, no link, a notice.
    assert.equal((await s.request('POST', REG, { body: good({ email: 'old@example.com' }) })).status, 202);
    assert.equal((await s.request('POST', '/api/v1/auth/verify-email/resend', { body: { email: 'old@example.com' } })).status, 202);
    await s.mailer.idle();
    assert.deepEqual(s.mailer.sent.map((m) => m.subject.replace(/ (for|on) .*/, '')).sort(), ['Confirm your e-mail address', 'Someone tried to register']);
    const link = s.mailer.sent.find((m) => /Confirm/.test(m.subject));
    assert.match(link.text, /oldtimer/);
    await verifyFromMail(s, link);
    assert.equal(s.store.users.byId(old.id).emailVerified, true);
    assert.equal(s.store.users.byUsername('Alice_1'), null, 'the held username made no account');
    l = await s.login('oldtimer', old.password);
    assert.equal(l.user.id, old.id);
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
