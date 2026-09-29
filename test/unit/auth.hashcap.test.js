import test from 'node:test';
import assert from 'node:assert/strict';
import { BUSY_RETRY_AFTER_SEC, serverBusy } from '../../src/auth/errors.js';
import { createPasswordHasher } from '../../src/security/password.js';
import { metrics } from '../../src/metrics.js';
import { linkIn, startTestServer } from './helpers/auth-fakes.js';

// The per-worker cap on password hashes (PASSWORD_HASH_*), seen from the API: a burst beyond the
// queue is answered 503 server_busy with Retry-After while the cap holds, for every endpoint that
// hashes or checks a password, and a refused request changes nothing.

const LOGIN = '/api/v1/auth/login';
const PW = 'correct horse battery';
const NEW = 'a brand new passphrase';
const FORM = 'application/x-www-form-urlencoded';

// A fast real hasher (scrypt N=2^10) whose calls wait on a gate while it is closed, and which
// records how many run at once.
function gatedHasher() {
    const real = createPasswordHasher({ scrypt: { logN: 10 }, argon2: false });
    const g = { running: 0, peak: 0, calls: 0, closed: null };
    const wrap = (fn) => async (...args) => {
        g.calls++;
        g.running++;
        g.peak = Math.max(g.peak, g.running);
        try {
            if (g.closed) await g.closed.promise;
            return await fn(...args);
        } finally {
            g.running--;
        }
    };
    g.close = () => { let open; g.closed = { promise: new Promise((r) => { open = r; }), open }; };
    g.open = () => { const c = g.closed; g.closed = null; if (c) c.open(); };
    g.reset = () => { g.peak = g.running; g.calls = 0; };
    g.hasher = { ...real, hash: wrap(real.hash), verify: wrap(real.verify), verifyDummy: wrap(real.verifyDummy) };
    return g;
}

async function waitFor(pred, what, timeoutMs = 5000) {
    const t0 = Date.now();
    while (!pred()) {
        if (Date.now() - t0 > timeoutMs) throw new Error(`timed out waiting for ${what}`);
        await new Promise((r) => setTimeout(r, 5));
    }
}

// The server's limiter, once the start-up warm-up (the dummy hash, which also takes a slot) is over.
async function idleLimiter(s) {
    const limiter = s.auth._svc.hasher.limiter;
    await s.hasher.warmUp();
    await waitFor(() => limiter.stats().active === 0, 'the start-up warm-up to finish');
    return limiter;
}

const rejected = (reason) => metrics.metrics.get('scacelith_password_hash_rejected_total').labels(reason).value;

function assertBusy(r) {
    assert.equal(r.status, 503, r.text);
    assert.equal(r.json.error, 'server_busy');
    assert.equal(r.json.message, 'The server is busy; try again in a few seconds.');
    assert.ok(Number.isInteger(r.json.retryAfter) && r.json.retryAfter >= 5 && r.json.retryAfter <= 15, `retryAfter ${r.json.retryAfter}`);
    assert.equal(r.headers['retry-after'], String(r.json.retryAfter));
}

test('a login flood: the cap holds, the queue overflow gets 503 server_busy with Retry-After', async (t) => {
    const g = gatedHasher();
    const s = await startTestServer({ hasher: g.hasher, env: { PASSWORD_HASH_CONCURRENCY: '1', PASSWORD_HASH_QUEUE_MAX: '3' } });
    t.after(async () => { g.open(); await s.close(); });
    await s.createUser({ username: 'alice', password: PW });
    const limiter = await idleLimiter(s);
    g.reset();
    const full0 = rejected('queue_full');

    g.close();
    const answered = [];
    // Unknown accounts: their dummy verification goes through the same queue.
    const flood = Array.from({ length: 12 }, (_, i) => s.request('POST', LOGIN, { body: { login: `ghost${i}`, password: 'not the password' }, ip: `203.0.113.${i}` })
        .then((r) => { answered.push(r); return r; }));
    await waitFor(() => answered.length === 8 && limiter.stats().waiting === 3, '8 refusals and 3 waiters');
    assert.equal(limiter.stats().active, 1);
    assert.equal(g.running, 1, 'one hash in flight while the others wait');
    for (const r of answered) assertBusy(r);
    g.open();
    const all = await Promise.all(flood);
    const byStatus = {};
    for (const r of all) byStatus[r.status] = (byStatus[r.status] || 0) + 1;
    assert.deepEqual(byStatus, { 401: 4, 503: 8 });
    assert.equal(g.peak, 1, 'never two hashes at once');
    assert.equal(g.calls, 4, 'refused requests hashed nothing');
    assert.equal(rejected('queue_full'), full0 + 8);
    assert.deepEqual([limiter.stats().active, limiter.stats().waiting], [0, 0]);

    // Refusals are not failed logins: only the 4 checked passwords were counted.
    s.auth.events.flush();
    assert.equal(s.store._raw.securityEvents.filter((e) => e.kind === 'login_failed').length, 4);
    assert.equal((await s.request('POST', LOGIN, { body: { login: 'alice', password: PW } })).status, 200);
});

test('a wait longer than PASSWORD_HASH_QUEUE_TIMEOUT_MS is answered 503 server_busy', async (t) => {
    const g = gatedHasher();
    const s = await startTestServer({ hasher: g.hasher, env: { PASSWORD_HASH_QUEUE_TIMEOUT_MS: '100' } });
    t.after(async () => { g.open(); await s.close(); });
    await s.createUser({ username: 'alice', password: PW });
    const limiter = await idleLimiter(s);
    assert.deepEqual(limiter.stats(), { active: 0, waiting: 0, concurrency: 1, queueMax: 32, queueTimeoutMs: 100 }, 'defaults');
    const timeout0 = rejected('timeout');

    g.close();
    const answered = [];
    const first = s.request('POST', LOGIN, { body: { login: 'alice', password: PW } });
    await waitFor(() => limiter.stats().active === 1, 'the first login to hold the slot');
    const waiters = [0, 1].map((i) => s.request('POST', LOGIN, { body: { login: `ghost${i}`, password: 'whatever it is' } }).then((r) => { answered.push(r); return r; }));
    await waitFor(() => answered.length === 2, 'the two waiters to give up');
    for (const r of answered) assertBusy(r);
    assert.equal(limiter.stats().waiting, 0, 'the expired waiters left the queue');
    assert.equal(rejected('timeout'), timeout0 + 2);
    g.open();
    assert.equal((await first).status, 200);
    await Promise.all(waiters);
});

test('every password endpoint goes through the cap, and a refused request changes nothing', async (t) => {
    const g = gatedHasher();
    const s = await startTestServer({ hasher: g.hasher, env: { PASSWORD_HASH_QUEUE_MAX: '0' } });
    t.after(async () => { g.open(); await s.close(); });
    await s.createUser({ username: 'alice', password: PW });
    await s.createUser({ username: 'bob', password: PW });
    const limiter = await idleLimiter(s);
    const alice = await s.login('alice', PW);
    const bob = await s.login('bob', PW);
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    await s.mailer.idle();
    const mail = s.mailer.sent.find((m) => m.to === 'alice@example.com' && /Reset your/.test(m.subject));
    const token = new URL(linkIn(mail.text)).searchParams.get('token');
    const aliceHash = s.store.users.byUsername('alice').passwordHash;

    // One login holds the only slot; with no queue, every other hash is refused at once.
    g.close();
    const holder = s.request('POST', LOGIN, { body: { login: 'ghost', password: 'whatever it is' } });
    await waitFor(() => limiter.stats().active === 1, 'the holder to take the slot');

    assertBusy(await s.request('POST', LOGIN, { body: { login: 'alice', password: PW } }));
    assertBusy(await s.request('POST', '/api/v1/auth/register', { body: { username: 'carol', email: 'carol@example.com', password: 'a fine passphrase of hers' } }));
    assert.equal(s.store.users.byUsername('carol'), null, 'no account created');
    assertBusy(await s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: NEW } }));
    const page = await s.request('POST', '/reset-password', { raw: `token=${token}&newPassword=${encodeURIComponent(NEW)}&confirmPassword=${encodeURIComponent(NEW)}`, contentType: FORM });
    assert.equal(page.status, 503);
    assert.match(page.headers['retry-after'], /^(?:[5-9]|1[0-5])$/);
    assert.match(page.headers['content-type'], /^text\/html/);
    assert.match(page.text, /The server is busy; try again in a few seconds\./);
    assert.match(page.text, /<input type="hidden" name="token" value="[A-Za-z0-9_-]{43}">/, 'the form is shown again');
    assertBusy(await s.request('POST', '/api/v1/account/password', { token: alice.token, body: { currentPassword: PW, newPassword: NEW } }));
    assertBusy(await s.request('POST', '/api/v1/account/mfa/totp/setup', { token: alice.token, body: { password: PW } }));
    assertBusy(await s.request('POST', '/api/v1/account/delete', { token: bob.token, body: { password: PW } }));
    assert.equal(s.store.users.byUsername('alice').passwordHash, aliceHash, 'password unchanged');
    assert.equal(s.store.users.byUsername('bob').status, 'active', 'account not deleted');
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: alice.token })).status, 200, 'sessions untouched');

    g.open();
    assert.equal((await holder).status, 401);
    // The reset link survived the refusals.
    const r = await s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: NEW } });
    assert.equal(r.status, 200, r.text);
    await s.login('alice', NEW);
    assert.equal((await s.request('POST', '/api/v1/account/delete', { token: bob.token, body: { password: PW } })).status, 200);
});

test('serverBusy: 503 with a Retry-After spread over 5 to 15 s', () => {
    const seen = new Set();
    for (let i = 0; i < 400; i++) {
        const e = serverBusy();
        assert.deepEqual([e.status, e.code, e.expose], [503, 'server_busy', true]);
        assert.ok(Number.isInteger(e.extra.retryAfter) && e.extra.retryAfter >= BUSY_RETRY_AFTER_SEC.min && e.extra.retryAfter <= BUSY_RETRY_AFTER_SEC.max);
        seen.add(e.extra.retryAfter);
    }
    assert.ok(seen.has(5) && seen.has(15) && seen.size >= 9, `values seen: ${[...seen].sort((a, b) => a - b)}`);
    assert.equal(serverBusy(7).extra.retryAfter, 7);
});
