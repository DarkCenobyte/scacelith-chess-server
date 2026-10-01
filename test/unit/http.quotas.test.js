// Per-account quotas of the HTTP layer (abuse design 3.5, 3.6): the account's budget across every
// endpoint, `by: 'user'` limits (per account when signed in, per client otherwise), checkRates
// taking its rates all or none, ctx.takeRates and refundRate, binary answers, a route's own
// body limit, the send deadline of large answers, and the close hooks of the route modules.

import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { createApiHandler } from '../../src/http/server.js';
import { register as registerReports } from '../../src/http/routes/reports.js';
import { testConfig } from '../../src/config.js';
import { logger } from '../../src/log.js';
import { metrics } from '../../src/metrics.js';
import { createClock, createFakePrimary, createFakeStore } from './helpers/auth-fakes.js';

const users = { [`sct_${'a'.repeat(43)}`]: 7, [`sct_${'b'.repeat(43)}`]: 8 };
const ALICE = `sct_${'a'.repeat(43)}`, BOB = `sct_${'b'.repeat(43)}`;

function rateLimitedCount(label) {
    const m = metrics.metrics.get('scacelith_http_rate_limited_total');
    const c = m && m.children.get(label);
    return c ? c.value : 0;
}

async function start(env = {}, { routes, sendTimeoutMs } = {}) {
    const config = testConfig({ HTTP_RATE_PER_IP: '100000', HTTP_BODY_LIMIT: '1024', ...env });
    const now = createClock();
    const primary = createFakePrimary({ now });
    const auth = {
        validateToken: (t) => (users[t] ? { userId: users[t], username: `u${users[t]}`, sessionId: users[t], emailVerified: true, tokenHash: 'h' } : null),
    };
    const closed = [];
    const handler = createApiHandler({
        config, store: createFakeStore({ now }), auth, primary, log: logger.child('quota-test'), now, routes, sendTimeoutMs,
        deps: { closed },
    });
    const server = http.createServer((req, res) => { if (req.headers['x-test-ip']) req.clientIp = req.headers['x-test-ip']; handler(req, res); });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const { port } = server.address();
    const req = (method, path, { token, ip, body, raw, contentType } = {}) => new Promise((resolve, reject) => {
        const headers = {};
        if (token) headers.Authorization = `Bearer ${token}`;
        if (ip) headers['X-Test-Ip'] = ip;
        let payload = null;
        if (body !== undefined) { payload = Buffer.from(JSON.stringify(body)); headers['Content-Type'] = 'application/json'; }
        if (raw !== undefined) { payload = Buffer.from(raw); headers['Content-Type'] = contentType || 'application/json'; }
        if (payload) headers['Content-Length'] = payload.length;
        const r = http.request({ host: '127.0.0.1', port, method, path, headers, agent: false }, (res) => {
            const c = [];
            res.on('data', (d) => c.push(d));
            res.on('end', () => {
                const bytes = Buffer.concat(c);
                let json; try { json = JSON.parse(bytes.toString('utf8')); } catch { /* binary */ }
                resolve({ status: res.statusCode, headers: res.headers, bytes, json });
            });
        });
        r.on('error', reject);
        if (payload) r.write(payload);
        r.end();
    });
    return {
        req, port, config, primary, now, handler, closed,
        close: async () => { await handler.close(); server.closeAllConnections?.(); await new Promise((r) => server.close(r)); },
    };
}

function quotaRoutes(router, deps) {
    router.get('/a', () => ({ body: { a: true } }), { auth: 'optional' });
    router.get('/b', () => ({ body: { b: true } }), { auth: 'required' });
    router.get('/byuser', (ctx) => ({ body: { user: ctx.user ? ctx.user.userId : null } }),
        { auth: 'optional', rate: { key: 'pr', limit: 2, windowMs: 60000, by: 'user' } });
    // Two rates: a slow hourly one first, a per-minute one second.
    router.get('/two', () => ({ body: { ok: true } }), {
        rate: [{ key: 'slow', limit: 2, windowMs: 3600000 }, { key: 'fast', limit: 1, windowMs: 60000 }],
    });
    router.get('/two-shared', () => ({ body: { ok: true } }), {
        rate: [{ key: 's1', limit: 5, windowMs: 3600000, shared: true }, { key: 's2', limit: 1, windowMs: 60000, shared: true }],
    });
    // The handler takes more rates; ?refund=1 answers 503 with refundRate (nothing was done).
    router.get('/take', async (ctx) => {
        const t = await ctx.takeRates([{ key: 'extra', limit: 1, windowMs: 3600000, shared: true }]);
        if (ctx.query.refund) return { status: 503, body: { error: 'server_busy', message: 'busy', retryAfter: 2 }, headers: { 'Retry-After': '2' }, refundRate: true };
        return { body: { taken: t.map((x) => x.key) } };
    }, { auth: 'required', rate: { key: 'route', limit: 1, windowMs: 3600000, by: 'user' } });
    router.get('/bin', (ctx) => ({ bytes: new Uint8Array([0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0, 1, 2]), contentType: 'image/gif',
        headers: { 'Content-Disposition': 'attachment; filename="x.gif"' }, status: ctx.query.s ? Number(ctx.query.s) : 200 }));
    router.get('/bin-default', () => ({ bytes: Buffer.from([1, 2, 3]) }));
    router.get('/huge', () => ({ bytes: Buffer.alloc(24 << 20, 0x61), contentType: 'application/octet-stream' }));
    router.post('/big-body', (ctx) => ({ body: { n: ctx.body.s.length } }), { body: { s: { type: 'string', max: 100000 } }, bodyLimit: 4096 });
    deps.onClose(() => { deps.closed.push('quota routes'); });
}

test('the account budget: USER_RATE_PER_MIN across every endpoint, each worker its share, after authentication', async (t) => {
    const s = await start({ USER_RATE_PER_MIN: '8' }, { routes: [quotaRoutes] });     // 1 worker: 8 / min, burst 4
    t.after(s.close);
    const before = rateLimitedCount('user');
    for (const p of ['/a', '/b', '/a', '/b']) assert.equal((await s.req('GET', `/api/v1${p}`, { token: ALICE })).status, 200, p);
    let r = await s.req('GET', '/api/v1/a', { token: ALICE });
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited']);
    assert.equal(r.headers['retry-after'], String(r.json.retryAfter));
    assert.equal(rateLimitedCount('user'), before + 1, 'counted as limit="user"');
    assert.equal((await s.req('GET', '/api/v1/b', { token: ALICE, ip: '198.51.100.9' })).status, 429, 'whatever the address');
    assert.equal((await s.req('GET', '/api/v1/a')).status, 200, 'anonymous requests are not counted');
    assert.equal((await s.req('GET', '/api/v1/a', { token: BOB })).status, 200, 'another account');
    assert.equal((await s.req('GET', '/api/v1/b', { token: `sct_${'z'.repeat(43)}` })).status, 401, 'an invalid token is refused first');
    s.now.advance(7500);
    assert.equal((await s.req('GET', '/api/v1/a', { token: ALICE })).status, 200, 'one token every 7.5 s');
    assert.equal((await s.req('GET', '/api/v1/a', { token: ALICE })).status, 429);
    assert.equal(s.primary.calls.filter((c) => c.type === 'ratelimit.take').length, 0, 'no IPC: the budget is local');

    // 4 workers: each allows ceil(2 x 8 / 4) = 4 a minute, a burst of 2.
    const four = await start({ USER_RATE_PER_MIN: '8', WORKERS: '4' }, { routes: [quotaRoutes] });
    t.after(four.close);
    assert.equal((await four.req('GET', '/api/v1/a', { token: ALICE })).status, 200);
    assert.equal((await four.req('GET', '/api/v1/a', { token: ALICE })).status, 200);
    assert.equal((await four.req('GET', '/api/v1/a', { token: ALICE })).status, 429);
});

test('by: "user" limits count per account when signed in, per client otherwise', async (t) => {
    const s = await start({}, { routes: [quotaRoutes] });
    t.after(s.close);
    const ip = '192.0.2.77';
    assert.equal((await s.req('GET', '/api/v1/byuser', { ip })).status, 200);
    assert.equal((await s.req('GET', '/api/v1/byuser', { ip })).status, 200);
    assert.equal((await s.req('GET', '/api/v1/byuser', { ip })).status, 429, 'anonymous: the address');
    assert.equal((await s.req('GET', '/api/v1/byuser', { ip, token: ALICE })).json.user, 7, 'signed in: the account, not the address');
    assert.equal((await s.req('GET', '/api/v1/byuser', { ip: '192.0.2.78', token: ALICE })).status, 200);
    assert.equal((await s.req('GET', '/api/v1/byuser', { ip: '192.0.2.79', token: ALICE })).status, 429, 'the account from any address');
    assert.equal((await s.req('GET', '/api/v1/byuser', { ip, token: BOB })).status, 200, 'another account behind the same address');

    // POST /reports counts per player.
    const routes = {};
    registerReports({ post: (p, h, o) => { routes[p] = o; } }, {});
    assert.deepEqual(routes['/api/v1/reports'].rate, { key: 'reports', limit: 30, windowMs: 3600000, by: 'user' });
});

test('checkRates: a refusal by a later rate gives back the tokens of the earlier ones (local and shared)', async (t) => {
    const s = await start({}, { routes: [quotaRoutes] });
    t.after(s.close);
    assert.equal((await s.req('GET', '/api/v1/two')).status, 200);       // slow 1 left, fast 0
    for (let i = 0; i < 4; i++) {
        const r = await s.req('GET', '/api/v1/two');
        assert.equal(r.status, 429, 'refused by "fast"');
    }
    s.now.advance(60000);                                                // "fast" refilled; "slow" barely
    assert.equal((await s.req('GET', '/api/v1/two')).status, 200, '"slow" kept its token through the refusals');
    s.now.advance(60000);
    assert.equal((await s.req('GET', '/api/v1/two')).status, 429, 'now "slow" is spent');
    assert.ok(rateLimitedCount('slow') >= 1 && rateLimitedCount('fast') >= 4);

    // Shared: the primary gets the earlier token back too.
    assert.equal((await s.req('GET', '/api/v1/two-shared', { ip: '203.0.113.1' })).status, 200);
    s.now.advance(60000);                                                // the local stage of s2 has a token again
    s.primary.refuse = (type, p) => (type === 'ratelimit.take' && p.key.startsWith('s2:') ? { allowed: false, retryAfterMs: 4000, count: 1 } : undefined);
    const r = await s.req('GET', '/api/v1/two-shared', { ip: '203.0.113.1' });
    assert.deepEqual([r.status, r.json.retryAfter], [429, 4]);
    s.primary.refuse = null;
    const refunds = s.primary.calls.filter((c) => c.type === 'ratelimit.refund');
    assert.deepEqual(refunds.map((c) => c.payload.key), ['s1:203.0.113.1']);
    assert.equal(refunds[0].payload.windowMs, 3600000);
});

test('ctx.takeRates: the handler\'s rates join the request\'s tokens; refundRate gives every one back', async (t) => {
    const s = await start({}, { routes: [quotaRoutes] });
    t.after(s.close);
    let r = await s.req('GET', '/api/v1/take?refund=1', { token: ALICE });
    assert.deepEqual([r.status, r.json.error, r.headers['retry-after']], [503, 'server_busy', '2']);
    assert.ok(s.primary.calls.some((c) => c.type === 'ratelimit.refund' && c.payload.key === 'extra:127.0.0.1'), 'the shared rate is refunded');
    r = await s.req('GET', '/api/v1/take', { token: ALICE });
    assert.equal(r.status, 200, 'both the route\'s token and the handler\'s were given back');
    assert.deepEqual(r.json.taken, ['extra:127.0.0.1']);
    r = await s.req('GET', '/api/v1/take', { token: ALICE });
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited'], 'the route limit (1 per hour per account)');
    r = await s.req('GET', '/api/v1/take', { token: BOB });
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited'], 'the handler\'s rate (1 per hour per address)');
    assert.ok(rateLimitedCount('extra') >= 1);
    // The handler had started (a GIF's options and PGN read): the route's token stays spent.
    r = await s.req('GET', '/api/v1/take', { token: BOB, ip: '198.51.100.3' });
    assert.deepEqual([r.status, r.json.error], [429, 'rate_limited']);
    s.now.advance(3600000 + 1000);
    assert.equal((await s.req('GET', '/api/v1/take', { token: BOB, ip: '198.51.100.3' })).status, 200);
});

test('binary answers: bytes, content type, length, security headers, HEAD', async (t) => {
    const s = await start({}, { routes: [quotaRoutes] });
    t.after(s.close);
    let r = await s.req('GET', '/api/v1/bin');
    assert.equal(r.status, 200);
    assert.deepEqual([...r.bytes], [0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0, 1, 2]);
    assert.equal(r.headers['content-type'], 'image/gif');
    assert.equal(r.headers['content-length'], '9');
    assert.equal(r.headers['content-disposition'], 'attachment; filename="x.gif"');
    assert.equal(r.headers['content-security-policy'], "default-src 'none'; frame-ancestors 'none'");
    assert.equal(r.headers['x-content-type-options'], 'nosniff');
    assert.equal(r.headers['cache-control'], 'no-store');
    r = await s.req('HEAD', '/api/v1/bin');
    assert.deepEqual([r.status, r.bytes.length, r.headers['content-length']], [200, 0, '9']);
    r = await s.req('GET', '/api/v1/bin-default');
    assert.equal(r.headers['content-type'], 'application/octet-stream');
    r = await s.req('GET', '/api/v1/bin?s=204');
    assert.deepEqual([r.status, r.bytes.length], [204, 0]);
});

test('a route\'s bodyLimit replaces HTTP_BODY_LIMIT', async (t) => {
    const s = await start({ HTTP_BODY_LIMIT: '1024' }, { routes: [quotaRoutes] });
    t.after(s.close);
    let r = await s.req('POST', '/api/v1/big-body', { body: { s: 'x'.repeat(3000) } });
    assert.deepEqual([r.status, r.json.n], [200, 3000]);
    r = await s.req('POST', '/api/v1/big-body', { body: { s: 'x'.repeat(5000) } });
    assert.deepEqual([r.status, r.json.error], [413, 'payload_too_large']);
});

test('a large answer must leave within the send deadline: a client that stops reading loses the socket', async (t) => {
    const s = await start({}, { routes: [quotaRoutes], sendTimeoutMs: 300 });
    t.after(s.close);
    const outcome = await new Promise((resolve) => {
        let got = 0;
        const done = (how) => resolve({ how, got });
        const r = http.request({ host: '127.0.0.1', port: s.port, method: 'GET', path: '/api/v1/huge', agent: false }, (res) => {
            res.pause();                                    // stops reading: the server's writes stall
            res.on('data', (c) => { got += c.length; });
            res.on('aborted', () => done('aborted'));
            res.on('error', () => done('error'));
            res.on('close', () => done(res.complete ? 'complete' : 'closed'));
            // Long after the deadline, read what the kernel kept: the answer ends short.
            setTimeout(() => res.resume(), 1500);
        });
        r.on('error', () => done('error'));
        r.end();
    });
    assert.notEqual(outcome.how, 'complete');
    assert.ok(outcome.got < 24 << 20, `${outcome.got} bytes of ${24 << 20}`);
    // A client that reads gets all of it.
    const r = await s.req('GET', '/api/v1/huge');
    assert.equal(r.bytes.length, 24 << 20);
});

test('handle.close() runs the close hooks of the route modules once', async () => {
    const s = await start({}, { routes: [quotaRoutes] });
    await s.close();
    await s.handler.close();
    assert.deepEqual(s.closed, ['quota routes']);
});
