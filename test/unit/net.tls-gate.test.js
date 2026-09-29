// Admission before TLS (src/net/listeners.js TlsGate) and the server-full signal it uses
// (src/cluster/router.js Router.isFull): the handshake cap, the per-address cap, the shedding
// while the server is full, and the slots given back by completed and failed handshakes.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { EventEmitter } from 'node:events';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import tls from 'node:tls';
import { after, before, describe, it } from 'node:test';
import { FULL_HOLD_MS, Router } from '../../src/cluster/router.js';
import { testConfig } from '../../src/config.js';
import { Registry } from '../../src/metrics.js';
import { Listeners, TlsGate } from '../../src/net/listeners.js';
import { WsServer } from '../../src/net/ws.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(pred, ms = 3000) {
    const t0 = Date.now();
    while (!pred()) {
        if (Date.now() - t0 > ms) throw new Error('condition not reached');
        await sleep(5);
    }
}

function refused(registry, reason) {
    const m = registry.metrics.get('scacelith_tls_refused_total');
    for (const c of m?.children.values() ?? []) if (c.labelValues[0] === reason) return c.value;
    return 0;
}

// A raw socket as the 'connection' event gives it (only what the gate uses).
class FakeSocket extends EventEmitter {
    constructor(ip = '192.0.2.1') { super(); this.remoteAddress = ip; this.reset = false; }
    resetAndDestroy() { this.reset = true; this.emit('close'); return this; }
    destroy() { this.emit('close'); return this; }
}

describe('TlsGate (simulated sockets)', () => {
    it('admits up to the cap and refuses the next socket; released slots are reusable', () => {
        const registry = new Registry();
        const gate = new TlsGate({ maxPending: 3, registry });
        const s = [1, 2, 3, 4].map((i) => new FakeSocket(`192.0.2.${i}`));
        assert.ok(gate.admit(s[0]) && gate.admit(s[1]) && gate.admit(s[2]));
        assert.equal(gate.pending, 3);
        assert.equal(gate.admit(s[3]), false);
        assert.equal(s[3].reset, true, 'closed with an RST');
        assert.equal(refused(registry, 'handshakes'), 1);
        gate.release(s[0]);                                  // handshake completed
        gate.release(s[0]);                                  // idempotent
        s[0].emit('close');                                  // and its later close is not counted again
        assert.equal(gate.pending, 2);
        s[1].emit('close');                                  // closed during the handshake
        assert.equal(gate.pending, 1);
        const s5 = new FakeSocket();
        assert.equal(gate.admit(s5), true);
        gate.release(new FakeSocket());                      // never admitted: ignored
        gate.release(undefined);
        assert.equal(gate.pending, 2);
        s[2].emit('close'); s5.emit('close');
        assert.equal(gate.pending, 0);
        assert.equal(gate.perIp.size, 0);
    });

    it('limits the handshakes of one address group (IPv4 address, IPv6 /64)', () => {
        const registry = new Registry();
        const gate = new TlsGate({ maxPending: 100, maxPendingPerIp: 2, registry });
        const a = new FakeSocket('2001:db8:1:2::1'), b = new FakeSocket('2001:db8:1:2:ffff::9');
        assert.ok(gate.admit(a) && gate.admit(b));
        assert.equal(gate.admit(new FakeSocket('2001:db8:1:2:aaaa::5')), false);
        assert.equal(gate.admit(new FakeSocket('2001:db8:1:3::1')), true, 'another /64');
        const v4 = new FakeSocket('::ffff:198.51.100.7');
        assert.ok(gate.admit(v4) && gate.admit(new FakeSocket('198.51.100.7')));
        assert.equal(gate.admit(new FakeSocket('198.51.100.7')), false, 'IPv4-mapped is the same address');
        assert.equal(refused(registry, 'per_ip'), 2);
        a.emit('close');
        assert.equal(gate.admit(new FakeSocket('2001:db8:1:2::77')), true);
    });

    it('while the server is full, only the upgrade listener sheds, at a limited rate', () => {
        const registry = new Registry();
        let full = true, t = 10000;
        const gate = new TlsGate({ maxPending: 100, full: () => full, fullRatePerSec: 2, now: () => t, registry });
        assert.equal(gate.admit(new FakeSocket(), true), true);
        assert.equal(gate.admit(new FakeSocket(), true), true);
        assert.equal(gate.admit(new FakeSocket(), true), false);
        assert.equal(refused(registry, 'server_full'), 1);
        assert.equal(gate.admit(new FakeSocket(), false), true, 'the API-only listener is never shed');
        t += 500;                                           // one more token
        assert.equal(gate.admit(new FakeSocket(), true), true);
        assert.equal(gate.admit(new FakeSocket(), true), false);
        full = false;
        for (let i = 0; i < 10; i++) assert.equal(gate.admit(new FakeSocket(`203.0.113.${i}`), true), true);
        assert.equal(refused(registry, 'server_full'), 2);
        assert.equal(new TlsGate({ maxPending: 128, registry: new Registry() }).fullRate, 64, 'default: half the cap per second');
    });

    it('refuses to wrap a server that has no connection listener', () => {
        assert.throws(() => new TlsGate({ registry: new Registry() }).attach(new EventEmitter()), /no connection listener/);
    });

    it('plain listeners (proxy, off) have no gate, and listen with the default backlog', () => {
        const wss = new WsServer({ registry: new Registry(), onConnection: () => {} });
        const lst = new Listeners({ config: { ...testConfig(), bindAddress: '127.0.0.1', apiPort: 0, wsPort: 0 }, wsServer: wss, apiHandler: null, registry: new Registry() });
        assert.equal(lst.gate, null);
        assert.equal(lst.backlog, 2048);
    });
});

let hasOpenssl = true;
try { execFileSync('openssl', ['version'], { stdio: 'ignore' }); } catch { hasOpenssl = false; }

function makeCert(dir) {
    const key = path.join(dir, 'gate.key'), cert = path.join(dir, 'gate.crt');
    execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes', '-keyout', key, '-out', cert,
        '-days', '1', '-subj', '/CN=gate.test'], { stdio: 'ignore' });
    return { key, cert };
}

function tlsConnect(port) {
    return new Promise((resolve, reject) => {
        const s = tls.connect({ host: '127.0.0.1', port, rejectUnauthorized: false, ALPNProtocols: ['http/1.1'] }, () => resolve(s));
        s.on('error', reject);
    });
}

function rawConnect(port) {
    return new Promise((resolve, reject) => {
        const s = net.connect(port, '127.0.0.1', () => resolve(s));
        s.on('error', reject);
    });
}

describe('TlsGate on a real TLS server', { skip: !hasOpenssl && 'openssl not available' }, () => {
    let dir, cert;
    before(() => {
        dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-gate-'));
        cert = makeCert(dir);
    });
    after(() => fs.rmSync(dir, { recursive: true, force: true }));

    it('closes sockets over the cap before any TLS work; completed and failed handshakes give their slot back', async () => {
        const registry = new Registry();
        const gate = new TlsGate({ maxPending: 2, registry });
        const accepted = [];
        const server = tls.createServer({ key: fs.readFileSync(cert.key), cert: fs.readFileSync(cert.cert), handshakeTimeout: 5000 }, (s) => accepted.push(s));
        server.on('tlsClientError', () => {});
        // Count the sockets handed to Node's TLS listener (the TLS work) behind the gate.
        let tlsWork = 0;
        const [tlsListener] = server.listeners('connection');
        server.removeAllListeners('connection');
        server.on('connection', function countTlsWork(s) { tlsWork++; return tlsListener.call(this, s); });
        gate.attach(server);
        await new Promise((r) => server.listen(0, '127.0.0.1', r));
        const { port } = server.address();
        const open = [];
        try {
            // Two clients that never send their ClientHello hold both slots.
            const idle1 = await rawConnect(port), idle2 = await rawConnect(port);
            open.push(idle1, idle2);
            await waitFor(() => gate.pending === 2);
            await assert.rejects(tlsConnect(port));
            assert.equal(tlsWork, 2, 'the refused socket never reached TLS');
            assert.equal(refused(registry, 'handshakes'), 1);
            // A slot frees when a pending socket closes.
            idle1.destroy();
            await waitFor(() => gate.pending === 1);
            // Completed handshakes give their slot back: more TLS connections than slots stay open.
            for (let i = 0; i < 4; i++) {
                open.push(await tlsConnect(port));
                await waitFor(() => accepted.length === i + 1 && gate.pending === 1);
            }
            assert.equal(accepted.length, 4);
            // A failed handshake (plain HTTP on the TLS port) gives its slot back too.
            const bad = await rawConnect(port);
            open.push(bad);
            bad.on('error', () => {});
            await waitFor(() => gate.pending === 2);
            bad.write('GET / HTTP/1.1\r\nHost: x\r\n\r\n');
            await waitFor(() => gate.pending === 1);
            idle2.destroy();
            await waitFor(() => gate.pending === 0);
            assert.equal(gate.perIp.size, 0);
        } finally {
            for (const s of open) s.destroy();
            for (const s of accepted) s.destroy();
            await new Promise((r) => server.close(r));
        }
    });

    it('Listeners: the gate guards both TLS listeners, sheds only the upgrade listener, and listens with LISTEN_BACKLOG', async () => {
        const registry = new Registry();
        const freePort = () => new Promise((resolve) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); }); });
        const apiPort = await freePort(), wsPort = await freePort();
        const config = {
            ...testConfig({ MAX_PENDING_HANDSHAKES: '2', LISTEN_BACKLOG: '4096' }), tlsMode: 'native', bindAddress: '127.0.0.1', apiPort, wsPort,
            tlsMinVersion: 'TLSv1.2', tlsCertFile: cert.cert, tlsKeyFile: cert.key,
        };
        const wss = new WsServer({ registry, onConnection: () => {} });
        const lst = new Listeners({ config, wsServer: wss, apiHandler: null, full: () => true, registry });
        const backlogs = [];
        for (const { server } of lst.servers) {
            const listen = server.listen;
            server.listen = function (opts, cb) { backlogs.push(opts.backlog); return listen.call(this, opts, cb); };
        }
        await lst.listen();
        const open = [];
        try {
            assert.deepEqual(backlogs, [4096, 4096]);
            assert.equal(lst.gate.maxPending, 2);
            assert.equal(lst.gate.fullRate, 1);
            // API listener (WS_PORT != API_PORT): never shed while full.
            for (let i = 0; i < 3; i++) open.push(await tlsConnect(apiPort));
            // Upgrade listener: one connection per second while full, the next one is closed before TLS.
            open.push(await tlsConnect(wsPort));
            await assert.rejects(tlsConnect(wsPort));
            assert.equal(refused(registry, 'server_full'), 1);
        } finally {
            for (const s of open) s.destroy();
            wss.closeAll();
            lst.close();
        }
    });

});

describe('Router.isFull (server-full signal)', () => {
    function mkRouter(env = {}) {
        let reply = { ok: true };
        const primary = { request: async () => reply, notify() {}, on() {} };
        const router = new Router({
            config: testConfig({ MAX_CONNECTIONS: '10', WORKERS: '2', ...env }), shard: 0, host: {}, auth: {}, primary, registry: new Registry(),
        });
        return { router, answer: (r) => { reply = r; } };
    }

    it('follows the exact check of the primary, for FULL_HOLD_MS at most', async () => {
        const { router, answer } = mkRouter();
        assert.equal(router.isFull(), false);
        answer({ ok: false, reason: 'per_ip' });
        await router.admission.acquire('192.0.2.1');
        assert.equal(router.isFull(), false, 'a per-address refusal says nothing about the server');
        answer({ ok: false, reason: 'global' });
        const r = await router.admission.acquire('192.0.2.1');
        assert.deepEqual([r.ok, r.status], [false, 503]);
        const now = Date.now();
        assert.equal(router.isFull(now), true);
        assert.equal(router.isFull(now + FULL_HOLD_MS + 1), false, 'the signal expires');
        answer({ ok: true });
        assert.equal(await router.admission.acquire('192.0.2.1'), true);
        assert.equal(router.isFull(), false, 'an admitted upgrade ends it');
    });

    it('is also true while this worker holds 1.2 times its share of MAX_CONNECTIONS', () => {
        const { router } = mkRouter();
        assert.equal(router.localCap, 6);                   // ceil(10 * 1.2 / 2)
        for (let i = 1; i <= 5; i++) router.conns.set(i, {});
        assert.equal(router.isFull(), false);
        router.conns.set(6, {});
        assert.equal(router.isFull(), true);
    });
});
