import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { createApiHandler, HttpError } from '../../src/http/server.js';
import { Router, validate } from '../../src/http/router.js';
import { testConfig } from '../../src/config.js';
import { logger } from '../../src/log.js';
import { capturedLogs, createClock, createFakePrimary, createFakeStore } from './helpers/auth-fakes.js';

const TOKEN = 'sct_' + 'a'.repeat(43);

function routesUnderTest(router) {
    router.get('/echo/:name', (ctx) => ({ body: { name: ctx.params.name, query: ctx.query, ip: ctx.ip } }));
    router.get('/echo/special', () => ({ body: { special: true } }));
    router.get('/api/v1/absolute', () => ({ body: { absolute: true } }));
    router.post('/body', (ctx) => ({ status: 201, body: ctx.body }), {
        body: {
            name: { type: 'string', min: 2, max: 8, pattern: /^[a-z]+$/ },
            n: { type: 'integer', min: 1, max: 9, optional: true },
            flag: { type: 'boolean', optional: true },
            kind: { type: 'enum', values: ['a', 'b'], optional: true },
            nested: { type: 'object', optional: true, fields: { x: { type: 'string', max: 3 } } },
            list: { type: 'array', optional: true, max: 2, items: { type: 'number' } },
            note: { type: 'string', max: 50, optional: true, multiline: true },
        },
    });
    router.post('/empty', () => null);
    router.get('/private', (ctx) => ({ body: { user: ctx.user, session: ctx.session } }), { auth: 'required' });
    router.get('/maybe', (ctx) => ({ body: { user: ctx.user ? ctx.user.username : null } }), { auth: 'optional' });
    router.get('/limited', () => ({ body: { ok: true } }), { rate: { key: 'lim', limit: 2, windowMs: 60000 } });
    router.get('/shared', () => ({ body: { ok: true } }), { rate: { key: 'shr', limit: 100, windowMs: 60000, shared: true } });
    router.get('/boom', () => { throw new Error('secret internal detail'); });
    router.get('/teapot', () => { throw new HttpError(418, 'teapot', 'I am a teapot.', { hint: 42 }); });
    router.get('/slow', () => new Promise((r) => setTimeout(() => r({ body: { late: true } }), 400)));
    router.put('/put', (ctx) => ({ body: ctx.body }), { body: { v: { type: 'number' } } });
    router.page('GET', '/page', () => ({ html: '<!DOCTYPE html><p>page</p>' }));
    router.page('POST', '/page', (ctx) => ({ html: `<p>${ctx.body.token}</p>` }), { body: { token: { type: 'string', max: 64 } } });
    router.page('GET', '/page-error', () => { throw new HttpError(400, 'bad', 'Bad <things>.'); });
    router.get('/file', () => ({ text: '[Event "é"]\n\n*\n', contentType: 'application/x-chess-pgn; charset=utf-8',
        headers: { 'Content-Disposition': 'attachment; filename="x.pgn"' } }));
    router.get('/plain', () => ({ status: 202, text: 'plain' }));
}

async function start(env = {}, opts = {}) {
    const config = testConfig({ HTTP_RATE_PER_IP: '1000', HTTP_BODY_LIMIT: '1024', ...env });
    const now = opts.now || createClock();
    const primary = opts.primary === undefined ? createFakePrimary({ now }) : opts.primary;
    const auth = { validateToken: (t) => (t === TOKEN ? { userId: 7, username: 'alice', sessionId: 3, emailVerified: true, tokenHash: 'h' } : null) };
    const handler = createApiHandler({
        config, store: createFakeStore({ now }), auth, primary, log: logger.child('router-test'), now,
        routes: [routesUnderTest], bodyTimeoutMs: 150, handlerTimeoutMs: 150, ready: opts.ready,
    });
    const server = http.createServer((req, res) => { if (req.headers['x-test-ip']) req.clientIp = req.headers['x-test-ip']; handler(req, res); });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const port = server.address().port;
    const req = (method, path, { body, raw, headers = {}, chunks } = {}) => new Promise((resolve, reject) => {
        const h = { ...headers };
        let payload = null;
        if (body !== undefined) { payload = Buffer.from(JSON.stringify(body)); h['Content-Type'] ??= 'application/json'; }
        if (raw !== undefined) payload = Buffer.from(raw);
        if (payload && !chunks) h['Content-Length'] = payload.length;
        const r = http.request({ host: '127.0.0.1', port, method, path, headers: h, agent: false }, (res) => {
            const c = [];
            res.on('data', (d) => c.push(d));
            res.on('end', () => {
                const text = Buffer.concat(c).toString();
                let json; try { json = JSON.parse(text); } catch { /* html or empty */ }
                resolve({ status: res.statusCode, headers: res.headers, text, json });
            });
        });
        r.on('error', reject);
        if (chunks) { (async () => { for (const ch of chunks) { r.write(ch); await new Promise((x) => setTimeout(x, 5)); } r.end(); })(); return; }
        if (payload) r.write(payload);
        r.end();
    });
    return { req, port, config, primary, now, close: () => new Promise((r) => { server.closeAllConnections?.(); server.close(r); }) };
}

test('routing: params, literal over parameter, API prefix, absolute paths, 404', async (t) => {
    const s = await start();
    t.after(s.close);
    let r = await s.req('GET', '/api/v1/echo/b%C3%A9b%C3%A9?x=1&x=2&y=z');
    assert.equal(r.status, 200);
    assert.deepEqual(r.json, { name: 'bébé', query: { x: '1', y: 'z' }, ip: '127.0.0.1' });
    assert.deepEqual((await s.req('GET', '/api/v1/echo/special')).json, { special: true });
    assert.deepEqual((await s.req('GET', '/api/v1/echo/special/')).json, { special: true });
    assert.deepEqual((await s.req('GET', '/api/v1/absolute')).json, { absolute: true });
    r = await s.req('GET', '/echo/x');
    assert.equal(r.status, 404);
    assert.deepEqual(r.json, { error: 'not_found', message: 'No such endpoint.' });
    assert.equal((await s.req('GET', '/api/v1/echo/%E0%A4%A')).status, 400);
    assert.equal((await s.req('GET', '//evil/x')).status, 400);
});

test('405 with Allow, OPTIONS without CORS, HEAD without body', async (t) => {
    const s = await start();
    t.after(s.close);
    let r = await s.req('DELETE', '/api/v1/echo/x');
    assert.equal(r.status, 405);
    assert.equal(r.headers.allow, 'GET, HEAD, OPTIONS');
    assert.equal(r.json.error, 'method_not_allowed');
    r = await s.req('OPTIONS', '/api/v1/body', { headers: { Origin: 'https://evil.example', 'Access-Control-Request-Method': 'POST' } });
    assert.equal(r.status, 204);
    assert.equal(r.headers.allow, 'POST, OPTIONS');
    assert.equal(r.headers['access-control-allow-origin'], undefined);
    r = await s.req('HEAD', '/api/v1/echo/x');
    assert.equal(r.status, 200);
    assert.equal(r.text, '');
    assert.ok(+r.headers['content-length'] > 0);
});

test('security headers on every answer; HSTS with native TLS only', async (t) => {
    const s = await start();
    t.after(s.close);
    for (const path of ['/api/v1/echo/x', '/nope', '/page']) {
        const r = await s.req('GET', path);
        assert.equal(r.headers['cache-control'], 'no-store');
        assert.equal(r.headers['x-content-type-options'], 'nosniff');
        assert.equal(r.headers['referrer-policy'], 'no-referrer');
        assert.ok(r.headers['content-security-policy']);
        assert.equal(r.headers['strict-transport-security'], undefined);
    }
    const page = await s.req('GET', '/page');
    assert.match(page.headers['content-type'], /^text\/html; charset=utf-8/);
    assert.equal(page.headers['content-security-policy'], "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'");
    const n = await start({ TLS_MODE: 'native', TLS_CERT_FILE: '/nonexistent/cert.pem', TLS_KEY_FILE: '/nonexistent/key.pem' });
    t.after(n.close);
    assert.equal((await n.req('GET', '/api/v1/echo/x')).headers['strict-transport-security'], 'max-age=31536000');
    assert.equal((await n.req('GET', '/api/v1/file')).headers['strict-transport-security'], 'max-age=31536000');
});

test('text answers: content type, byte length, extra headers, the API security headers, HEAD', async (t) => {
    const s = await start();
    t.after(s.close);
    let r = await s.req('GET', '/api/v1/file');
    assert.equal(r.status, 200);
    assert.equal(r.text, '[Event "é"]\n\n*\n');
    assert.equal(r.headers['content-type'], 'application/x-chess-pgn; charset=utf-8');
    assert.equal(r.headers['content-disposition'], 'attachment; filename="x.pgn"');
    assert.equal(+r.headers['content-length'], Buffer.byteLength('[Event "é"]\n\n*\n'));
    assert.equal(r.headers['content-security-policy'], "default-src 'none'; frame-ancestors 'none'");
    assert.equal(r.headers['cache-control'], 'no-store');
    assert.equal(r.headers['x-content-type-options'], 'nosniff');
    assert.equal(r.headers['x-frame-options'], 'DENY');
    assert.equal(r.headers['referrer-policy'], 'no-referrer');
    r = await s.req('GET', '/api/v1/plain');
    assert.equal(r.status, 202);
    assert.equal(r.text, 'plain');
    assert.equal(r.headers['content-type'], 'text/plain; charset=utf-8');
    r = await s.req('HEAD', '/api/v1/file');
    assert.equal(r.status, 200);
    assert.equal(r.text, '');
    assert.equal(+r.headers['content-length'], Buffer.byteLength('[Event "é"]\n\n*\n'));
});

test('JSON body: 201, 415 wrong type, 400 invalid JSON, charset, empty body allowed only without required fields', async (t) => {
    const s = await start();
    t.after(s.close);
    let r = await s.req('POST', '/api/v1/body', { body: { name: 'abc', n: 3, flag: true, kind: 'b', nested: { x: 'yz' }, list: [1, 2.5], note: 'two\nlines' } });
    assert.equal(r.status, 201);
    assert.deepEqual(r.json, { name: 'abc', n: 3, flag: true, kind: 'b', nested: { x: 'yz' }, list: [1, 2.5], note: 'two\nlines' });
    r = await s.req('POST', '/api/v1/body', { raw: 'name=abc', headers: { 'Content-Type': 'application/x-www-form-urlencoded' } });
    assert.equal(r.status, 415);
    assert.equal(r.json.error, 'unsupported_media_type');
    r = await s.req('POST', '/api/v1/body', { raw: '{"name":"abc"}', headers: { 'Content-Type': 'text/plain' } });
    assert.equal(r.status, 415, 'simple cross-site requests are refused');
    r = await s.req('POST', '/api/v1/body', { raw: '{"name":"abc"}', headers: { 'Content-Type': 'application/json; charset=latin1' } });
    assert.equal(r.status, 415);
    r = await s.req('POST', '/api/v1/body', { raw: '{"name":"abc"}', headers: { 'Content-Type': 'application/json; charset=UTF-8' } });
    assert.equal(r.status, 201);
    r = await s.req('POST', '/api/v1/body', { raw: '{"name":', headers: { 'Content-Type': 'application/json' } });
    assert.deepEqual([r.status, r.json.error], [400, 'invalid_json']);
    r = await s.req('POST', '/api/v1/body');
    assert.deepEqual([r.status, r.json.error, r.json.field], [400, 'invalid_request', 'name']);
    r = await s.req('POST', '/api/v1/empty');
    assert.equal(r.status, 204);
    r = await s.req('POST', '/api/v1/empty', { body: { x: 1 } });
    assert.equal(r.status, 400, 'a route without schema accepts no fields');
    r = await s.req('PUT', '/api/v1/put', { body: { v: 1.5 } });
    assert.deepEqual(r.json, { v: 1.5 });
});

test('strict schema validation', async (t) => {
    const s = await start();
    t.after(s.close);
    const bad = [
        [{ name: 'abc', extra: 1 }, 'extra'], [{ name: 'a' }, 'name'], [{ name: 'abcdefghi' }, 'name'], [{ name: 'ABC' }, 'name'],
        [{ name: 5 }, 'name'], [{ name: 'abc', n: 1.5 }, 'n'], [{ name: 'abc', n: 10 }, 'n'], [{ name: 'abc', n: '3' }, 'n'],
        [{ name: 'abc', flag: 'yes' }, 'flag'], [{ name: 'abc', kind: 'c' }, 'kind'], [{ name: 'abc', nested: { x: 'long' } }, 'nested.x'],
        [{ name: 'abc', nested: { y: 1 } }, 'nested.y'], [{ name: 'abc', nested: [] }, 'nested'], [{ name: 'abc', list: [1, 2, 3] }, 'list'],
        [{ name: 'abc', list: ['x'] }, 'list[0]'], [{ name: 'ab\u0000c' }, 'name'], [{ name: 'abc', note: 'bell\u0007' }, 'note'],
        [{ name: null }, 'name'],
    ];
    for (const [body, field] of bad) {
        const r = await s.req('POST', '/api/v1/body', { body });
        assert.equal(r.status, 400, JSON.stringify(body));
        assert.equal(r.json.error, 'invalid_request');
        assert.equal(r.json.field, field, JSON.stringify(body));
    }
    const r = await s.req('POST', '/api/v1/body', { raw: '{"__proto__":{"admin":true},"name":"abc"}', headers: { 'Content-Type': 'application/json' } });
    assert.equal(r.status, 400);
    assert.equal((await s.req('POST', '/api/v1/body', { raw: '[1]', headers: { 'Content-Type': 'application/json' } })).status, 400);
    assert.deepEqual(validate({ a: { type: 'string' } }, { a: 'x' }), { ok: true, value: { a: 'x' } });
});

test('body limit: 413 from Content-Length and while streaming', async (t) => {
    const s = await start();
    t.after(s.close);
    let r = await s.req('POST', '/api/v1/body', { raw: JSON.stringify({ name: 'a'.repeat(2000) }), headers: { 'Content-Type': 'application/json' } });
    assert.equal(r.status, 413);
    assert.equal(r.json.error, 'payload_too_large');
    assert.equal(r.headers.connection, 'close');
    r = await s.req('POST', '/api/v1/body', { chunks: ['{"name":"', 'x'.repeat(600), 'y'.repeat(600), '"}'], headers: { 'Content-Type': 'application/json' } });
    assert.equal(r.status, 413);
});

test('slow bodies time out (408); slow handlers answer 503', async (t) => {
    const s = await start();
    t.after(s.close);
    let pending;
    const slow = await new Promise((resolve, reject) => {
        pending = http.request({ host: '127.0.0.1', port: s.port, method: 'POST', path: '/api/v1/body', agent: false,
            headers: { 'Content-Type': 'application/json', 'Content-Length': 100 } }, (res) => {
            const c = [];
            res.on('data', (d) => c.push(d));
            res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, json: JSON.parse(Buffer.concat(c).toString()) }));
        });
        pending.on('error', reject);
        pending.write('{"name":');          // the rest never comes
    });
    pending.destroy();
    assert.equal(slow.status, 408);
    assert.equal(slow.json.error, 'request_timeout');
    assert.equal(slow.headers.connection, 'close');
    const res = await s.req('GET', '/api/v1/slow');
    assert.equal(res.status, 503);
    assert.equal(res.json.error, 'timeout');
});

test('authentication modes', async (t) => {
    const s = await start();
    t.after(s.close);
    let r = await s.req('GET', '/api/v1/private');
    assert.equal(r.status, 401);
    assert.equal(r.json.error, 'unauthorized');
    assert.match(r.headers['www-authenticate'], /^Bearer/);
    r = await s.req('GET', '/api/v1/private', { headers: { Authorization: 'Bearer sct_' + 'b'.repeat(43) } });
    assert.deepEqual([r.status, r.json.error], [401, 'invalid_token']);
    r = await s.req('GET', '/api/v1/private', { headers: { Authorization: 'Basic abc' } });
    assert.equal(r.status, 401);
    r = await s.req('GET', '/api/v1/private', { headers: { Authorization: `Bearer ${TOKEN}` } });
    assert.equal(r.status, 200);
    assert.deepEqual(r.json, { user: { id: 7, userId: 7, username: 'alice', emailVerified: true }, session: { id: 3, tokenHash: 'h' } });
    assert.deepEqual((await s.req('GET', '/api/v1/maybe')).json, { user: null });
    assert.deepEqual((await s.req('GET', '/api/v1/maybe', { headers: { Authorization: `Bearer ${TOKEN}` } })).json, { user: 'alice' });
    assert.equal((await s.req('GET', '/api/v1/maybe', { headers: { Authorization: 'Bearer nope' } })).status, 401);
});

test('rate limits: global per IP, per route, shared through the primary, fail open', async (t) => {
    const s = await start({ HTTP_RATE_PER_IP: '5' });
    t.after(s.close);
    for (let i = 0; i < 5; i++) assert.equal((await s.req('GET', '/api/v1/echo/x', { headers: { 'X-Test-Ip': '198.51.100.1' } })).status, 200);
    const r = await s.req('GET', '/api/v1/echo/x', { headers: { 'X-Test-Ip': '198.51.100.1' } });
    assert.equal(r.status, 429);
    assert.equal(r.json.error, 'rate_limited');
    assert.equal(r.headers['retry-after'], String(r.json.retryAfter));
    assert.equal((await s.req('GET', '/api/v1/echo/x', { headers: { 'X-Test-Ip': '198.51.100.2' } })).status, 200, 'another client');
    assert.equal((await s.req('GET', '/healthz', { headers: { 'X-Test-Ip': '198.51.100.1' } })).status, 200, 'health is not limited');
    s.now.advance(60000);
    assert.equal((await s.req('GET', '/api/v1/echo/x', { headers: { 'X-Test-Ip': '198.51.100.1' } })).status, 200, 'refilled');

    const q = await start();
    t.after(q.close);
    assert.equal((await q.req('GET', '/api/v1/limited')).status, 200);
    assert.equal((await q.req('GET', '/api/v1/limited')).status, 200);
    assert.equal((await q.req('GET', '/api/v1/limited')).status, 429);
    assert.equal(q.primary.calls.filter((c) => c.type === 'ratelimit.take').length, 0, 'local limits never ask the primary');
    assert.equal((await q.req('GET', '/api/v1/shared', { headers: { 'X-Test-Ip': '2001:db8::1' } })).status, 200);
    const call = q.primary.calls.find((c) => c.type === 'ratelimit.take');
    assert.deepEqual(call.payload, { key: 'shr:2001:db8:0:0::/64', limit: 100, windowMs: 60000, cost: 1 });
    q.primary.refuse = (type) => (type === 'ratelimit.take' ? { allowed: false, retryAfterMs: 5000, count: 101 } : undefined);
    const refused = await q.req('GET', '/api/v1/shared');
    assert.deepEqual([refused.status, refused.json.retryAfter], [429, 5]);
    q.primary.refuse = null;
    q.primary.failing = true;
    assert.equal((await q.req('GET', '/api/v1/shared')).status, 200, 'fail open when the primary does not answer');
});

test('errors: uniform JSON, internal details hidden, HTML errors on pages', async (t) => {
    const s = await start();
    t.after(s.close);
    let r = await s.req('GET', '/api/v1/boom');
    assert.equal(r.status, 500);
    assert.deepEqual(r.json, { error: 'internal_error', message: 'Internal server error.' });
    assert.ok(capturedLogs.some((l) => l.includes('request failed') && l.includes('secret internal detail')));
    r = await s.req('GET', '/api/v1/teapot');
    assert.deepEqual([r.status, r.json], [418, { error: 'teapot', message: 'I am a teapot.', hint: 42 }]);
    r = await s.req('GET', '/page-error');
    assert.equal(r.status, 400);
    assert.match(r.headers['content-type'], /text\/html/);
    assert.match(r.text, /Bad &lt;things&gt;\./);
});

test('page forms: urlencoded bodies, duplicates refused', async (t) => {
    const s = await start();
    t.after(s.close);
    let r = await s.req('POST', '/page', { raw: 'token=abc%2Bdef', headers: { 'Content-Type': 'application/x-www-form-urlencoded' } });
    assert.equal(r.status, 200);
    assert.equal(r.text, '<p>abc+def</p>');
    r = await s.req('POST', '/page', { raw: 'token=a&token=b', headers: { 'Content-Type': 'application/x-www-form-urlencoded' } });
    assert.equal(r.status, 400);
    r = await s.req('POST', '/page', { raw: 'token=a&x=1', headers: { 'Content-Type': 'application/x-www-form-urlencoded' } });
    assert.equal(r.status, 400);
});

test('health and readiness', async (t) => {
    let ready = false;
    const s = await start({}, { ready: () => ready });
    t.after(s.close);
    assert.deepEqual((await s.req('GET', '/healthz')).json, { status: 'ok' });
    assert.deepEqual((await s.req('GET', '/api/v1/healthz')).json, { status: 'ok' });
    assert.equal((await s.req('GET', '/readyz')).status, 503);
    ready = true;
    assert.equal((await s.req('GET', '/readyz')).status, 200);
    assert.equal((await s.req('POST', '/healthz')).status, 405);
});

test('router: duplicate routes refused, other owners\' modules plug in', () => {
    const r = new Router();
    r.get('/players/:username', () => null);
    assert.throws(() => r.get('/api/v1/players/:username', () => null), /twice/);
    assert.ok(r.match('GET', '/api/v1/players/bob').route);
    assert.deepEqual(r.match('POST', '/api/v1/players/bob'), { methods: ['GET'] });
    assert.equal(r.match('GET', '/players/bob'), null);
    assert.throws(() => createApiHandler({ config: testConfig(), store: {}, auth: {}, log: logger, routes: [{}] }), /register/);
});
