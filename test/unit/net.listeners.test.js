import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import tls from 'node:tls';
import { after, before, describe, it } from 'node:test';
import { testConfig } from '../../src/config.js';
import { Registry } from '../../src/metrics.js';
import { ipGroupKey, ipMatcher, normalizeIp, resolveClientIp } from '../../src/net/ip.js';
import { IpGuard } from '../../src/net/ipguard.js';
import { Listeners, deriveTicketKeys, makeClientIp } from '../../src/net/listeners.js';
import { WsServer } from '../../src/net/ws.js';
import { connectWs } from '../../src/net/ws-raw-client.js';

function freePort() {
    return new Promise((resolve) => {
        const s = net.createServer();
        s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); });
    });
}

function get(port, p, headers = {}) {
    return new Promise((resolve, reject) => {
        http.get({ host: '127.0.0.1', port, path: p, headers }, (res) => {
            let body = '';
            res.on('data', (d) => { body += d; });
            res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body }));
        }).on('error', reject);
    });
}

describe('ip helpers', () => {
    it('normalises addresses', () => {
        assert.equal(normalizeIp('::ffff:10.1.2.3'), '10.1.2.3');
        assert.equal(normalizeIp('[2001:DB8::1]'), '2001:db8::1');
        assert.equal(normalizeIp('fe80::1%eth0'), 'fe80::1');
        assert.equal(normalizeIp('not an ip'), '');
    });
    it('groups IPv6 by /64 and keeps IPv4 whole', () => {
        assert.equal(ipGroupKey('2001:db8:1:2:aaaa::1'), ipGroupKey('2001:db8:1:2:bbbb:cccc:dddd:eeee'));
        assert.notEqual(ipGroupKey('2001:db8:1:2::1'), ipGroupKey('2001:db8:1:3::1'));
        assert.equal(ipGroupKey('::ffff:192.0.2.7'), '192.0.2.7');
        assert.notEqual(ipGroupKey('192.0.2.7'), ipGroupKey('192.0.2.8'));
    });
    it('groups IPv6 by /48 on request (TLS admission gate)', () => {
        assert.equal(ipGroupKey('2001:db8:1:2::1', 48), ipGroupKey('2001:db8:1:ffff:aaaa::1', 48));
        assert.notEqual(ipGroupKey('2001:db8:1::1', 48), ipGroupKey('2001:db8:2::1', 48));
        assert.notEqual(ipGroupKey('2001:db8:1::1', 48), ipGroupKey('2001:db8:1::1'), 'a /48 key is not a /64 key');
        assert.equal(ipGroupKey('::ffff:192.0.2.7', 48), '192.0.2.7');
        assert.equal(ipGroupKey('', 48), 'unknown');
    });
    it('matches addresses and subnets, and rejects bad entries', () => {
        const m = ipMatcher(['127.0.0.1', '10.0.0.0/8', 'fd00::/8']);
        assert.ok(m('127.0.0.1') && m('::ffff:127.0.0.1') && m('10.200.1.1') && m('fd12::5'));
        assert.ok(!m('11.0.0.1') && !m('2001:db8::1') && !m('garbage'));
        assert.throws(() => ipMatcher(['10.0.0.0/33']));
        assert.throws(() => ipMatcher(['nope']));
        assert.equal(ipMatcher([])('127.0.0.1'), false);
    });
    it('reads X-Forwarded-For only from trusted proxies, right to left', () => {
        const trusted = ipMatcher(['127.0.0.1', '10.0.0.0/8']);
        assert.equal(resolveClientIp('203.0.113.9', '1.2.3.4', trusted), '203.0.113.9');         // untrusted peer
        assert.equal(resolveClientIp('127.0.0.1', '1.2.3.4', trusted), '1.2.3.4');
        assert.equal(resolveClientIp('127.0.0.1', '6.6.6.6, 1.2.3.4, 10.0.0.2', trusted), '1.2.3.4'); // forged left part ignored
        assert.equal(resolveClientIp('127.0.0.1', 'junk, 10.0.0.2', trusted), '10.0.0.2');
        assert.equal(resolveClientIp('127.0.0.1', undefined, trusted), '127.0.0.1');
    });
    it('makeClientIp trusts proxies only in proxy mode', () => {
        const req = { headers: { 'x-forwarded-for': '198.51.100.1' } };
        const sock = { remoteAddress: '::ffff:127.0.0.1' };
        assert.equal(makeClientIp(testConfig({ TLS_MODE: 'off' }))(req, sock), '127.0.0.1');
        const cfg = { ...testConfig(), tlsMode: 'proxy', trustedProxies: ['127.0.0.1'] };
        assert.equal(makeClientIp(cfg)(req, sock), '198.51.100.1');
    });
});

describe('listeners (plain)', () => {
    let lst, wss, apiPort, wsPort;
    const seen = [];
    before(async () => {
        apiPort = await freePort();
        wsPort = await freePort();
        const config = { ...testConfig(), tlsMode: 'proxy', trustedProxies: ['127.0.0.1'], bindAddress: '127.0.0.1', apiPort, wsPort };
        wss = new WsServer({ registry: new Registry(), clientIp: makeClientIp(config), onConnection: (c) => seen.push(c.ip) });
        lst = new Listeners({ config, wsServer: wss, apiHandler: (req, res) => { res.end(JSON.stringify({ ip: req.clientIp, url: req.url })); }, ready: () => true });
        await lst.listen();
    });
    after(() => { wss.closeAll(); lst.close(); });

    it('serves the API with the proxy-resolved client address', async () => {
        const r = await get(apiPort, '/api/v1/info', { 'X-Forwarded-For': '198.51.100.7' });
        assert.deepEqual(JSON.parse(r.body), { ip: '198.51.100.7', url: '/api/v1/info' });
    });

    it('answers the health endpoints before the API handler', async () => {
        const h = await get(apiPort, '/api/v1/healthz');
        assert.equal(h.status, 200);
        assert.equal(h.headers['cache-control'], 'no-store');
        assert.equal((await get(apiPort, '/readyz')).status, 200);
    });

    it('accepts WebSockets on the dedicated port, with the forwarded address', async () => {
        const c = await connectWs({ port: wsPort, headers: { 'X-Forwarded-For': '192.0.2.44' } });
        await new Promise((r) => setTimeout(r, 20));
        assert.equal(seen.at(-1), '192.0.2.44');
        c.destroy();
    });

    it('does not upgrade on the API port when the ports differ', async () => {
        await assert.rejects(connectWs({ port: apiPort }));
    });
});

describe('listeners (shared port)', () => {
    let lst, wss, port;
    before(async () => {
        port = await freePort();
        const config = { ...testConfig(), bindAddress: '127.0.0.1', apiPort: port, wsPort: port };
        wss = new WsServer({ registry: new Registry(), onConnection: () => {} });
        lst = new Listeners({ config, wsServer: wss, apiHandler: null, ready: () => false });
        await lst.listen();
    });
    after(() => { wss.closeAll(); lst.close(); });

    it('serves the API and the upgrade on one port', async () => {
        assert.equal((await get(port, '/api/v1/readyz')).status, 503);
        assert.equal((await get(port, '/api/v1/nothing')).status, 404);
        const c = await connectWs({ port });
        assert.equal(c.protocol, 'scacelith.v1');
        c.destroy();
    });
});

let hasOpenssl = true;
try { execFileSync('openssl', ['version'], { stdio: 'ignore' }); } catch { hasOpenssl = false; }

function makeCert(dir, name, cn) {
    const key = path.join(dir, `${name}.key`), cert = path.join(dir, `${name}.crt`);
    execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes', '-keyout', key, '-out', cert,
        '-days', '1', '-subj', `/CN=${cn}`], { stdio: 'ignore' });
    return { key, cert };
}

function tlsConnect(port, opts = {}) {
    return new Promise((resolve, reject) => {
        const s = tls.connect({ host: '127.0.0.1', port, rejectUnauthorized: false, ALPNProtocols: ['http/1.1'], ...opts }, () => resolve(s));
        s.on('error', reject);
    });
}

describe('listeners (native TLS)', { skip: !hasOpenssl && 'openssl not available' }, () => {
    let dir, a, b, lst, lst2, wss, apiPort, wsPort, config;
    before(async () => {
        dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-tls-'));
        a = makeCert(dir, 'a', 'first.test');
        b = makeCert(dir, 'b', 'second.test');
        fs.copyFileSync(a.cert, path.join(dir, 'live.crt'));
        fs.copyFileSync(a.key, path.join(dir, 'live.key'));
        apiPort = await freePort();
        wsPort = await freePort();
        config = {
            ...testConfig(), tlsMode: 'native', bindAddress: '127.0.0.1', apiPort, wsPort, tlsMinVersion: 'TLSv1.2',
            tlsCertFile: path.join(dir, 'live.crt'), tlsKeyFile: path.join(dir, 'live.key'),
        };
        wss = new WsServer({ registry: new Registry(), onConnection: () => {} });
        lst = new Listeners({ config, wsServer: wss, apiHandler: null });
        await lst.listen();
    });
    after(() => { wss.closeAll(); lst.close(); lst2?.close(); fs.rmSync(dir, { recursive: true, force: true }); });

    it('negotiates http/1.1 by ALPN and serves WSS on the dedicated port', async () => {
        const s = await tlsConnect(wsPort);
        assert.equal(s.alpnProtocol, 'http/1.1');
        assert.match(s.getPeerCertificate().subject.CN, /first\.test/);
        s.destroy();
        const c = await connectWs({ port: wsPort, tls: { rejectUnauthorized: false } });
        assert.equal(c.protocol, 'scacelith.v1');
        c.destroy();
    });

    it('refuses TLS below the minimum version', async () => {
        await assert.rejects(tlsConnect(wsPort, { maxVersion: 'TLSv1.1', minVersion: 'TLSv1' }));
    });

    it('resumes sessions across listeners that share the derived ticket keys (other workers)', async () => {
        const port2 = await freePort();
        lst2 = new Listeners({ config: { ...config, wsPort: port2, apiPort: await freePort() }, wsServer: wss, apiHandler: null });
        await lst2.listen();
        const s1 = await tlsConnect(wsPort, { maxVersion: 'TLSv1.2' });
        const session = s1.getSession();
        s1.destroy();
        const s2 = await tlsConnect(port2, { maxVersion: 'TLSv1.2', session });
        assert.equal(s2.isSessionReused(), true);
        s2.destroy();
        assert.deepEqual(deriveTicketKeys(config.serverSecret, 5), deriveTicketKeys(config.serverSecret, 5));
        assert.notDeepEqual(deriveTicketKeys(config.serverSecret, 5), deriveTicketKeys(config.serverSecret, 6));
        assert.equal(deriveTicketKeys(crypto.randomBytes(32), 1).length, 48);
    });

    it('reloads the certificate without a restart and keeps it when the new one is broken', async () => {
        fs.copyFileSync(b.cert, config.tlsCertFile);
        fs.copyFileSync(b.key, config.tlsKeyFile);
        assert.equal(lst.reloadCertificates(), true);
        const s = await tlsConnect(apiPort);
        assert.match(s.getPeerCertificate().subject.CN, /second\.test/);
        s.destroy();
        fs.copyFileSync(a.key, config.tlsKeyFile);               // key no longer matches the certificate
        assert.equal(lst.reloadCertificates(), false);
        const s2 = await tlsConnect(apiPort);
        assert.match(s2.getPeerCertificate().subject.CN, /second\.test/);
        s2.destroy();
    });
});

// ---- Protection per address on the listeners (net/ipguard.js) and the slow-client timeouts ----

function fakeClock(start = 1e6) {
    let t = start;
    const now = () => t;
    now.advance = (ms) => { t += ms; };
    return now;
}

// One HTTP request on a raw socket; resolves with { status, headers, body, socket } once the answer is read.
function rawRequest(port, text, { socket = null } = {}) {
    return new Promise((resolve, reject) => {
        const s = socket || net.connect(port, '127.0.0.1');
        let buf = '';
        const onData = (d) => {
            buf += d;
            const end = buf.indexOf('\r\n\r\n');
            if (end < 0) return;
            const lines = buf.slice(0, end).split('\r\n');
            const headers = {};
            for (const l of lines.slice(1)) { const c = l.indexOf(':'); headers[l.slice(0, c).toLowerCase()] = l.slice(c + 1).trim(); }
            const len = +(headers['content-length'] || 0);
            if (buf.length < end + 4 + len) return;
            s.removeListener('data', onData);
            resolve({ status: +lines[0].split(' ')[1], headers, body: buf.slice(end + 4, end + 4 + len), socket: s });
        };
        s.setEncoding('utf8');
        s.on('data', onData);
        s.once('error', reject);
        s.write(text);
    });
}

function closedWithin(socket, ms) {
    return new Promise((resolve) => {
        if (socket.destroyed) { resolve(true); return; }
        const t = setTimeout(() => resolve(false), ms);
        socket.once('close', () => { clearTimeout(t); resolve(true); });
    });
}

const waitUntil = (pred) => new Promise((resolve) => { const t = setInterval(() => { if (pred()) { clearInterval(t); resolve(); } }, 5); });

describe('listeners: protection per address and slow clients', () => {
    async function setup(env = {}, o = {}) {
        const port = await freePort();
        const config = { ...testConfig({ HTTP_RATE_PER_IP: '20', ...env }), bindAddress: '127.0.0.1', apiPort: port, wsPort: port, ...(o.config || {}) };
        const registry = new Registry();
        const now = fakeClock();
        const guard = new IpGuard({ config, workers: 1, registry, now, report: () => {} });
        const wss = new WsServer({ registry, guard, clientIp: makeClientIp(config), onConnection: () => {} });
        const lst = new Listeners({
            config, wsServer: wss, apiHandler: o.apiHandler || ((req, res) => { res.writeHead(404); res.end('{}'); }), ready: () => true,
            registry, guard, ...(o.listeners || {}),
        });
        await lst.listen();
        return { port, guard, registry, now, lst, wss, close: () => { wss.closeAll(); lst.close(); guard.close(); } };
    }

    it('every request takes a token before routing: health, 404, HEAD, OPTIONS, a page, a 414 target, an upgrade', async () => {
        const { port, guard, close } = await setup();              // 20 per minute: a burst of 10
        try {
            const tokens = () => guard.reqs.buckets.peek('127.0.0.1').tokens;
            const request = (method, p) => new Promise((resolve) => {
                http.request({ host: '127.0.0.1', port, method, path: p }, (r) => { r.resume(); r.on('end', () => resolve(r.statusCode)); }).end();
            });
            assert.equal((await get(port, '/healthz')).status, 200);
            assert.equal(tokens(), 9);
            assert.equal((await get(port, '/api/v1/nothing')).status, 404);
            await request('HEAD', '/api/v1/info');
            await request('OPTIONS', '/api/v1/info');
            await get(port, '/verify-email?token=x');
            await get(port, '/' + 'a'.repeat(5000));
            (await connectWs({ port })).destroy();
            assert.equal(tokens(), 3);
            for (let i = 0; i < 3; i++) assert.notEqual((await get(port, '/healthz')).status, 429);
            const r = await get(port, '/healthz');
            assert.equal(r.status, 429);
            assert.deepEqual(JSON.parse(r.body), { error: 'rate_limited', message: 'Too many requests; try again later.', retryAfter: 3 });
            assert.equal(r.headers['retry-after'], '3');
            assert.equal(r.headers['cache-control'], 'no-store');
            assert.equal(r.headers['x-content-type-options'], 'nosniff');
            const ws = await connectWs({ port }).catch((e) => e);
            assert.equal(ws.status, 429, 'the upgrade too');
            assert.equal(ws.headers['retry-after'], '3');
            assert.equal(JSON.parse(ws.body).error, 'rate_limited');
        } finally { close(); }
    });

    it('a blocked address on an open keep-alive connection gets 429 with Connection: close, and the socket closes', async () => {
        const { port, guard, close } = await setup({ HTTP_RATE_PER_IP: '600' });
        try {
            const first = await rawRequest(port, 'GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n');
            assert.equal(first.status, 200);
            assert.notEqual(first.headers.connection, 'close');
            guard.applyBlocks([['127.0.0.1', 120000, 2]]);
            const r = await rawRequest(port, 'GET /api/v1/info HTTP/1.1\r\nHost: x\r\n\r\n', { socket: first.socket });
            assert.equal(r.status, 429);
            assert.equal(r.headers.connection, 'close');
            assert.equal(r.headers['retry-after'], '120');
            assert.equal(JSON.parse(r.body).retryAfter, 120);
            assert.ok(await closedWithin(first.socket, 2000), 'the server ended the connection');
        } finally { close(); }
    });

    it('caps the requests in progress of one address (IP_MAX_INFLIGHT)', async () => {
        const waiting = [];
        const { port, close } = await setup({ IP_MAX_INFLIGHT: '2', HTTP_RATE_PER_IP: '600' }, {
            apiHandler: (req, res) => { waiting.push(res); },
        });
        try {
            const pending = [get(port, '/api/v1/slow'), get(port, '/api/v1/slow')];
            await waitUntil(() => waiting.length === 2);
            const r = await get(port, '/api/v1/slow');
            assert.equal(r.status, 429);
            assert.equal(r.headers['retry-after'], '1');
            for (const res of waiting.splice(0)) res.end('{}');
            await Promise.all(pending);
            const later = get(port, '/api/v1/slow');
            await waitUntil(() => waiting.length === 1);
            waiting[0].end('{}');
            assert.equal((await later).status, 200, 'the slots came back');
        } finally { close(); }
    });

    it('slowloris: a header line every 300 ms is cut within headersTimeout + 1 s, and counted', async () => {
        const { port, guard, registry, close } = await setup({}, { listeners: { headersTimeoutMs: 1000 } });
        try {
            const s = net.connect(port, '127.0.0.1');
            s.on('error', () => {});
            s.write('GET /healthz HTTP/1.1\r\nHost: x\r\n');
            const t0 = Date.now();
            const timer = setInterval(() => { if (!s.destroyed) s.write(`X-Slow-${Date.now()}: 1\r\n`); }, 300);
            let answer = '';
            s.on('data', (d) => { answer += d; });
            const closed = await closedWithin(s, 4000);
            clearInterval(timer);
            const ms = Date.now() - t0;
            assert.ok(closed && ms >= 900 && ms < 2600, `closed after ${ms} ms`);
            assert.match(answer, /^HTTP\/1\.1 408/);
            const m = registry.metrics.get('scacelith_http_client_errors_total');
            assert.equal([...m.children.values()].find((c) => c.labelValues[0] === 'timeout').value, 1);
            assert.deepEqual(guard.flushReports(), [['127.0.0.1', null, 1]], 'counted toward a block of the address');
        } finally { close(); }
    });

    it('a client that stops reading a large answer is cut by the inactivity timeout; a slow reader by the send deadline', async () => {
        const big = Buffer.alloc(48 << 20, 120);
        const { port, lst, close } = await setup({ HTTP_RATE_PER_IP: '600' }, {
            apiHandler: (req, res) => { res.writeHead(200, { 'Content-Length': big.length }); res.end(big); },
            listeners: { idleTimeoutMs: 500, sendTimeoutMs: 1500 },
        });
        const server = lst.servers[0].server;
        const connections = () => new Promise((resolve) => server.getConnections((e, n) => resolve(e ? -1 : n)));
        try {
            // Never reads: nothing moves on the socket, the inactivity timeout ends it (a paused
            // client does not notice the reset, so the server's side is watched).
            const idle = net.connect(port, '127.0.0.1');
            idle.on('error', () => {});
            idle.pause();
            idle.write('GET /big HTTP/1.1\r\nHost: x\r\n\r\n');
            let t0 = Date.now();
            while ((await connections()) !== 1) await new Promise((r) => setTimeout(r, 5));
            for (;;) {
                await new Promise((r) => setTimeout(r, 50));
                if ((await connections()) === 0) break;
                assert.ok(Date.now() - t0 < 3000, 'idle reader closed by the server');
            }
            const idleMs = Date.now() - t0;
            assert.ok(idleMs >= 400 && idleMs < 2500, `idle reader closed after ${idleMs} ms`);
            idle.destroy();
            // Reads a chunk every 50 ms: bytes keep moving, so only the send deadline ends it.
            const slow = net.connect(port, '127.0.0.1');
            slow.on('error', () => {});
            let got = 0;
            slow.on('data', (d) => { got += d.length; slow.pause(); setTimeout(() => slow.resume(), 50); });
            slow.write('GET /big HTTP/1.1\r\nHost: x\r\n\r\n');
            t0 = Date.now();
            assert.ok(await closedWithin(slow, 6000), 'slow reader closed');
            const ms = Date.now() - t0;
            assert.ok(ms >= 1400, `not before the deadline (${ms} ms)`);
            assert.ok(got > 0 && got < big.length, `cut before the end of the answer (${got} bytes read)`);
        } finally { close(); }
    });

    it('a request still in its handler keeps its socket past the inactivity timeout (a GIF render, an export)', async () => {
        const { port, close } = await setup({ HTTP_RATE_PER_IP: '600' }, {
            apiHandler: (req, res) => { setTimeout(() => { res.writeHead(200, { 'Content-Length': 2 }); res.end('{}'); }, 900); },
            listeners: { idleTimeoutMs: 300 },
        });
        try {
            const t0 = Date.now();
            const r = await get(port, '/api/v1/slow-render');
            const ms = Date.now() - t0;
            assert.equal(r.status, 200);
            assert.equal(r.body, '{}');
            assert.ok(ms >= 850, `answered by the handler, three inactivity periods later (${ms} ms)`);
        } finally { close(); }
    });

    it('malformed HTTP is answered 400 and counted toward a block of the client, not of a trusted proxy', async () => {
        const plain = await setup();
        try {
            const r = await rawRequest(plain.port, 'BLAH\r\n\r\n');
            assert.equal(r.status, 400);
            assert.deepEqual(plain.guard.flushReports(), [['127.0.0.1', null, 1]]);
        } finally { plain.close(); }
        const proxied = await setup({}, { config: { tlsMode: 'proxy', trustedProxies: ['127.0.0.1'] } });
        try {
            const r = await rawRequest(proxied.port, 'BLAH\r\n\r\n');
            assert.equal(r.status, 400);
            assert.deepEqual(proxied.guard.flushReports(), [], 'the proxy is not the client');
        } finally { proxied.close(); }
    });

    it('proxy mode: keyed by X-Forwarded-For, and a block never closes the proxy connection', async () => {
        const { port, guard, close } = await setup({ HTTP_RATE_PER_IP: '600' }, { config: { tlsMode: 'proxy', trustedProxies: ['127.0.0.1'] } });
        try {
            guard.applyBlocks([['198.51.100.7', 60000, 1]]);
            const r = await rawRequest(port, 'GET /api/v1/x HTTP/1.1\r\nHost: x\r\nX-Forwarded-For: 198.51.100.7\r\n\r\n');
            assert.equal(r.status, 429);
            assert.notEqual(r.headers.connection, 'close');
            const other = await rawRequest(port, 'GET /healthz HTTP/1.1\r\nHost: x\r\nX-Forwarded-For: 198.51.100.8\r\n\r\n', { socket: r.socket });
            assert.equal(other.status, 200, 'the same proxy connection serves the next client');
            assert.equal(guard.reqs.buckets.peek('198.51.100.8').tokens, 299, 'counted for the forwarded client');
            r.socket.destroy();
        } finally { close(); }
    });
});
