// Admission before TLS (src/net/listeners.js TlsGate) and the server-full signal it uses
// (src/cluster/router.js Router.isFull): the wait for the ClientHello without a slot, the
// handshake cap, the per-group caps, the shedding while the server is full, the slots given back
// by completed and failed handshakes, and the sockets closed after a handshake timeout.

import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { EventEmitter } from 'node:events';
import fs from 'node:fs';
import https from 'node:https';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { Duplex } from 'node:stream';
import tls from 'node:tls';
import { after, before, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';
import { FULL_HOLD_MS, Router } from '../../src/cluster/router.js';
import { describe as describeConfig, testConfig } from '../../src/config.js';
import { Registry } from '../../src/metrics.js';
import { Listeners, TlsGate, defaultPendingPerGroup } from '../../src/net/listeners.js';
import { connectWs } from '../../src/net/ws-raw-client.js';
import { WsServer } from '../../src/net/ws.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(pred, ms = 3000) {
    const t0 = Date.now();
    while (!(await pred())) {
        if (Date.now() - t0 > ms) throw new Error('condition not reached');
        await sleep(5);
    }
}
const connections = (server) => new Promise((resolve) => server.getConnections((e, n) => resolve(e ? -1 : n)));

function refused(registry, reason) {
    const m = registry.metrics.get('scacelith_tls_refused_total');
    for (const c of m?.children.values() ?? []) if (c.labelValues[0] === reason) return c.value;
    return 0;
}

// A raw socket as the 'connection' event gives it (only what the gate uses).
class FakeSocket extends EventEmitter {
    constructor(ip = '192.0.2.1') { super(); this.remoteAddress = ip; this.reset = false; this.destroyed = false; this.paused = false; this.unshifted = null; }
    resetAndDestroy() { this.reset = true; this.destroyed = true; this.emit('close'); return this; }
    destroy() { this.destroyed = true; this.emit('close'); return this; }
    pause() { this.paused = true; return this; }
    unshift(b) { this.unshifted = this.unshifted ? Buffer.concat([b, this.unshifted]) : b; }
}

// A TLS record that holds a handshake message: a ClientHello of `size` bytes by default. Only
// the headers are meaningful, which is all the gate reads.
function helloRecord(size = 300, { type = 22, hsType = 1, hsLen = size, recLen = size + 4 } = {}) {
    const b = Buffer.alloc(Math.max(9, 5 + recLen), 0xab);
    b[0] = type; b[1] = 3; b[2] = 1; b.writeUInt16BE(recLen, 3);
    b[5] = hsType; b.writeUIntBE(hsLen, 6, 3);
    return b.subarray(0, 5 + recLen);
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

    it('limits the handshakes of one address group (IPv4 address, IPv6 /48)', () => {
        const registry = new Registry();
        const gate = new TlsGate({ maxPending: 100, maxPendingPerIp: 2, registry });
        const a = new FakeSocket('2001:db8:1:2::1'), b = new FakeSocket('2001:db8:1:3:ffff::9');
        assert.ok(gate.admit(a) && gate.admit(b));
        assert.equal(gate.admit(new FakeSocket('2001:db8:1:ffff:aaaa::5')), false, 'another /64 of the same /48');
        assert.equal(gate.admit(new FakeSocket('2001:db8:2::1')), true, 'another /48');
        const v4 = new FakeSocket('::ffff:198.51.100.7');
        assert.ok(gate.admit(v4) && gate.admit(new FakeSocket('198.51.100.7')));
        assert.equal(gate.admit(new FakeSocket('198.51.100.7')), false, 'IPv4-mapped is the same address');
        assert.equal(refused(registry, 'per_ip'), 2);
        a.emit('close');
        assert.equal(gate.admit(new FakeSocket('2001:db8:1:77::77')), true);
    });

    it('has its own small per-group cap, independent of MAX_CONNECTIONS_PER_IP', () => {
        const gate = new TlsGate({ maxPending: 128, registry: new Registry() });
        assert.deepEqual([gate.maxPendingPerIp, gate.maxWaiting, gate.maxWaitingPerIp], [4, 2048, 16]);
        assert.deepEqual([2, 3, 32, 64, 100, 1000, 100000].map(defaultPendingPerGroup), [1, 2, 2, 2, 3, 31, 3125]);
        for (const n of [2, 3, 5, 64, 128, 4096]) assert.ok(defaultPendingPerGroup(n) < n, `below the total (${n})`);
        // With the default caps, 32 address groups are needed to hold every slot.
        const g = new TlsGate({ maxPending: 128, registry: new Registry() });
        let admitted = 0;
        for (let i = 0; i < 8; i++) for (let j = 0; j < 16; j++) if (g.admit(new FakeSocket(`127.0.1.${i + 1}`))) admitted++;
        assert.equal(admitted, 32);
    });

    it('while the server is full, only the upgrade listener sheds, at a limited rate', () => {
        const registry = new Registry();
        let full = true, t = 10000;
        const gate = new TlsGate({ maxPending: 100, maxPendingPerIp: 50, full: () => full, fullRatePerSec: 2, now: () => t, registry });
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

    it('uses a monotonic clock, and a clock that goes back leaves no token debt', () => {
        const g = new TlsGate({ registry: new Registry() });
        assert.ok(Math.abs(g.now() - performance.now()) < 1000, 'performance.now by default, not the wall clock');
        let t = 1e6;
        const gate = new TlsGate({ maxPending: 100, maxPendingPerIp: 50, full: () => true, fullRatePerSec: 10, now: () => t, registry: new Registry() });
        for (let i = 0; i < 10; i++) assert.equal(gate.admit(new FakeSocket(), true), true);
        assert.equal(gate.admit(new FakeSocket(), true), false);
        t -= 60000;                                         // the clock steps back one minute
        assert.equal(gate.admit(new FakeSocket(), true), false);
        t += 100;                                           // 100 ms later: one token, not minus 599
        assert.equal(gate.admit(new FakeSocket(), true), true);
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

describe('TlsGate: waiting for the ClientHello (simulated sockets)', () => {
    function setup(opts = {}) {
        const registry = new Registry();
        const gate = new TlsGate({ maxPending: 8, maxPendingPerIp: 4, registry, ...opts });
        const ready = [];
        return { registry, gate, ready, onReady: (s) => ready.push(s) };
    }

    it('holds no slot until the whole first record has arrived, then hands it over unread', () => {
        const { gate, ready, onReady } = setup();
        const rec = helloRecord(700);
        const extra = Buffer.from([0x14, 0x03, 0x03, 0x00, 0x01, 0x01]);   // a following record (ChangeCipherSpec)
        const s = new FakeSocket();
        assert.equal(gate.accept(s, false, onReady), true);
        const parts = [rec.subarray(0, 3), rec.subarray(3, 7), rec.subarray(7, 300), rec.subarray(300, 704), Buffer.concat([rec.subarray(704), extra])];
        for (const p of parts.slice(0, -1)) {
            s.emit('data', p);
            assert.deepEqual([gate.waiting, gate.pending, ready.length], [1, 0, 0], 'still waiting, no slot');
        }
        s.emit('data', parts.at(-1));
        assert.deepEqual([gate.waiting, gate.pending, ready.length], [0, 1, 1]);
        assert.equal(ready[0], s);
        assert.equal(s.paused, true);
        assert.deepEqual(s.unshifted, Buffer.concat([rec, extra]), 'every byte read is put back, in order');
        assert.equal(s.listenerCount('data') + s.listenerCount('end'), 0, 'the gate no longer reads the socket');
        assert.equal(gate.waitingPerIp.size, 0);
        s.emit('close');
        assert.equal(gate.pending, 0);
    });

    it('accepts a ClientHello sent byte by byte, and a large one that needs more room', () => {
        const { gate, ready, onReady } = setup();
        for (const size of [300, 9000, 16380]) {
            const rec = helloRecord(size);
            const s = new FakeSocket(`192.0.2.${size % 250}`);
            gate.accept(s, false, onReady);
            for (let i = 0; i < rec.length; i++) s.emit('data', rec.subarray(i, i + 1));
            assert.equal(ready.at(-1), s);
            assert.deepEqual(s.unshifted, rec);
            s.emit('close');
        }
        assert.deepEqual([ready.length, gate.waiting, gate.pending], [3, 0, 0]);
    });

    it('closes a socket whose first record does not start a ClientHello, before any slot', () => {
        const { registry, gate, ready, onReady } = setup();
        const bad = [
            Buffer.from('GET / HTTP/1.1\r\nHost: x\r\n\r\n'),                      // plain HTTP on the TLS port
            helloRecord(300, { type: 23 }),                                         // application data
            helloRecord(16381),                                                     // record body over 2^14 bytes
            helloRecord(0, { recLen: 0 }),                                          // empty handshake record
            helloRecord(300, { hsType: 2 }),                                        // not a ClientHello
            Buffer.from([0x80, 0x2e, 0x01, 0x03, 0x01]),                            // SSLv2-style ClientHello
        ];
        for (const b of bad) {
            const s = new FakeSocket();
            gate.accept(s, false, onReady);
            s.emit('data', b);
            assert.equal(s.reset, true);
        }
        assert.deepEqual([refused(registry, 'bad_hello'), ready.length, gate.waiting, gate.pending], [bad.length, 0, 0, 0]);
    });

    it('takes the slot at the first record of a ClientHello fragmented over several records', () => {
        const { gate, ready, onReady } = setup();
        // A 1000-byte ClientHello whose first record carries 50 bytes of it, then 1 byte (legal,
        // RFC 8446 section 5.1), then the rest.
        const msg = helloRecord(1000).subarray(5);
        const rec = (part) => Buffer.concat([Buffer.from([22, 3, 1, part.length >> 8, part.length & 0xff]), part]);
        const records = [rec(msg.subarray(0, 50)), rec(msg.subarray(50, 51)), rec(msg.subarray(51))];
        const s = new FakeSocket();
        gate.accept(s, false, onReady);
        s.emit('data', records[0].subarray(0, 54));
        assert.deepEqual([gate.waiting, gate.pending, ready.length], [1, 0, 0]);
        s.emit('data', Buffer.concat([records[0].subarray(54), records[1].subarray(0, 2)]));
        assert.deepEqual([gate.waiting, gate.pending, ready.length], [0, 1, 1]);
        assert.deepEqual(s.unshifted, Buffer.concat([records[0], records[1].subarray(0, 2)]), 'every byte read is put back, in order');
        // A first record of one byte starts a ClientHello too.
        const t = new FakeSocket('192.0.2.2');
        gate.accept(t, false, onReady);
        t.emit('data', records[1].subarray(0, 5));
        t.emit('data', Buffer.from([1]));
        assert.deepEqual([gate.pending, ready.length], [2, 2]);
    });

    it('bounds the waiting sockets in total and per address group (IPv6 /48)', () => {
        const { registry, gate, onReady } = setup({ maxWaiting: 3, maxWaitingPerIp: 2 });
        const a = new FakeSocket('2001:db8:5:1::1'), b = new FakeSocket('2001:db8:5:2::1');
        assert.ok(gate.accept(a, false, onReady) && gate.accept(b, false, onReady));
        const c = new FakeSocket('2001:db8:5:3::1');
        assert.equal(gate.accept(c, false, onReady), false, 'a third one from the same /48');
        assert.equal(c.reset, true);
        assert.equal(gate.accept(new FakeSocket('192.0.2.9'), false, onReady), true);
        assert.equal(gate.accept(new FakeSocket('192.0.2.10'), false, onReady), false, 'total reached');
        assert.deepEqual([refused(registry, 'waiting_per_ip'), refused(registry, 'waiting'), gate.waiting], [1, 1, 3]);
        a.emit('close');                                    // the client left
        assert.equal(gate.accept(new FakeSocket('2001:db8:5:4::1'), false, onReady), true);
        assert.equal(refused(registry, 'waiting_per_ip') + refused(registry, 'waiting'), 2, 'a client leaving is not counted');
    });

    it('closes a socket that has not sent its ClientHello in time, and one that ends before it', async () => {
        const { registry, gate, ready, onReady } = setup({ helloTimeoutMs: 40 });
        const slow = new FakeSocket(), ended = new FakeSocket('192.0.2.2');
        gate.accept(slow, false, onReady);
        gate.accept(ended, false, onReady);
        slow.emit('data', helloRecord(300).subarray(0, 100));
        ended.emit('data', helloRecord(300).subarray(0, 10));
        ended.emit('end');
        assert.deepEqual([ended.destroyed, ended.reset, gate.waiting], [true, false, 1]);
        await sleep(80);
        assert.equal(slow.reset, true, 'closed at the deadline, whatever it sent before');
        assert.deepEqual([refused(registry, 'hello_timeout'), refused(registry, 'bad_hello'), ready.length, gate.waiting], [1, 0, 0, 0]);
    });

    it('a complete ClientHello still needs a free slot, and sheds while the server is full', () => {
        let full = false;
        const { registry, gate, ready, onReady } = setup({ maxPending: 1, full: () => full, fullRatePerSec: 1 });
        const s1 = new FakeSocket('192.0.2.1'), s2 = new FakeSocket('192.0.2.2');
        gate.accept(s1, false, onReady); gate.accept(s2, false, onReady);
        s1.emit('data', helloRecord());
        s2.emit('data', helloRecord());
        assert.deepEqual([ready.length, s2.reset, refused(registry, 'handshakes'), gate.pending, gate.waiting], [1, true, 1, 1, 0]);
        s1.emit('close');
        full = true;
        const s3 = new FakeSocket('192.0.2.3'), s4 = new FakeSocket('192.0.2.4');
        gate.accept(s3, true, onReady); gate.accept(s4, true, onReady);
        assert.equal(gate.waiting, 2, 'waiting is not shed: a silent socket costs no token');
        s3.emit('data', helloRecord());
        s3.emit('close');
        s4.emit('data', helloRecord());
        assert.deepEqual([ready.length, s4.reset, refused(registry, 'server_full')], [2, true, 1]);
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

// A plain TCP client; it reads (and drops) what it receives, so it sees the server close it.
function rawConnect(port) {
    return new Promise((resolve, reject) => {
        const s = net.connect(port, '127.0.0.1', () => resolve(s));
        s.on('error', reject);
        s.resume();
    });
}

function httpsGet(port, p) {
    return new Promise((resolve, reject) => {
        https.get({ host: '127.0.0.1', port, path: p, rejectUnauthorized: false, agent: false }, (res) => {
            res.resume();
            res.on('end', () => resolve(res.statusCode));
        }).on('error', reject);
    });
}

// The ClientHello of a real TLS client, captured without any server.
function captureClientHello() {
    return new Promise((resolve) => {
        const chunks = [];
        let c = null;
        const d = new Duplex({
            read() {},
            write(chunk, enc, cb) {
                if (!chunks.length) setImmediate(() => { c?.destroy(); resolve(Buffer.concat(chunks)); });
                chunks.push(Buffer.from(chunk));
                cb();
            },
        });
        c = tls.connect({ socket: d, rejectUnauthorized: false, servername: 'gate.test' });
        c.on('error', () => {});
    });
}

// A raw socket that sends a ClientHello and then nothing: the server does its part of the
// handshake and waits for the client's.
async function stalledHello(port, hello) {
    const s = await rawConnect(port);
    s.on('error', () => {});
    s.write(hello);
    return s;
}

// Splits the handshake record that holds a ClientHello into records of at most `size` bytes of
// body each (TLS lets a handshake message span several records).
function fragmentHello(record, size) {
    assert.equal(record.readUInt16BE(3) + 5, record.length, 'one record');
    const out = [];
    for (let off = 5; off < record.length; off += size) {
        const part = record.subarray(off, Math.min(record.length, off + size));
        const head = Buffer.from(record.subarray(0, 5));
        head.writeUInt16BE(part.length, 3);
        out.push(head, part);
    }
    return Buffer.concat(out);
}

// A real TLS client over a raw socket whose ClientHello is written after `firstDelayMs`, `piece`
// bytes at a time (`delayMs` apart, or one per event-loop turn), in records of at most
// `recordSize` bytes, and whose later writes wait `holdMs`.
function tlsOverRaw(port, { piece = Infinity, delayMs = 0, firstDelayMs = 0, holdMs = 0, recordSize = 0 } = {}) {
    return new Promise((resolve, reject) => {
        const raw = net.connect(port, '127.0.0.1');
        raw.setNoDelay(true);
        let first = true;
        const d = new Duplex({
            read() {},
            write(chunk, enc, cb) {
                if (!first) { setTimeout(() => { if (!raw.destroyed) raw.write(chunk); cb(); }, holdMs); return; }
                first = false;
                if (recordSize) chunk = fragmentHello(chunk, recordSize);
                let off = 0;
                const step = () => {
                    if (raw.destroyed) return cb();
                    raw.write(chunk.subarray(off, off + piece));
                    off += piece;
                    if (off >= chunk.length) return cb();
                    if (delayMs) setTimeout(step, delayMs); else setImmediate(step);
                };
                if (firstDelayMs) setTimeout(step, firstDelayMs); else step();
            },
            final(cb) { raw.end(); cb(); },
            destroy(err, cb) { raw.destroy(); cb(err); },
        });
        raw.on('data', (b) => d.push(b));
        raw.on('end', () => d.push(null));
        raw.on('error', (e) => d.destroy(e));
        const c = tls.connect({ socket: d, rejectUnauthorized: false, ALPNProtocols: ['http/1.1'] }, () => resolve(c));
        c.on('error', reject);
    });
}

// A client whose second flight (its Finished) comes `holdMs` late. The server must have closed the
// socket by then; the client alone may believe it is connected, since a TLS 1.3 client is done
// once it has sent its Finished. Resolves once the server has ended the connection.
async function lateClientClosed(port, holdMs) {
    const late = await tlsOverRaw(port, { holdMs }).catch((e) => e);
    if (late instanceof Error) return;
    let ended = late.destroyed || late.readableEnded;
    late.on('error', () => {});
    late.once('end', () => { ended = true; });
    late.once('close', () => { ended = true; });
    try { await waitFor(() => ended, 3000); } finally { late.destroy(); }
}

describe('TlsGate on a real TLS server', { skip: !hasOpenssl && 'openssl not available' }, () => {
    let dir, cert, hello;
    before(async () => {
        dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-gate-'));
        cert = makeCert(dir);
        hello = await captureClientHello();
    });
    after(() => fs.rmSync(dir, { recursive: true, force: true }));

    // A tls.Server behind a gate, echoing what its clients send; `tlsWork` counts the sockets
    // handed to Node's TLS listener.
    async function gatedServer(gateOpts, { handshakeTimeout = 5000 } = {}) {
        const registry = new Registry();
        const gate = new TlsGate({ registry, ...gateOpts });
        const accepted = [];
        const server = tls.createServer({ key: fs.readFileSync(cert.key), cert: fs.readFileSync(cert.cert), handshakeTimeout }, (s) => {
            accepted.push(s);
            s.on('data', (d) => s.write(d));
            s.on('error', () => {});
        });
        const raw = new Set();
        const env = {
            registry, gate, server, accepted, tlsWork: 0, port: 0,
            // Closes the server, and every socket it still has (a failing test must not hang).
            close: () => { for (const s of raw) s.destroy(); return new Promise((r) => server.close(r)); },
        };
        const [tlsListener] = server.listeners('connection');
        server.removeAllListeners('connection');
        server.on('connection', function countTlsWork(s) { env.tlsWork++; return tlsListener.call(this, s); });
        gate.attach(server);
        server.on('connection', (s) => { raw.add(s); s.once('close', () => raw.delete(s)); });
        await new Promise((r) => server.listen(0, '127.0.0.1', r));
        env.port = server.address().port;
        return env;
    }
    const echo = (c) => new Promise((resolve, reject) => { c.once('data', (d) => resolve(String(d))); c.once('error', reject); c.write('ping'); });

    it('silent connections hold no slot and are closed at the ClientHello deadline', async () => {
        const { registry, gate, server, accepted, port, close } = await gatedServer({ maxPending: 2, maxPendingPerIp: 2, maxWaitingPerIp: 50, helloTimeoutMs: 300 });
        const open = [];
        try {
            const idle = [];
            for (let i = 0; i < 10; i++) idle.push(await rawConnect(port));
            open.push(...idle);
            await waitFor(() => gate.waiting === 10);
            assert.equal(gate.pending, 0);
            // Before, two silent sockets held both slots and every other client was refused.
            const c = await tlsConnect(port);
            open.push(c);
            assert.equal(await echo(c), 'ping');
            // The server closes them: nothing stays open, and nothing can complete later.
            await waitFor(() => idle.every((s) => s.destroyed));
            assert.equal(refused(registry, 'hello_timeout'), 10);
            c.destroy();
            await waitFor(async () => gate.waiting === 0 && gate.pending === 0 && (await connections(server)) === 0);
            assert.equal(accepted.length, 1);
        } finally {
            for (const s of open) s.destroy();
            await close();
        }
    });

    it('completes handshakes whose ClientHello arrives in pieces, byte by byte or in several records, within the deadline', async () => {
        const { gate, port, close } = await gatedServer({ maxPending: 4 });
        let reads = 0;
        const waitData = gate._waitData.bind(gate);
        gate._waitData = (s, chunk) => { reads++; return waitData(s, chunk); };
        const open = [];
        try {
            const cases = [
                [{ piece: 100, delayMs: 10 }, 5], [{ piece: 7, delayMs: 1 }, 20], [{ piece: 1 }, 100],
                // A ClientHello fragmented over several TLS records, in several TCP segments.
                [{ recordSize: 40, piece: 30, delayMs: 5 }, 2], [{ recordSize: 1, piece: 3 }, 1],
            ];
            for (const [opts, minReads] of cases) {
                reads = 0;
                const c = await tlsOverRaw(port, opts);
                open.push(c);
                assert.ok(reads >= minReads, `the ClientHello came in ${reads} reads (piece ${opts.piece})`);
                assert.equal(await echo(c), 'ping');
                assert.equal(gate.pending, 0);
            }
        } finally {
            for (const s of open) s.destroy();
            await close();
        }
    });

    it('hands TLS the bytes that came after the first record in the same read', async () => {
        const { gate, port, close } = await gatedServer({ maxPending: 4 });
        const open = [];
        try {
            // Both send a ClientHello; `extra` sends an application data record with it, in one
            // write, which TLS refuses at once (before the handshake completes) if it gets it.
            const stalled = await stalledHello(port, hello);
            const extra = await stalledHello(port, Buffer.concat([hello, Buffer.from([23, 3, 3, 0, 4, 1, 2, 3, 4])]));
            open.push(stalled, extra);
            await waitFor(() => extra.destroyed);
            assert.equal(stalled.destroyed, false, 'the ClientHello alone waits for the handshake timeout');
            await waitFor(() => gate.pending === 1);
        } finally {
            for (const s of open) s.destroy();
            await close();
        }
    });

    it('closes sockets over the cap before any TLS work; completed and failed handshakes give their slot back', async () => {
        const env = await gatedServer({ maxPending: 2, maxPendingPerIp: 2 });
        const { registry, gate, accepted, port, close } = env;
        const open = [];
        try {
            // Two clients that sent their ClientHello and stopped hold both slots.
            const stalled1 = await stalledHello(port, hello), stalled2 = await stalledHello(port, hello);
            open.push(stalled1, stalled2);
            await waitFor(() => gate.pending === 2);
            await assert.rejects(tlsConnect(port));
            assert.equal(env.tlsWork, 2, 'the refused socket never reached TLS');
            assert.equal(refused(registry, 'handshakes'), 1);
            assert.equal(accepted.length, 0);
            // A slot frees when a pending socket closes.
            stalled1.destroy();
            await waitFor(() => gate.pending === 1);
            // Completed handshakes give their slot back: more TLS connections than slots stay open.
            for (let i = 0; i < 4; i++) {
                open.push(await tlsConnect(port));
                await waitFor(() => accepted.length === i + 1 && gate.pending === 1);
            }
            assert.equal(accepted.length, 4);
            // A failed handshake (a ClientHello that makes no sense) gives its slot back too, and
            // plain HTTP on the TLS port never reaches TLS.
            const bad = await rawConnect(port);
            open.push(bad);
            bad.on('error', () => {});
            bad.write(helloRecord(300));
            await waitFor(() => gate.pending === 1 && bad.destroyed);
            const http = await rawConnect(port);
            open.push(http);
            http.on('error', () => {});
            http.write('GET / HTTP/1.1\r\nHost: x\r\n\r\n');
            await waitFor(() => http.destroyed);
            assert.equal(refused(registry, 'bad_hello'), 1);
            stalled2.destroy();
            await waitFor(() => gate.pending === 0);
            assert.equal(gate.perIp.size, 0);
            assert.equal(gate.waiting, 0);
        } finally {
            for (const s of open) s.destroy();
            for (const s of accepted) s.destroy();
            await close();
        }
    });

    it('destroys a socket whose handshake timed out: its slot comes back and a late client cannot complete', async () => {
        const { gate, server, accepted, port, close } = await gatedServer({ maxPending: 4, helloTimeoutMs: 300 }, { handshakeTimeout: 300 });
        const open = [];
        const settled = () => waitFor(async () => gate.pending === 0 && gate.waiting === 0 && (await connections(server)) === 0);
        try {
            const s = await stalledHello(port, hello);
            open.push(s);
            let answered = 0;
            s.on('data', (d) => { answered += d.length; });
            let closed = false;
            s.once('close', () => { closed = true; });
            await waitFor(() => gate.pending === 1);
            await waitFor(() => closed);                     // closed by the server, not by the client
            assert.ok(answered > 0, 'the server did its part of the handshake');
            await settled();
            // A client whose second flight comes after the timeout finds the socket closed, and
            // so does one whose ClientHello comes after the deadline of the gate.
            await lateClientClosed(port, 600);
            await assert.rejects(tlsOverRaw(port, { firstDelayMs: 600 }));
            assert.equal(accepted.length, 0, 'no handshake completed on the server');
            await settled();
        } finally {
            for (const s of open) s.destroy();
            await close();
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
            assert.equal(lst.gate.maxPendingPerIp, 1, 'MAX_PENDING_HANDSHAKES / 32, below the total; not MAX_CONNECTIONS_PER_IP');
            assert.equal(config.maxPendingHandshakesPerIp, 1, 'the configuration holds the same value (check-config prints it)');
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

    it('Listeners on a shared port (the default): idle and timed-out sockets are closed, slots come back, and the API is shed while full', async () => {
        const registry = new Registry();
        const port = await new Promise((resolve) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); }); });
        const config = {
            ...testConfig({ MAX_PENDING_HANDSHAKES: '2' }), tlsMode: 'native', bindAddress: '127.0.0.1', apiPort: port, wsPort: port,
            tlsMinVersion: 'TLSv1.2', tlsCertFile: cert.cert, tlsKeyFile: cert.key,
        };
        let full = false;
        const wss = new WsServer({ registry, onConnection: () => {} });
        const lst = new Listeners({ config, wsServer: wss, apiHandler: null, full: () => full, registry, handshakeTimeoutMs: 300, helloTimeoutMs: 300 });
        await lst.listen();
        const { gate } = lst;
        const [{ kind, server }] = lst.servers;
        const open = [], raw = new Set();
        server.on('connection', (s) => { raw.add(s); s.once('close', () => raw.delete(s)); });
        const settled = () => waitFor(async () => gate.pending === 0 && gate.waiting === 0 && (await connections(server)) === 0);
        try {
            assert.deepEqual([lst.servers.length, kind, server instanceof https.Server], [1, 'api+ws', true]);
            // The API and the upgrade work through the gate.
            assert.equal(await httpsGet(port, '/api/v1/healthz'), 200);
            const ws = await connectWs({ port, tls: { rejectUnauthorized: false } });
            assert.equal(ws.protocol, 'scacelith.v1');
            ws.destroy();
            await settled();
            // A socket that never sends its ClientHello is closed by the server.
            const idle = await rawConnect(port);
            open.push(idle);
            idle.on('error', () => {});
            await waitFor(() => idle.destroyed);
            assert.equal(refused(registry, 'hello_timeout'), 1);
            await settled();
            // A handshake that times out is closed by the server too; a FIN sent after the timeout
            // does not leave the socket open (CLOSE_WAIT) either.
            const stalled = await stalledHello(port, hello);
            open.push(stalled);
            await waitFor(() => gate.pending === 1);
            await waitFor(() => gate.pending === 0);
            stalled.end();
            await settled();
            // A client that completes its part after the timeout cannot finish the handshake.
            let secure = 0;
            server.on('secureConnection', () => secure++);
            await lateClientClosed(port, 600);
            assert.equal(secure, 0);
            await settled();
            // While full, the shared port lets one new connection per second through (API and
            // upgrade alike) and closes the others before TLS; the slots come back.
            full = true;
            open.push(await tlsConnect(port));
            await assert.rejects(httpsGet(port, '/api/v1/healthz'));
            assert.equal(refused(registry, 'server_full'), 1);
            await waitFor(() => gate.pending === 0);
            full = false;
            assert.equal(await httpsGet(port, '/api/v1/healthz'), 200);
        } finally {
            for (const s of [...open, ...raw]) s.destroy();
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
        const now = performance.now();
        assert.equal(router.isFull(now), true);
        assert.equal(router.isFull(now + FULL_HOLD_MS + 1), false, 'the signal expires');
        answer({ ok: true });
        assert.equal(await router.admission.acquire('192.0.2.1'), true);
        assert.equal(router.isFull(), false, 'an admitted upgrade ends it');
    });

    it('measures FULL_HOLD_MS on a monotonic clock: a wall-clock step does not prolong it', async () => {
        const { router, answer } = mkRouter();
        const realNow = Date.now;
        try {
            Date.now = () => realNow() + 3600e3;             // the wall clock is an hour ahead at the refusal...
            answer({ ok: false, reason: 'global' });
            await router.admission.acquire('192.0.2.1');
            Date.now = () => realNow() - 3600e3;             // ...and is stepped back an hour afterwards
            assert.equal(router.isFull(), true);
            assert.equal(router.isFull(performance.now() + FULL_HOLD_MS + 1), false);
        } finally {
            Date.now = realNow;
        }
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

describe('config: MAX_PENDING_HANDSHAKES_PER_IP', () => {
    it('defaults to a share of MAX_PENDING_HANDSHAKES and must stay below it', () => {
        // Empty: loadConfig stores the effective value (before, it stayed null and only the gate
        // derived it, so check-config could not show it).
        assert.equal(testConfig().maxPendingHandshakesPerIp, 4, 'empty: MAX_PENDING_HANDSHAKES / 32');
        assert.equal(testConfig({ MAX_PENDING_HANDSHAKES: '2' }).maxPendingHandshakesPerIp, 1, 'always below the total');
        assert.equal(testConfig({ MAX_PENDING_HANDSHAKES: '64' }).maxPendingHandshakesPerIp, 2, 'floor of 2');
        assert.equal(testConfig({ MAX_PENDING_HANDSHAKES: '4096' }).maxPendingHandshakesPerIp, 128);
        assert.equal(describeConfig(testConfig()).maxPendingHandshakesPerIp, 4, 'what check-config prints');
        assert.equal(testConfig({ MAX_PENDING_HANDSHAKES_PER_IP: '8' }).maxPendingHandshakesPerIp, 8);
        assert.equal(testConfig({ MAX_CONNECTIONS_PER_IP: '100000' }).maxPendingHandshakesPerIp, 4, 'not tied to MAX_CONNECTIONS_PER_IP');
        assert.throws(() => testConfig({ MAX_PENDING_HANDSHAKES_PER_IP: '128' }), /MAX_PENDING_HANDSHAKES_PER_IP must be lower than MAX_PENDING_HANDSHAKES/);
        assert.throws(() => testConfig({ MAX_PENDING_HANDSHAKES: '16', MAX_PENDING_HANDSHAKES_PER_IP: '20' }), /must be lower/);
        assert.throws(() => testConfig({ MAX_PENDING_HANDSHAKES: '1' }), /MAX_PENDING_HANDSHAKES: at least 2/);
        assert.equal(testConfig({ MAX_PENDING_HANDSHAKES: '16', MAX_PENDING_HANDSHAKES_PER_IP: '15' }).maxPendingHandshakesPerIp, 15);
    });

    it('check-config prints the value the gate uses', () => {
        const run = (extra) => {
            const r = spawnSync(process.execPath, [fileURLToPath(new URL('../../bin/scacelith-server.js', import.meta.url)), 'check-config'], {
                cwd: os.tmpdir(), encoding: 'utf8',
                env: {
                    PATH: process.env.PATH, SCACELITH_ENV_FILE: '', SERVER_SECRET: Buffer.alloc(48, 7).toString('base64'),
                    TLS_MODE: 'off', ALLOW_INSECURE_DEV: '1', ...extra,
                },
            });
            assert.equal(r.status, 0, r.stderr);
            return JSON.parse(r.stdout);
        };
        const def = run({});
        assert.deepEqual([def.maxPendingHandshakes, def.maxPendingHandshakesPerIp], [128, 4]);
        const small = run({ MAX_PENDING_HANDSHAKES: '2' });
        assert.deepEqual([small.maxPendingHandshakes, small.maxPendingHandshakesPerIp], [2, 1]);
        for (const n of [2, 64, 128, 4096]) {
            const gate = new TlsGate({ maxPending: n, maxPendingPerIp: testConfig({ MAX_PENDING_HANDSHAKES: String(n) }).maxPendingHandshakesPerIp, registry: new Registry() });
            assert.equal(gate.maxPendingPerIp, new TlsGate({ maxPending: n, registry: new Registry() }).maxPendingPerIp, `same value as the gate's own default (${n})`);
        }
    });
});
