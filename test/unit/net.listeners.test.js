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
