import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import http from 'node:http';
import net from 'node:net';
import { after, before, describe, it } from 'node:test';
import { testConfig } from '../../src/config.js';
import { Registry } from '../../src/metrics.js';
import { ipMatcher } from '../../src/net/ip.js';
import { IpGuard } from '../../src/net/ipguard.js';
import { WsServer, parseRequestHead, acceptKey } from '../../src/net/ws.js';
import { connectWs } from '../../src/net/ws-raw-client.js';

function listenRaw(wss) {
    return new Promise((resolve) => {
        const srv = net.createServer((s) => wss.handleSocket(s));
        srv.listen(0, '127.0.0.1', () => resolve(srv));
    });
}

function listenHttp(wss) {
    return new Promise((resolve) => {
        const srv = http.createServer((req, res) => { res.writeHead(200); res.end('api'); });
        srv.on('upgrade', (req, socket, head) => wss.handleUpgrade(req, socket, head));
        srv.listen(0, '127.0.0.1', () => resolve(srv));
    });
}

function rawRequest(port, text) {
    return new Promise((resolve) => {
        const s = net.connect(port, '127.0.0.1', () => s.write(text));
        let buf = '';
        s.on('data', (d) => { buf += d.toString('latin1'); });
        s.on('close', () => resolve(buf));
        s.on('error', () => resolve(buf));
    });
}

for (const mode of ['raw', 'http']) {
    describe(`ws handshake (${mode})`, () => {
        let srv, port, wss;
        const opened = [];
        const admission = { mode: 'ok', acquired: [], released: [] };
        before(async () => {
            wss = new WsServer({
                registry: new Registry(), allowOrigins: ['https://play.example.org'], handshakeTimeoutMs: 300,
                upgradeHeaders: { 'Scacelith-Server-Id': 'srv-test' },
                onConnection: (c) => { opened.push(c); c.onMessage = (cn, b) => cn.sendFrame(Buffer.from(b)); },
                admission: {
                    acquire(ip) {
                        admission.acquired.push(ip);
                        if (admission.mode === 'refuse') return { ok: false, status: 429, error: 'too_many_connections' };
                        if (admission.mode === 'async') return new Promise((r) => setTimeout(() => r(true), 20));
                        return true;
                    },
                    release(ip) { admission.released.push(ip); },
                },
            });
            srv = mode === 'raw' ? await listenRaw(wss) : await listenHttp(wss);
            port = srv.address().port;
        });
        after(() => { wss.closeAll(); srv.close(); });

        it('accepts a valid upgrade and echoes the subprotocol', async () => {
            const c = await connectWs({ port, protocols: ['other', 'scacelith.v1'] });
            assert.equal(c.protocol, 'scacelith.v1');
            assert.equal(c.headers['scacelith-server-id'], 'srv-test');     // upgradeHeaders
            c.send(Buffer.from([1, 2, 3]));
            assert.deepEqual([...await c.next()], [1, 2, 3]);
            c.close(1000);
            assert.equal((await c.closed).code, 1000);
        });

        it('refuses methods other than GET (405)', async () => {
            await assert.rejects(connectWs({ port, method: 'POST' }), (e) => e.status === 405);
        });

        it('refuses HTTP/1.0 (400)', async () => {
            await assert.rejects(connectWs({ port, httpVersion: '1.0' }), (e) => e.status === 400 || e.status === 505);
        });

        it('refuses other paths (404)', async () => {
            await assert.rejects(connectWs({ port, path: '/other' }), (e) => e.status === 404);
        });

        it('requires the Upgrade and Connection tokens (400)', async () => {
            await assert.rejects(connectWs({ port, headers: { Upgrade: 'h2c' } }), (e) => e.status === 400);
            if (mode === 'raw') await assert.rejects(connectWs({ port, headers: { Connection: 'keep-alive' } }), (e) => e.status === 400);
        });

        it('requires version 13 (426 with Sec-WebSocket-Version)', async () => {
            await assert.rejects(connectWs({ port, headers: { 'Sec-WebSocket-Version': '8' } }), (e) => e.status === 426 && e.headers['sec-websocket-version'] === '13');
        });

        it('validates the key format (400)', async () => {
            await assert.rejects(connectWs({ port, headers: { 'Sec-WebSocket-Key': 'short' } }), (e) => e.status === 400);
            await assert.rejects(connectWs({ port, headers: { 'Sec-WebSocket-Key': 'AAAAAAAAAAAAAAAAAAAAAB==' } }), (e) => e.status === 400);
        });

        it('requires the scacelith.v1 subprotocol (426 JSON)', async () => {
            await assert.rejects(connectWs({ port, protocols: ['chat'] }), (e) => {
                assert.equal(e.status, 426);
                assert.deepEqual(JSON.parse(e.body), { error: 'unsupported_protocol', supported: ['scacelith.v1'] });
                return true;
            });
            await assert.rejects(connectWs({ port, protocols: null }), (e) => e.status === 426);
        });

        it('refuses an Origin that is not allowed (403), accepts an allowed one', async () => {
            await assert.rejects(connectWs({ port, headers: { Origin: 'https://evil.example' } }), (e) => e.status === 403);
            const c = await connectWs({ port, headers: { Origin: 'https://play.example.org' } });
            c.destroy();
        });

        it('applies the admission hook (429) and releases admitted connections', async () => {
            admission.mode = 'refuse';
            await assert.rejects(connectWs({ port }), (e) => e.status === 429);
            admission.mode = 'async';
            const before = admission.released.length;
            const c = await connectWs({ port });
            c.close(1000);
            await c.closed;
            await new Promise((r) => setTimeout(r, 50));
            assert.equal(admission.released.length, before + 1);
            admission.mode = 'ok';
        });

        it('refuses new upgrades while shutting down (503)', async () => {
            wss.accepting = false;
            await assert.rejects(connectWs({ port }), (e) => e.status === 503);
            wss.accepting = true;
        });

        if (mode === 'raw') {
            it('answers malformed requests with 400', async () => {
                const r = await rawRequest(port, 'GET /ws HTTP/1.1\r\nBad Header\r\n\r\n');
                assert.match(r, /^HTTP\/1\.1 400/);
                const r2 = await rawRequest(port, 'GET /ws HTTP/1.1\r\nHost: x\r\n folded: y\r\n\r\n');
                assert.match(r2, /^HTTP\/1\.1 400/);
            });

            it('refuses oversized request heads (431)', async () => {
                const r = await rawRequest(port, 'GET /ws HTTP/1.1\r\nX-Pad: ' + 'a'.repeat(9000) + '\r\n\r\n');
                assert.match(r, /^HTTP\/1\.1 431/);
            });

            it('times out an incomplete request (408)', async () => {
                const r = await rawRequest(port, 'GET /ws HTTP/1.1\r\nHost: x\r\n');
                assert.match(r, /^HTTP\/1\.1 408/);
            });

            it('delivers frames sent in the same packet as the request', async () => {
                const key = 'dGhlIHNhbXBsZSBub25jZQ==';
                const frame = Buffer.from([0x82, 0x83, 1, 2, 3, 4, 7 ^ 1, 8 ^ 2, 9 ^ 3]);
                const s = net.connect(port, '127.0.0.1');
                const req = `GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: ${key}\r\n` +
                    'Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: scacelith.v1\r\n\r\n';
                s.write(Buffer.concat([Buffer.from(req), frame]));
                const data = await new Promise((resolve) => {
                    let b = Buffer.alloc(0);
                    s.on('data', (d) => {
                        b = Buffer.concat([b, d]);
                        const end = b.indexOf('\r\n\r\n');
                        if (end >= 0 && b.length >= end + 4 + 5) resolve(b);
                    });
                });
                const text = data.toString('latin1');
                assert.match(text, /101 Switching Protocols/);
                assert.ok(text.includes(`Sec-WebSocket-Accept: ${acceptKey(key)}`));
                const end = data.indexOf('\r\n\r\n') + 4;
                assert.deepEqual([...data.subarray(end, end + 5)], [0x82, 3, 7, 8, 9]);
                s.destroy();
            });
        }
    });
}

describe('ws handshake on a raw socket: client gone, head in pieces', () => {
    const rejected = (registry, reason) => {
        const m = registry.metrics.get('scacelith_ws_handshakes_rejected_total');
        return [...m.children.values()].find((c) => c.labelValues[0] === reason)?.value ?? 0;
    };

    it('a client that leaves before the end of its head is no handshake timeout', async () => {
        const registry = new Registry();
        const wss = new WsServer({ registry, handshakeTimeoutMs: 200, onConnection: () => {} });
        const srv = await listenRaw(wss);
        const port = srv.address().port;
        try {
            const s = net.connect(port, '127.0.0.1');
            s.on('error', () => {});
            await new Promise((r) => s.once('connect', r));
            s.write('GET /ws HTTP/1.1\r\nHo');
            await new Promise((r) => setTimeout(r, 50));
            s.destroy();
            await new Promise((r) => setTimeout(r, 400));
            assert.equal(rejected(registry, 'timeout'), 0);
            assert.match(await rawRequest(port, 'GET /ws HTTP/1.1\r\nHost: x\r\n'), /^HTTP\/1\.1 408/, 'one that stays still gets 408');
            assert.equal(rejected(registry, 'timeout'), 1);
        } finally { srv.close(); }
    });

    it('a head read in any pieces gets the same answer, and the same frames after it, as in one read', () => {
        // handleSocket on a fake socket: what it writes and what the connection delivers.
        function run(chunks) {
            const out = [], msgs = [];
            const wss = new WsServer({
                registry: new Registry(), handshakeTimeoutMs: 100000, maxHeaderBytes: 512,
                onConnection: (c) => { c.onMessage = (cn, b) => msgs.push(Buffer.from(b).toString('hex')); },
            });
            const s = new EventEmitter();
            s.setNoDelay = s.pause = s.resume = s.setTimeout = s.cork = s.uncork = () => {};
            s.end = (x) => { out.push('END:' + (x ? String(x).split('\r\n')[0] : '')); };
            s.destroy = () => { s.destroyed = true; };
            s.write = (x) => { out.push(Buffer.from(x).toString('latin1')); return true; };
            s.remoteAddress = '192.0.2.1';
            s.writableLength = 0;
            wss.handleSocket(s);
            for (const c of chunks) if (s.listenerCount('data')) s.emit('data', Buffer.from(c));
            return JSON.stringify([out, msgs]);
        }
        let seed = 7;
        const rnd = (n) => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed % n; };
        const keys = ['dGhlIHNhbXBsZSBub25jZQ==', 'bad', 'AAAAAAAAAAAAAAAAAAAAAA=='];
        for (let t = 0; t < 300; t++) {
            const pad = 'p'.repeat(rnd(4) === 0 ? 400 + rnd(200) : rnd(50));       // around maxHeaderBytes: 431 or not
            const head = `GET ${rnd(5) ? '/ws' : '/x'} HTTP/1.${rnd(6) ? 1 : 0}\r\nHost: h\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n` +
                `Sec-WebSocket-Key: ${keys[rnd(3)]}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: scacelith.v1\r\nX-P: ${pad}\r\n` +
                `${rnd(10) ? '' : 'Bad Line\r\n'}\r\n`;
            const frames = [];
            for (let i = rnd(3); i > 0; i--) frames.push(Buffer.from([0x82, 0x83, 1, 2, 3, 4, rnd(256), rnd(256), rnd(256)]));
            const all = Buffer.concat([Buffer.from(head, 'latin1'), ...frames]);
            const pieces = [];
            for (let o = 0; o < all.length;) { const n = 1 + rnd(rnd(2) ? 5 : 200); pieces.push(all.subarray(o, o + n)); o += n; }
            assert.equal(run(pieces), run([all]), `head ${t}`);
        }
    });
});

describe('ws handshake on a raw socket: unreadable heads and the address guard', () => {
    // Malformed HTTP on the API port counts 1 toward a block of the address (hardenHttp); so does a
    // head the dedicated port cannot read, unless the peer is a trusted proxy.
    async function counted(isTrusted) {
        const config = testConfig({ ABUSE_BLOCK_REFUSALS_PER_MIN: '100000' });
        const guard = new IpGuard({ config, workers: 1, registry: new Registry(), report: () => {} });
        const wss = new WsServer({
            registry: new Registry(), handshakeTimeoutMs: 200, maxHeaderBytes: 512, onConnection: () => {}, guard, isTrusted,
        });
        const srv = await listenRaw(wss);
        const port = srv.address().port;
        try {
            assert.match(await rawRequest(port, 'GET /ws HTTP/1.1\r\nBad Header\r\n\r\n'), /^HTTP\/1\.1 400/);
            assert.match(await rawRequest(port, 'GET /ws HTTP/1.1\r\nX-Pad: ' + 'a'.repeat(600) + '\r\n\r\n'), /^HTTP\/1\.1 431/);
            assert.match(await rawRequest(port, 'GET /ws HTTP/1.1\r\nHost: x\r\n'), /^HTTP\/1\.1 408/);
            // A client that leaves before the end of its head is neither refused nor counted.
            const s = net.connect(port, '127.0.0.1');
            s.on('error', () => {});
            await new Promise((r) => s.once('connect', r));
            s.write('GET /ws HTTP/1.1\r\nHo');
            await new Promise((r) => setTimeout(r, 50));
            s.destroy();
            await new Promise((r) => setTimeout(r, 300));
            return guard.flushReports();
        } finally { srv.close(); guard.close(); }
    }

    it('400, 431 and 408 count 1 each toward a block of the address', async () => {
        assert.deepEqual(await counted(null), [['127.0.0.1', null, 3]]);
    });

    it('behind a proxy, only the peers outside TRUSTED_PROXIES are counted', async () => {
        assert.deepEqual(await counted(ipMatcher(['127.0.0.1'])), []);
        assert.deepEqual(await counted(ipMatcher(['10.0.0.1'])), [['127.0.0.1', null, 3]]);
    });
});

describe('ws upgrade headers', () => {
    it('refuses header names and values that would break the 101 response', () => {
        const make = (h) => new WsServer({ registry: new Registry(), upgradeHeaders: h });
        assert.throws(() => make({ 'Scacelith-Server-Id': 'a\r\nSet-Cookie: x=1' }), /invalid upgrade header/);
        assert.throws(() => make({ 'Bad Name': 'x' }), /invalid upgrade header/);
        assert.equal(make({ 'Scacelith-Server-Id': '0b6f3c1e-2a4d-4c55-9a1f-7d2c9e8b1a33' })._upgradeExtra,
            'Scacelith-Server-Id: 0b6f3c1e-2a4d-4c55-9a1f-7d2c9e8b1a33\r\n');
        assert.equal(make(null)._upgradeExtra, '');
    });
});

describe('parseRequestHead', () => {
    it('parses a request and joins duplicate headers', () => {
        const r = parseRequestHead(Buffer.from('GET /ws?x=1 HTTP/1.1\r\nHost: a\r\nSec-WebSocket-Protocol: a\r\nsec-websocket-protocol: b'));
        assert.equal(r.method, 'GET');
        assert.equal(r.url, '/ws?x=1');
        assert.equal(r.httpVersion, '1.1');
        assert.equal(r.headers['sec-websocket-protocol'], 'a, b');
    });
    it('refuses control characters, bad names and bad request lines', () => {
        assert.equal(parseRequestHead(Buffer.from('GET /ws HTTP/1.1\r\nX: a\x01b')), null);
        assert.equal(parseRequestHead(Buffer.from('GET /ws HTTP/1.1\r\nX Y: a')), null);
        assert.equal(parseRequestHead(Buffer.from('get /ws HTTP/1.1')), null);
        assert.equal(parseRequestHead(Buffer.from('GET /ws HTTP/1.1\r\n' + 'X: a\r\n'.repeat(70))), null);
    });
    it('computes the RFC 6455 accept value', () => {
        assert.equal(acceptKey('dGhlIHNhbXBsZSBub25jZQ=='), 's3pPLMBiTxaQ9kYGzzhZRbK+xOo=');
    });
});
