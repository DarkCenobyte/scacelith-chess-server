import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { BUSY_RETRY_AFTER_SEC, serverBusy } from '../../src/auth/errors.js';
import { configWarnings, testConfig, threadPoolSize } from '../../src/config.js';
import { createPasswordHasher } from '../../src/security/password.js';
import { metrics } from '../../src/metrics.js';
import { capturedLogs, linkIn, startTestServer } from './helpers/auth-fakes.js';

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

// A fast real hasher whose calls for a held password wait until that password is let go.
// `blocked` counts the calls waiting on a hold now.
function keyedHasher(logN = 10) {
    const real = createPasswordHasher({ scrypt: { logN }, argon2: false });
    const holds = new Map();
    const k = { real, blocked: 0 };
    const gate = async (password) => {
        const h = holds.get(password);
        if (!h) return;
        k.blocked++;
        try { await h.promise; } finally { k.blocked--; }
    };
    k.hold = (password) => { let open; const promise = new Promise((r) => { open = r; }); holds.set(password, { promise, open }); };
    k.release = (password) => { const h = holds.get(password); holds.delete(password); if (h) h.open(); };
    k.releaseAll = () => { for (const p of [...holds.keys()]) k.release(p); };
    k.hasher = {
        ...real,
        hash: async (password) => { await gate(password); return real.hash(password); },
        verify: async (stored, password) => { await gate(password); return real.verify(stored, password); },
        verifyDummy: async (password) => { await gate(password); return real.verifyDummy(password); },
    };
    return k;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const liveSessions = (s, userId) => s.store.sessions.listForUser(userId).filter((x) => !x.revokedAt).length;

async function resetToken(s, email) {
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email } });
    await s.mailer.idle();
    const mail = [...s.mailer.sent].reverse().find((m) => m.to === email && /Reset your/.test(m.subject));
    return new URL(linkIn(mail.text)).searchParams.get('token');
}

test('a password change waits for both of its hashes within one queue timeout', async (t) => {
    const k = keyedHasher();
    const Q = 1200;
    const s = await startTestServer({ hasher: k.hasher, env: { PASSWORD_HASH_QUEUE_TIMEOUT_MS: String(Q) } });
    t.after(async () => { k.releaseAll(); await s.close(); });
    await s.createUser({ username: 'alice', password: PW });
    const limiter = await idleLimiter(s);
    const alice = await s.login('alice', PW);
    const other = await s.login('alice', PW);
    const aliceHash = s.store.users.byUsername('alice').passwordHash;
    const timeout0 = rejected('timeout');

    // Queue: [holder 1 (running), the change's check of the current password, holder 2].
    k.hold('holder one pw'); k.hold('holder two pw');
    const h1 = s.request('POST', LOGIN, { body: { login: 'ghost1', password: 'holder one pw' }, ip: '203.0.113.1' });
    await waitFor(() => limiter.stats().active === 1 && k.blocked === 1, 'holder 1 to take the slot');
    const t0 = performance.now();
    const change = s.request('POST', '/api/v1/account/password', { token: alice.token, body: { currentPassword: PW, newPassword: NEW }, ip: '203.0.113.2' });
    await waitFor(() => limiter.stats().waiting === 1, 'the change to wait');
    const h2 = s.request('POST', LOGIN, { body: { login: 'ghost2', password: 'holder two pw' }, ip: '203.0.113.3' });
    await waitFor(() => limiter.stats().waiting === 2, 'holder 2 to wait');
    // Half the budget is spent; then the current password is checked, and holder 2 takes the
    // slot before the new password's hash, which may only wait for what is left.
    await sleep(Q / 2);
    k.release('holder one pw');
    const r = await change;
    const ms = performance.now() - t0;
    assertBusy(r);
    // Two separate waits would answer after about 1.5 x Q.
    assert.ok(ms < Q + 300, `answered after ${ms.toFixed(0)} ms: the two waits must share the ${Q} ms budget`);
    assert.equal(rejected('timeout'), timeout0 + 1);
    assert.equal(s.store.users.byUsername('alice').passwordHash, aliceHash, 'password unchanged');
    assert.equal((await s.request('GET', '/api/v1/account/me', { token: other.token })).status, 200, 'other sessions untouched');
    k.release('holder two pw');
    assert.deepEqual([(await h1).status, (await h2).status], [401, 401]);
    // With a free queue the same change goes through.
    const ok = await s.request('POST', '/api/v1/account/password', { token: alice.token, body: { currentPassword: PW, newPassword: NEW } });
    assert.equal(ok.status, 200, ok.text);
    await s.login('alice', NEW);
});

test('the login rehash of an outdated hash does not wait for a slot, and is not an error when skipped', async (t) => {
    const k = keyedHasher();
    const s = await startTestServer({ hasher: k.hasher });
    t.after(async () => { k.releaseAll(); await s.close(); });
    const old = createPasswordHasher({ scrypt: { logN: 9 }, argon2: false });
    const P1 = 'legacy passphrase one';
    const id = s.store.users.create({ username: 'legacy', email: 'legacy@example.com', passwordHash: await old.hash(P1), emailVerified: true });
    const oldHash = s.store.users.byId(id).passwordHash;
    const limiter = await idleLimiter(s);
    const counts0 = ['queue_full', 'timeout', 'source_limit'].map(rejected);

    // The login's check holds the slot; a holder queues behind it and gets the slot next.
    k.hold(P1); k.hold('holder pw');
    const login = s.request('POST', LOGIN, { body: { login: 'legacy', password: P1 }, ip: '203.0.113.1' });
    await waitFor(() => limiter.stats().active === 1 && k.blocked === 1, 'the login check to hold the slot');
    const holder = s.request('POST', LOGIN, { body: { login: 'ghost', password: 'holder pw' }, ip: '203.0.113.2' });
    await waitFor(() => limiter.stats().waiting === 1, 'the holder to wait');
    const logs0 = capturedLogs.length;
    k.release(P1);
    const r = await Promise.race([login, sleep(3000).then(() => null)]);
    assert.ok(r, 'the login answered while the slot was still taken (it did not wait again for the rehash)');
    assert.equal(r.status, 200, r.text);
    assert.equal(s.store.users.byId(id).passwordHash, oldHash, 'rehash skipped: no free slot');
    const logged = capturedLogs.slice(logs0).map((l) => JSON.parse(l));
    assert.ok(!logged.some((l) => /rehash failed|saturated/.test(l.msg)), JSON.stringify(logged.map((l) => l.msg)));
    assert.deepEqual(['queue_full', 'timeout', 'source_limit'].map(rejected), counts0, 'a skipped rehash is no refusal');
    k.release('holder pw');
    assert.equal((await holder).status, 401);

    // With a free slot, the next login upgrades the hash.
    await waitFor(() => limiter.stats().active === 0, 'the queue to drain');
    await s.login('legacy', P1);
    assert.match(s.store.users.byId(id).passwordHash, /^scrypt\$10\$/);
});

for (const concurrency of [1, 2]) {
    test(`a password reset that lands during a login with a rehash wins (PASSWORD_HASH_CONCURRENCY=${concurrency})`, async (t) => {
        const k = keyedHasher();
        const s = await startTestServer({ hasher: k.hasher, env: { PASSWORD_HASH_CONCURRENCY: String(concurrency) } });
        t.after(async () => { k.releaseAll(); await s.close(); });
        const old = createPasswordHasher({ scrypt: { logN: 9 }, argon2: false });
        const P1 = 'old leaked passphrase', P2 = 'fresh secret passphrase';
        const id = s.store.users.create({ username: 'alice', email: 'alice@example.com', passwordHash: await old.hash(P1), emailVerified: true });
        const limiter = await idleLimiter(s);
        const token = await resetToken(s, 'alice@example.com');

        // The login checks P1 (an outdated hash, so it will want to rehash); the reset runs while
        // the check is in its slot (concurrency 2), or right after it (concurrency 1).
        k.hold(P1);
        const login = s.request('POST', LOGIN, { body: { login: 'alice', password: P1 }, ip: '203.0.113.9' });
        await waitFor(() => k.blocked === 1, 'the login check to start');
        const reset = s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: P2 } });
        if (concurrency === 2) assert.equal((await reset).status, 200);
        else await waitFor(() => limiter.stats().waiting === 1, 'the reset to wait');
        k.release(P1);
        const [rl, rr] = await Promise.all([login, reset]);
        assert.equal(rr.status, 200, rr.text);
        assert.ok(rl.status === 200 || rl.status === 401, rl.text);
        const stored = s.store.users.byId(id).passwordHash;
        assert.equal((await k.real.verify(stored, P2)).ok, true, 'the reset password is the one stored');
        assert.equal((await k.real.verify(stored, P1)).ok, false, 'the old password no longer works');
        assert.equal(liveSessions(s, id), 0, 'no session opened with the old password survives the reset');
        assert.equal((await s.request('POST', LOGIN, { body: { login: 'alice', password: P1 } })).status, 401);
        await s.login('alice', P2);
    });
}

test('a password reset that lands while a password change hashes the new password wins', async (t) => {
    const k = keyedHasher();
    const s = await startTestServer({ hasher: k.hasher, env: { PASSWORD_HASH_CONCURRENCY: '2' } });
    t.after(async () => { k.releaseAll(); await s.close(); });
    const u = await s.createUser({ username: 'alice', password: PW });
    await idleLimiter(s);
    const alice = await s.login('alice', PW);
    const token = await resetToken(s, 'alice@example.com');
    const P2 = 'the reset passphrase';

    k.hold(NEW);
    const change = s.request('POST', '/api/v1/account/password', { token: alice.token, body: { currentPassword: PW, newPassword: NEW } });
    await waitFor(() => k.blocked === 1, 'the change to hash the new password');
    assert.equal((await s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: P2 } })).status, 200);
    k.release(NEW);
    const r = await change;
    assert.equal(r.status, 403, r.text);
    assert.equal(r.json.error, 'invalid_password');
    const stored = s.store.users.byId(u.id).passwordHash;
    assert.equal((await k.real.verify(stored, P2)).ok, true, 'the reset password is the one stored');
    assert.equal((await k.real.verify(stored, NEW)).ok, false);
});

test('a concurrent login that upgraded the hash does not make another login of the same password fail', async (t) => {
    const k = keyedHasher();
    const s = await startTestServer({ hasher: k.hasher, env: { PASSWORD_HASH_CONCURRENCY: '2' } });
    t.after(async () => { k.releaseAll(); await s.close(); });
    const old = createPasswordHasher({ scrypt: { logN: 9 }, argon2: false });
    const P1 = 'legacy passphrase one';
    const id = s.store.users.create({ username: 'legacy', email: 'legacy@example.com', passwordHash: await old.hash(P1), emailVerified: true });
    await idleLimiter(s);
    // Login A checks P1 and waits; login B checks, rehashes and stores the new hash; A then
    // finds another hash than the one it checked, and checks the password again against it.
    k.hold(P1);
    const a = s.request('POST', LOGIN, { body: { login: 'legacy', password: P1 }, ip: '203.0.113.1' });
    await waitFor(() => k.blocked === 1, 'login A to start');
    const oldHash = s.store.users.byId(id).passwordHash;
    s.store.users.update(id, { passwordHash: await k.real.hash(P1) });     // what login B stored
    assert.notEqual(s.store.users.byId(id).passwordHash, oldHash);
    k.release(P1);
    const r = await a;
    assert.equal(r.status, 200, r.text);
    assert.equal(liveSessions(s, id), 1);
});

test('once the queue is half full, one client source has at most 2 hashes waiting; the next one gets 429 rate_limited', async (t) => {
    const k = keyedHasher();
    // A queue of 6: the per-source cap applies from 3 waiting hashes on.
    const s = await startTestServer({ hasher: k.hasher, env: { PASSWORD_HASH_QUEUE_MAX: '6' } });
    t.after(async () => { k.releaseAll(); await s.close(); });
    await s.createUser({ username: 'alice', password: PW });
    const limiter = await idleLimiter(s);
    assert.equal(limiter.perSourceMax, 2, 'PASSWORD_HASH_WAITERS_PER_SOURCE defaults to 2');
    const token = await resetToken(s, 'alice@example.com');
    const src0 = rejected('source_limit');
    k.hold('holder pw');
    const holder = s.request('POST', LOGIN, { body: { login: 'ghost', password: 'holder pw' }, ip: '192.0.2.1' });
    await waitFor(() => limiter.stats().active === 1 && k.blocked === 1, 'the holder to take the slot');

    const queued = [];
    const send = (ip, login = 'nobody') => s.request('POST', LOGIN, { body: { login, password: 'not the password' }, ip });
    // Another address of the same /24 is another source.
    queued.push(send('198.51.100.8'));
    await waitFor(() => limiter.stats().waiting === 1, '1 waiter');
    // One IPv4 address: 2 waiting; the queue is then half full, and its third is refused.
    queued.push(send('198.51.100.7'), send('198.51.100.7'));
    await waitFor(() => limiter.stats().waiting === 3, '3 waiters');
    const r3 = await send('198.51.100.7');
    assert.equal(r3.status, 429, r3.text);
    assert.equal(r3.json.error, 'rate_limited');
    assert.ok(Number.isInteger(r3.json.retryAfter) && r3.json.retryAfter >= 5 && r3.json.retryAfter <= 15, `retryAfter ${r3.json.retryAfter}`);
    assert.equal(r3.headers['retry-after'], String(r3.json.retryAfter));
    // The reset page shows its form again (the link stays valid), with the same status.
    const page = await s.request('POST', '/reset-password', { raw: `token=${token}&newPassword=${encodeURIComponent(NEW)}&confirmPassword=${encodeURIComponent(NEW)}`, contentType: FORM, ip: '198.51.100.7' });
    assert.equal(page.status, 429);
    assert.match(page.headers['retry-after'], /^(?:[5-9]|1[0-5])$/);
    assert.match(page.text, /<input type="hidden" name="token" value="[A-Za-z0-9_-]{43}">/, 'the form is shown again');
    // IPv6: the /64s of one /48 are one source.
    queued.push(send('2001:db8:1:1::1'), send('2001:db8:1:2::1'));
    await waitFor(() => limiter.stats().waiting === 5, '5 waiters');
    const r6 = await s.request('POST', '/api/v1/auth/register', { body: { username: 'carol', email: 'carol@example.com', password: 'a fine passphrase of hers' }, ip: '2001:db8:1:ffff::5' });
    assert.equal(r6.status, 429, r6.text);
    assert.equal(r6.json.error, 'rate_limited');
    assert.equal(s.store.users.byUsername('carol'), null, 'nothing changed');
    queued.push(send('2001:db8:2::1'));     // another /48
    await waitFor(() => limiter.stats().waiting === 6, '6 waiters');
    assert.equal(rejected('source_limit'), src0 + 3);

    k.release('holder pw');
    assert.equal((await holder).status, 401);
    for (const r of await Promise.all(queued)) assert.equal(r.status, 401);
    // Once its waiters are served, the source may queue again; the reset link survived.
    assert.equal((await s.request('POST', LOGIN, { body: { login: 'alice', password: PW }, ip: '198.51.100.7' })).status, 200);
    assert.equal((await s.request('POST', '/api/v1/auth/password/reset', { body: { token, newPassword: NEW }, ip: '198.51.100.7' })).status, 200);
});

test('the auth rate limit also applies to each IPv6 /48 as a whole (AUTH_RATE_PER_PREFIX)', async (t) => {
    const FORGOT = '/api/v1/auth/password/forgot';
    const s = await startTestServer({ env: { AUTH_RATE_PER_IP: '3', AUTH_RATE_PER_PREFIX: '5' } });
    t.after(s.close);
    const send = (ip) => s.request('POST', FORGOT, { body: { email: 'someone@example.com' }, ip });
    // 5 requests from 5 /64s of one /48 pass; the next /64 is refused although it sent nothing.
    for (let i = 0; i < 5; i++) assert.equal((await send(`2001:db8:5:${i}::1`)).status, 202, `/64 number ${i}`);
    const r = await send('2001:db8:5:99::1');
    assert.equal(r.status, 429, r.text);
    assert.equal(r.json.error, 'rate_limited');
    assert.ok(r.json.retryAfter >= 1 && r.headers['retry-after'] === String(r.json.retryAfter));
    // Another /48 and IPv4 addresses are not concerned (IPv4 is limited per address only).
    assert.equal((await send('2001:db8:6::1')).status, 202);
    for (let i = 1; i <= 6; i++) assert.equal((await send(`192.0.2.${i}`)).status, 202);
    // The per-/64 limit still holds on its own.
    for (let i = 0; i < 3; i++) assert.equal((await send('2001:db8:7:1::1')).status, 202);
    assert.equal((await send('2001:db8:7:1::2')).status, 429);
    // The default is 5 x AUTH_RATE_PER_IP.
    assert.equal(testConfig({ AUTH_RATE_PER_IP: '7' }).authRatePerPrefix, 35);
    assert.equal(testConfig({ AUTH_RATE_PER_IP: '7', AUTH_RATE_PER_PREFIX: '9' }).authRatePerPrefix, 9);
});

test('an idle worker: 10 simultaneous logins from one IPv4 address (a classroom) all get in', async (t) => {
    const g = gatedHasher();
    const s = await startTestServer({ hasher: g.hasher, env: { AUTH_RATE_PER_IP: '20' } });
    t.after(async () => { g.open(); await s.close(); });
    for (let i = 0; i < 10; i++) await s.createUser({ username: `pupil${i}`, password: PW });
    const limiter = await idleLimiter(s);
    const src0 = rejected('source_limit');
    g.close();
    const logins = Array.from({ length: 10 }, (_, i) => s.request('POST', LOGIN, { body: { login: `pupil${i}`, password: PW }, ip: '198.51.100.7' }));
    // One check runs, the 9 others wait (beyond the per-source cap of 2: the queue of 32 is far from half full).
    await waitFor(() => limiter.stats().active === 1 && limiter.stats().waiting === 9, '1 running and 9 waiting');
    assert.equal(limiter.waitingFrom('198.51.100.7'), 9);
    g.open();
    const all = await Promise.all(logins);
    assert.deepEqual(all.map((r) => r.status), Array(10).fill(200), all.map((r) => r.text).join('\n'));
    assert.equal(rejected('source_limit'), src0);
});

test('a request refused for its source gives its auth rate tokens back (local and shared buckets, IPv6 /48 too)', async (t) => {
    const k = keyedHasher();
    // A queue of 2 (contended from 1 waiting on), 1 waiter per source, 3 attempts per 10 minutes.
    const s = await startTestServer({
        hasher: k.hasher,
        env: { AUTH_RATE_PER_IP: '3', AUTH_RATE_PER_PREFIX: '3', PASSWORD_HASH_QUEUE_MAX: '2', PASSWORD_HASH_WAITERS_PER_SOURCE: '1' },
    });
    t.after(async () => { k.releaseAll(); await s.close(); });
    const limiter = await idleLimiter(s);
    assert.equal(limiter.perSourceMax, 1);
    k.hold('holder pw');
    const holder = s.request('POST', LOGIN, { body: { login: 'ghost', password: 'holder pw' }, ip: '192.0.2.1' });
    await waitFor(() => limiter.stats().active === 1 && k.blocked === 1, 'the holder to take the slot');
    let n = 0;          // a new login each time, so that no account's failure delay starts
    const send = (ip) => s.request('POST', LOGIN, { body: { login: `nobody${n++}`, password: 'not the password' }, ip });
    const isSourceLimit = (r) => r.status === 429 && r.json.error === 'rate_limited' && r.json.retryAfter >= 5 && r.json.retryAfter <= 15;

    // IPv4: one login waits (1 of the 3 tokens); 4 more are refused for the source, and each gives
    // its token back, so none of them hits the rate limit (its Retry-After would be 200 s).
    const A = '198.51.100.7';
    const waiterA = send(A);
    await waitFor(() => limiter.stats().waiting === 1, 'the waiter of A');
    for (let i = 0; i < 4; i++) {
        const r = await send(A);
        assert.ok(isSourceLimit(r), `refusal ${i}: ${r.status} ${r.text}`);
    }
    // IPv6: the /64s of one /48 are one source, and the /48 bucket is refunded as well.
    const waiterX = send('2001:db8:9:1::1');
    await waitFor(() => limiter.stats().waiting === 2, 'the waiter of the /48');
    for (let i = 0; i < 4; i++) {
        const r = await send('2001:db8:9:2::1');
        assert.ok(isSourceLimit(r), `/48 refusal ${i}: ${r.status} ${r.text}`);
    }
    const refunds = s.primary.calls.filter((c) => c.type === 'ratelimit.refund');
    assert.equal(refunds.length, 4 + 8, 'one shared refund per token taken: the address, or the /64 and the /48');
    assert.ok(refunds.every((c) => c.payload.windowMs === 600000 && c.payload.cost === 1 && c.payload.ageMs >= 0));
    assert.deepEqual([...new Set(refunds.map((c) => c.payload.key))].sort(),
        ['auth/48:2001:db8:9::/48', 'auth:198.51.100.7', 'auth:2001:db8:9:2::/64']);
    // A request refused by the queue itself (queue full) keeps its token: it is no source refusal.
    const full = await send('203.0.113.50');
    assert.equal(full.status, 503);
    assert.equal(s.primary.calls.filter((c) => c.type === 'ratelimit.refund').length, 12);

    k.release('holder pw');
    assert.equal((await holder).status, 401);
    assert.deepEqual([(await waiterA).status, (await waiterX).status], [401, 401]);
    // A still has its 2 other attempts, the /48 too; then the limit of 3 applies (local and shared).
    assert.deepEqual([(await send(A)).status, (await send(A)).status], [401, 401]);
    const over = await send(A);
    assert.equal(over.status, 429);
    assert.ok(over.json.retryAfter > 15, `the rate limit's own Retry-After: ${over.json.retryAfter}`);
    assert.deepEqual([(await send('2001:db8:9:3::1')).status, (await send('2001:db8:9:4::1')).status], [401, 401]);
    const over48 = await send('2001:db8:9:5::1');
    assert.equal(over48.status, 429);
    assert.ok(over48.json.retryAfter > 15, `the /48 limit's own Retry-After: ${over48.json.retryAfter}`);
});

test('the first failed login of a fresh worker is padded to the warm-up baseline: an unknown account takes as long as a slow stored hash', async (t) => {
    // The dummy uses the preferred algorithm (fast here), a stored hash an older, slower one; the
    // warm-up reports the slowest verification it timed (the real hasher times the legacy scrypt).
    const SLOW = 150, FAST = 20;
    const fake = {
        algorithm: 'fast', parse: () => null,
        hash: async (pw) => { await sleep(FAST); return `fast:${pw}`; },
        verify: async (stored, pw) => { await sleep(stored.startsWith('slow:') ? SLOW : FAST); return { ok: stored.slice(5) === pw, needsRehash: false }; },
        verifyDummy: async () => { await sleep(FAST); return false; },
        warmUp: async () => { await sleep(SLOW); return SLOW; },
    };
    const s = await startTestServer({ hasher: fake });
    t.after(s.close);
    s.store.users.create({ username: 'dormant', email: 'dormant@example.com', passwordHash: 'slow:the right one', emailVerified: true });
    const hasher = s.auth._svc.hasher;
    await waitFor(() => hasher.floor.baselineMs() > 0 && hasher.limiter.stats().active === 0, 'the warm-up');
    const time = async (login) => {
        const t0 = performance.now();
        const r = await s.request('POST', LOGIN, { body: { login, password: 'a wrong one' } });
        assert.equal(r.status, 401);
        return performance.now() - t0;
    };
    // No earlier check, no medians: the very first failure of the worker, then the slow one.
    const unknown = await time('ghost@example.com');
    const known = await time('dormant@example.com');
    assert.ok(unknown >= known - 30, `first failure: unknown account ${unknown.toFixed(0)} ms, scrypt-like stored hash ${known.toFixed(0)} ms`);
    assert.ok(unknown >= SLOW - 5, `unknown ${unknown.toFixed(0)} ms`);
});

test('a failed login takes as long for an unknown account as for a stored hash of a slower algorithm', async (t) => {
    // The dummy uses the preferred algorithm (fast here), stored hashes an older, slower one.
    const SLOW = 150, FAST = 20;
    const fake = {
        algorithm: 'fast', parse: () => null,
        hash: async (pw) => { await sleep(FAST); return `fast:${pw}`; },
        verify: async (stored, pw) => { await sleep(stored.startsWith('slow:') ? SLOW : FAST); return { ok: stored.slice(5) === pw, needsRehash: false }; },
        verifyDummy: async () => { await sleep(FAST); return false; },
        warmUp: async () => {},
    };
    const s = await startTestServer({ hasher: fake });
    t.after(s.close);
    for (let i = 0; i < 8; i++) s.store.users.create({ username: `user${i}`, email: `user${i}@example.com`, passwordHash: 'slow:the right one', emailVerified: true });
    const time = async (login) => {
        const t0 = performance.now();
        const r = await s.request('POST', LOGIN, { body: { login, password: 'a wrong one' } });
        assert.equal(r.status, 401);
        return performance.now() - t0;
    };
    const known = [], unknown = [];
    for (let i = 0; i < 8; i++) { known.push(await time(`user${i}@example.com`)); unknown.push(await time(`ghost${i}@example.com`)); }
    const med = (a) => [...a].sort((x, y) => x - y)[4];
    const detail = `known ${known.map((x) => x.toFixed(0))} / unknown ${unknown.map((x) => x.toFixed(0))} ms`;
    assert.ok(med(unknown) >= SLOW - 10, `unknown accounts are padded to the slow check: ${detail}`);
    assert.ok(Math.abs(med(known) - med(unknown)) < 50, detail);
    // A successful login is not padded (and never needed to be).
    const t0 = performance.now();
    assert.equal((await s.request('POST', LOGIN, { body: { login: 'user0', password: 'the right one' } })).status, 200);
    assert.ok(performance.now() - t0 < SLOW + 400);
});

test('config: PASSWORD_HASH_QUEUE_TIMEOUT_MS at most 13000; a warning when the hash concurrency fills the thread pool', () => {
    assert.equal(testConfig({ PASSWORD_HASH_QUEUE_TIMEOUT_MS: '13000' }).passwordHashQueueTimeoutMs, 13000);
    assert.throws(() => testConfig({ PASSWORD_HASH_QUEUE_TIMEOUT_MS: '13001' }), /PASSWORD_HASH_QUEUE_TIMEOUT_MS: at most 13000/);
    assert.deepEqual(configWarnings(testConfig(), {}), []);
    assert.deepEqual(configWarnings(testConfig({ PASSWORD_HASH_CONCURRENCY: '3' }), {}), []);
    const w = configWarnings(testConfig({ PASSWORD_HASH_CONCURRENCY: '4' }), {});
    assert.equal(w.length, 1);
    assert.match(w[0], /PASSWORD_HASH_CONCURRENCY \(4\) is not below the size of the libuv thread pool \(4 threads/);
    assert.deepEqual(configWarnings(testConfig({ PASSWORD_HASH_CONCURRENCY: '4' }), { UV_THREADPOOL_SIZE: '8' }), []);
    assert.equal(configWarnings(testConfig({ PASSWORD_HASH_CONCURRENCY: '2' }), { UV_THREADPOOL_SIZE: '2' }).length, 1);
    // libuv reads the value with atoi(): an unreadable value is 0, which gives 1 thread.
    assert.equal(configWarnings(testConfig({ PASSWORD_HASH_CONCURRENCY: '4' }), { UV_THREADPOOL_SIZE: 'lots' }).length, 1, 'an unreadable value gives libuv 1 thread');
});

test('config: UV_THREADPOOL_SIZE is read as libuv reads it', () => {
    assert.deepEqual(['', '0', 'abc', ' ', '0x10'].map(threadPoolSize), [1, 1, 1, 1, 1], 'empty, 0 or not a number: 1 thread');
    assert.equal(threadPoolSize(undefined), 4, 'unset: 4 threads');
    assert.deepEqual(['8', ' 8', '8 threads', '+3', '1024'].map(threadPoolSize), [8, 8, 8, 3, 1024], 'leading digits, as atoi() reads them');
    assert.deepEqual(['2000', '-1', '-7'].map(threadPoolSize), [1024, 1024, 1024], 'above 1024, or negative (unsigned in libuv): 1024');
    const cfg = testConfig();          // PASSWORD_HASH_CONCURRENCY=1
    for (const v of ['', '0', 'abc']) {
        const w = configWarnings(cfg, { UV_THREADPOOL_SIZE: v });
        assert.equal(w.length, 1, `UV_THREADPOOL_SIZE="${v}"`);
        assert.match(w[0], /PASSWORD_HASH_CONCURRENCY \(1\) is not below the size of the libuv thread pool \(1 thread,/);
    }
    for (const env of [{}, { UV_THREADPOOL_SIZE: '8' }, { UV_THREADPOOL_SIZE: '-1' }, { UV_THREADPOOL_SIZE: '2' }]) {
        assert.deepEqual(configWarnings(cfg, env), [], JSON.stringify(env));
    }
});

test('config: PASSWORD_HASH_WAITERS_PER_SOURCE (default 2, at least 1) reaches the hash limiter', async (t) => {
    assert.equal(testConfig().passwordHashWaitersPerSource, 2);
    assert.equal(testConfig({ PASSWORD_HASH_WAITERS_PER_SOURCE: '1' }).passwordHashWaitersPerSource, 1);
    assert.throws(() => testConfig({ PASSWORD_HASH_WAITERS_PER_SOURCE: '0' }), /PASSWORD_HASH_WAITERS_PER_SOURCE: at least 1/);
    const s = await startTestServer({ env: { PASSWORD_HASH_WAITERS_PER_SOURCE: '6' } });
    t.after(s.close);
    assert.equal(s.auth._svc.hasher.limiter.perSourceMax, 6);
});

test('check-config prints the thread-pool warning on stderr', () => {
    const r = spawnSync(process.execPath, [fileURLToPath(new URL('../../bin/scacelith-server.js', import.meta.url)), 'check-config'], {
        cwd: os.tmpdir(), encoding: 'utf8',
        env: {
            PATH: process.env.PATH, SCACELITH_ENV_FILE: '', SERVER_SECRET: Buffer.alloc(48, 7).toString('base64'),
            TLS_MODE: 'off', ALLOW_INSECURE_DEV: '1', PASSWORD_HASH_CONCURRENCY: '4', UV_THREADPOOL_SIZE: '4',
        },
    });
    assert.equal(r.status, 0, r.stderr);
    assert.equal(JSON.parse(r.stdout).passwordHashConcurrency, 4, 'stdout stays the JSON configuration');
    assert.match(r.stderr, /^warning: PASSWORD_HASH_CONCURRENCY \(4\) is not below the size of the libuv thread pool/m);
    // An empty UV_THREADPOOL_SIZE (a bare line in a systemd EnvironmentFile) gives libuv 1 thread.
    const empty = spawnSync(process.execPath, [fileURLToPath(new URL('../../bin/scacelith-server.js', import.meta.url)), 'check-config'], {
        cwd: os.tmpdir(), encoding: 'utf8',
        env: {
            PATH: process.env.PATH, SCACELITH_ENV_FILE: '', SERVER_SECRET: Buffer.alloc(48, 7).toString('base64'),
            TLS_MODE: 'off', ALLOW_INSECURE_DEV: '1', UV_THREADPOOL_SIZE: '',
        },
    });
    assert.equal(empty.status, 0, empty.stderr);
    assert.match(empty.stderr, /^warning: PASSWORD_HASH_CONCURRENCY \(1\) is not below the size of the libuv thread pool \(1 thread,/m);
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
