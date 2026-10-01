// The refusals of the API's route limits and the protection per address (src/http/server.js
// routeRefusal and sendError, src/net/ipguard.js IpGuard.noteRefusal): a 429 of a limit keyed by
// the client's address counts toward a block of that address, with the rate's `abuseWeight` (5 for
// the login, registration, reset and MFA family) or 1, from the local bucket and from the
// primary's shared window alike, and for an IPv6 /48 sub-limit too; a limit keyed by a signed-in
// account never counts (an anonymous request on such a route is keyed by its address and does);
// nor does a 429 that is not a route limit (the password hash queue's), nor anything without a
// guard.

import assert from 'node:assert/strict';
import http from 'node:http';
import test from 'node:test';
import { testConfig } from '../../src/config.js';
import { HttpError, createApiHandler } from '../../src/http/server.js';
import { logger } from '../../src/log.js';
import { Registry } from '../../src/metrics.js';
import { IpGuard } from '../../src/net/ipguard.js';
import { createClock, createFakePrimary, createFakeStore } from './helpers/auth-fakes.js';

const TOKEN = 'sct_' + 'b'.repeat(43);

function routes(router) {
    router.get('/ip', () => ({ body: { ok: true } }), { rate: { key: 'ipr', limit: 1, windowMs: 60000 } });
    router.post('/login', () => ({ body: { ok: true } }), {
        rate: { key: 'auth', limit: 1, windowMs: 600000, shared: true, prefixLimit: 5, abuseWeight: 5 },
    });
    router.post('/v6', () => ({ body: { ok: true } }), {
        rate: { key: 'v6', limit: 5, windowMs: 600000, prefixLimit: 1, abuseWeight: 5 },
    });
    router.get('/remote', () => ({ body: { ok: true } }), { rate: { key: 'remote', limit: 100, windowMs: 60000, shared: true } });
    router.get('/mine', () => ({ body: { ok: true } }), { auth: 'optional', rate: { key: 'pub', limit: 1, windowMs: 60000, by: 'user' } });
    // A 429 that is not a route limit: the password hash queue's, given back (auth/errors.js).
    router.get('/busy', () => { throw Object.assign(new HttpError(429, 'rate_limited', 'Busy.', { retryAfter: 1 }), { refundRate: true }); });
}

async function start({ guard: withGuard = true } = {}) {
    const config = testConfig({ HTTP_RATE_PER_IP: '100000' });
    const now = createClock();
    const primary = createFakePrimary({ now });
    const reports = [];
    const guard = withGuard ? new IpGuard({ config, workers: 1, registry: new Registry(), now, report: (e) => reports.push(e) }) : null;
    const auth = { validateToken: (t) => (t === TOKEN ? { userId: 9, username: 'carol', sessionId: 4, emailVerified: true, tokenHash: 'h' } : null) };
    const handler = createApiHandler({
        config, store: createFakeStore({ now }), auth, primary, log: logger.child('route-refusals-test'), now, routes: [routes], guard,
    });
    const server = http.createServer((req, res) => { req.clientIp = req.headers['x-test-ip']; handler(req, res); });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const { port } = server.address();
    const req = (method, path, ip, headers = {}) => new Promise((resolve, reject) => {
        const r = http.request({ host: '127.0.0.1', port, method, path: `/api/v1${path}`, agent: false, headers: { 'X-Test-Ip': ip, ...headers } }, (res) => {
            res.resume();
            res.on('end', () => resolve(res.statusCode));
        });
        r.on('error', reject);
        r.end();
    });
    // What the guard counted since the last call, as { k64: weight }.
    const counted = () => {
        const out = {};
        for (const [k64, , w] of guard.flushReports()) out[k64] = w;
        return out;
    };
    return { req, counted, primary, reports, close: () => { guard?.close(); return new Promise((r) => server.close(r)); } };
}

test('a 429 of a limit keyed by the address counts toward its block: once, or abuseWeight times', async (t) => {
    const s = await start();
    t.after(s.close);
    assert.equal(await s.req('GET', '/ip', '198.51.100.1'), 200);
    assert.equal(await s.req('GET', '/ip', '198.51.100.1'), 429);
    assert.equal(await s.req('GET', '/ip', '198.51.100.1'), 429);
    assert.equal(await s.req('POST', '/login', '198.51.100.2'), 200);
    assert.equal(await s.req('POST', '/login', '198.51.100.2'), 429, 'the auth family');
    assert.deepEqual(s.counted(), { '198.51.100.1': 2, '198.51.100.2': 5 });
    assert.equal(s.reports.length, 1, 'reported to the primary in one notification');
});

test('a refusal by the primary\'s shared window counts like the local one', async (t) => {
    const s = await start();
    t.after(s.close);
    assert.equal(await s.req('POST', '/login', '198.51.100.3'), 200);
    s.primary.refuse = (type) => (type === 'ratelimit.take' ? { allowed: false, retryAfterMs: 5000 } : undefined);
    assert.equal(await s.req('GET', '/remote', '198.51.100.4'), 429, 'refused by the primary, not by the local bucket');
    assert.equal(await s.req('POST', '/login', '198.51.100.5'), 429);
    assert.deepEqual(s.counted(), { '198.51.100.4': 1, '198.51.100.5': 5 });
});

test('an IPv6 /48 sub-limit counts against the /64 that asked, with its /48', async (t) => {
    const s = await start();
    t.after(s.close);
    assert.equal(await s.req('POST', '/v6', '2001:db8:4:1::1'), 200);
    assert.equal(await s.req('POST', '/v6', '2001:db8:4:2::1'), 429, 'another /64 of the same /48');
    const entries = s.counted();
    assert.deepEqual(entries, { '2001:db8:4:2::/64': 5 });
});

test('a limit keyed by a signed-in account never counts; the same route without a session does', async (t) => {
    const s = await start();
    t.after(s.close);
    const signedIn = { Authorization: `Bearer ${TOKEN}` };
    assert.equal(await s.req('GET', '/mine', '198.51.100.6', signedIn), 200);
    assert.equal(await s.req('GET', '/mine', '198.51.100.6', signedIn), 429, 'the account\'s limit');
    assert.equal(await s.req('GET', '/mine', '198.51.100.6', signedIn), 429);
    assert.deepEqual(s.counted(), {}, 'an account\'s problem, not its network\'s');
    assert.equal(await s.req('GET', '/mine', '198.51.100.7'), 200);
    assert.equal(await s.req('GET', '/mine', '198.51.100.7'), 429, 'anonymous: keyed by the address');
    assert.deepEqual(s.counted(), { '198.51.100.7': 1 });
});

test('a 429 that is not a route limit is not counted; without a guard nothing is', async (t) => {
    const s = await start();
    t.after(s.close);
    for (let i = 0; i < 3; i++) assert.equal(await s.req('GET', '/busy', '198.51.100.8'), 429);
    assert.deepEqual(s.counted(), {});

    const bare = await start({ guard: false });
    t.after(bare.close);
    assert.equal(await bare.req('GET', '/ip', '198.51.100.9'), 200);
    assert.equal(await bare.req('GET', '/ip', '198.51.100.9'), 429, 'the limit itself still applies');
});
