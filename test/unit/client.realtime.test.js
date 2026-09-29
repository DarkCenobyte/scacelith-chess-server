// Node SDK, realtime side: WsClient (RFC 6455 client) and ScacelithClient, against a tiny
// WebSocket server written here (handshake, masked-frame parsing, scripted answers).

import { test, describe, after } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import tls from 'node:tls';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { WsClient, buildFrame, isValidCloseCode, OPEN, CLOSED } from '../../src/client/ws-client.js';
import { ScacelithClient, ScacelithError } from '../../src/client/client.js';
import * as P from '../../src/protocol/index.js';

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- tiny WebSocket server ---------------------------------------------------------------------

function serverFrame(op, payload = Buffer.alloc(0), { fin = true, mask = false, rsv = 0, len } = {}) {
    const n = len ?? payload.length;
    const hdr = n < 126 ? 2 : n < 65536 ? 4 : 10;
    const b = Buffer.alloc(hdr + (mask ? 4 : 0) + payload.length);
    b[0] = (fin ? 0x80 : 0) | rsv | op;
    if (n < 126) b[1] = n; else if (n < 65536) { b[1] = 126; b.writeUInt16BE(n, 2); } else { b[1] = 127; b.writeUInt32BE(Math.floor(n / 2 ** 32), 2); b.writeUInt32BE(n >>> 0, 6); }
    if (mask) b[1] |= 0x80;
    payload.copy(b, hdr + (mask ? 4 : 0));
    return b;
}
const closeBody = (code, reason = '') => Buffer.concat([Buffer.from([code >> 8, code & 0xff]), Buffer.from(reason)]);

class Peer {
    constructor(socket) {
        this.socket = socket;
        this.request = null;
        this.frames = [];          // every client frame: { op, fin, masked, payload, mask }
        this.messages = [];        // reassembled data messages: { data, isBinary }
        this.closeFrame = null;
        this.ended = false;
        this._waiters = [];
        this._buf = Buffer.alloc(0);
        this._frags = null;
        socket.on('end', () => { this.ended = true; socket.end(); this._wake(); });
        socket.on('close', () => { this.ended = true; this._wake(); });
        socket.on('error', () => {});
    }
    _wake() { for (const w of this._waiters.splice(0)) w(); }
    async until(pred, ms = 3000) {
        const t0 = Date.now();
        while (!pred()) {
            if (Date.now() - t0 > ms) throw new Error('peer: condition not met in time');
            await new Promise((r) => { this._waiters.push(r); setTimeout(r, 20); });
        }
    }
    async nextMessage(ms = 3000) {
        const i = this._read = (this._read ?? 0);
        await this.until(() => this.messages.length > i, ms);
        this._read = i + 1;
        return this.messages[i];
    }
    feed(chunk) {
        this._buf = Buffer.concat([this._buf, chunk]);
        for (;;) {
            const b = this._buf;
            if (b.length < 2) return;
            let len = b[1] & 0x7f, off = 2;
            if (len === 126) { if (b.length < 4) return; len = b.readUInt16BE(2); off = 4; } else if (len === 127) { if (b.length < 10) return; len = Number(b.readBigUInt64BE(2)); off = 10; }
            const masked = (b[1] & 0x80) !== 0;
            const mask = masked ? b.subarray(off, off + 4) : null;
            if (masked) off += 4;
            if (b.length < off + len) return;
            const payload = Buffer.from(b.subarray(off, off + len));
            if (mask) for (let i = 0; i < len; i++) payload[i] ^= mask[i & 3];
            const op = b[0] & 0x0f, fin = (b[0] & 0x80) !== 0;
            this._buf = b.subarray(off + len);
            this.frames.push({ op, fin, masked, payload, mask: mask && Buffer.from(mask) });
            if (op === 8) {
                this.closeFrame = { code: payload.length >= 2 ? payload.readUInt16BE(0) : 1005, reason: payload.subarray(2).toString() };
                if (!this.closeSent) { this.closeSent = true; this.socket.write(serverFrame(8, payload.subarray(0, 2))); }
                this.socket.end();
            } else if (op === 9) {
                this.socket.write(serverFrame(10, payload));
            } else if (op === 0 || op === 1 || op === 2) {
                if (op !== 0) this._frags = { op, parts: [] };
                this._frags.parts.push(payload);
                if (fin) { this.messages.push({ data: Buffer.concat(this._frags.parts), isBinary: this._frags.op === 2 }); this._frags = null; }
            }
            this._wake();
            if (this.onFrame) this.onFrame(this.frames[this.frames.length - 1]);
        }
    }
    send(buf) { this.socket.write(serverFrame(2, Buffer.from(buf))); }
    sendText(s) { this.socket.write(serverFrame(1, Buffer.from(s))); }
    frame(op, payload, opts) { this.socket.write(serverFrame(op, payload ? Buffer.from(payload) : undefined, opts)); }
    raw(bytes) { this.socket.write(bytes); }
    close(code = 1000, reason = '') { this.closeSent = true; this.socket.write(serverFrame(8, closeBody(code, reason))); }
}

// handshake(req) may return { status, headers, body, skip } to answer something else than a
// normal 101 (or skip: never answer).
async function startWsServer({ tlsOptions = null, handshake = null, onPeer = null } = {}) {
    const peers = [];
    const waiters = [];
    const onSocket = (socket) => {
        const peer = new Peer(socket);
        let head = Buffer.alloc(0);
        const onData = (c) => {
            if (peer.request) { peer.feed(c); return; }
            head = Buffer.concat([head, c]);
            const end = head.indexOf('\r\n\r\n');
            if (end < 0) return;
            const [line, ...hl] = head.subarray(0, end).toString('latin1').split('\r\n');
            const headers = {};
            for (const l of hl) { const i = l.indexOf(':'); headers[l.slice(0, i).trim().toLowerCase()] = l.slice(i + 1).trim(); }
            const [method, url] = line.split(' ');
            peer.request = { method, url, line, headers, rawHeaders: hl };
            const custom = handshake ? handshake(peer.request) : null;
            if (custom && custom.skip) return;
            const accept = crypto.createHash('sha1').update(headers['sec-websocket-key'] + GUID).digest('base64');
            const status = custom?.status ?? 101;
            const out = { Upgrade: 'websocket', Connection: 'Upgrade', 'Sec-WebSocket-Accept': accept };
            if (headers['sec-websocket-protocol']) out['Sec-WebSocket-Protocol'] = headers['sec-websocket-protocol'].split(',')[0].trim();
            Object.assign(out, custom?.headers || {});
            for (const k of Object.keys(out)) if (out[k] === null) delete out[k];
            const text = `HTTP/1.1 ${status} ${status === 101 ? 'Switching Protocols' : 'Refused'}\r\n` + Object.entries(out).map(([k, v]) => `${k}: ${v}`).join('\r\n') + '\r\n\r\n';
            socket.write(Buffer.concat([Buffer.from(text, 'latin1'), custom?.body ? Buffer.from(custom.body) : Buffer.alloc(0)]));
            if (status !== 101) { socket.end(); return; }
            const rest = head.subarray(end + 4);
            peers.push(peer);
            if (onPeer) onPeer(peer);
            for (const w of waiters.splice(0)) w(peer);
            if (rest.length) peer.feed(rest);
        };
        socket.on('data', onData);
        socket.on('error', () => {});
    };
    const server = tlsOptions ? tls.createServer(tlsOptions, onSocket) : net.createServer(onSocket);
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const sockets = new Set();
    server.on(tlsOptions ? 'secureConnection' : 'connection', (s) => { sockets.add(s); s.on('close', () => sockets.delete(s)); });
    return {
        port: server.address().port,
        peers,
        nextPeer(ms = 3000) {
            const n = peers.length;
            return new Promise((resolve, reject) => {
                const t = setTimeout(() => reject(new Error('no peer')), ms);
                const check = (p) => { clearTimeout(t); resolve(p); };
                if (peers.length > n) check(peers[n]); else waiters.push(check);
            });
        },
        async close() { for (const s of sockets) s.destroy(); await new Promise((r) => server.close(r)); },
    };
}

async function openPair(extra = {}, serverOpts = {}) {
    const srv = await startWsServer(serverOpts);
    const peerP = srv.nextPeer();
    const ws = await WsClient.connect({ port: srv.port, insecure: true, ...extra });
    const peer = await peerP;
    return { srv, ws, peer };
}

function closeEvent(ws) { return new Promise((r) => ws.on('close', (code, reason) => r({ code, reason }))); }
function nextClientMessage(ws) { return new Promise((r) => ws.once('message', (data, isBinary) => r({ data: Buffer.from(data), isBinary }))); }

// ---- WsClient ------------------------------------------------------------------------------------

describe('WsClient', () => {
    test('upgrade request: path, key, version, subprotocol, custom headers, no Origin', async () => {
        const { srv, ws, peer } = await openPair({ path: '/ws?x=1', headers: { 'X-Test': 'yes' } });
        const h = peer.request.headers;
        assert.equal(peer.request.method, 'GET');
        assert.equal(peer.request.url, '/ws?x=1');
        assert.match(peer.request.line, / HTTP\/1\.1$/);
        assert.equal(h.host, `127.0.0.1:${srv.port}`);
        assert.equal(h.upgrade, 'websocket');
        assert.equal(h.connection, 'Upgrade');
        assert.equal(h['sec-websocket-version'], '13');
        assert.equal(h['sec-websocket-protocol'], 'scacelith.v1');
        assert.equal(Buffer.from(h['sec-websocket-key'], 'base64').length, 16);
        assert.equal(h['x-test'], 'yes');
        assert.equal(h.origin, undefined);
        assert.equal(ws.readyState, OPEN);
        assert.equal(ws.protocol, 'scacelith.v1');
        await ws.close();
        await srv.close();
    });

    test('binary and text messages both ways, every length encoding, masked with fresh keys', async () => {
        const { srv, ws, peer } = await openPair({ maxMessageBytes: 1 << 20 });
        const sizes = [0, 1, 125, 126, 65535, 65536, 200000];
        for (const n of sizes) {
            const data = crypto.randomBytes(n);
            ws.send(data);
            const got = await peer.nextMessage();
            assert.ok(got.isBinary && got.data.equals(data), `client -> server ${n}`);
            const back = nextClientMessage(ws);
            peer.send(data);
            const m = await back;
            assert.ok(m.isBinary && m.data.equals(data), `server -> client ${n}`);
        }
        ws.send('héllo ♞');
        const t = await peer.nextMessage();
        assert.equal(t.isBinary, false);
        assert.equal(t.data.toString(), 'héllo ♞');
        const back = nextClientMessage(ws);
        peer.sendText('ユキ');
        const m = await back;
        assert.equal(m.isBinary, false);
        assert.equal(m.data.toString(), 'ユキ');
        assert.ok(peer.frames.every((f) => f.masked), 'every client frame is masked');
        const keys = new Set(peer.frames.map((f) => f.mask.toString('hex')));
        assert.ok(keys.size >= peer.frames.length - 1, 'mask keys vary');
        assert.equal(typeof ws.bufferedAmount, 'number');
        await ws.close();
        await srv.close();
    });

    test('fragmented incoming message with an interleaved ping; the ping is answered', async () => {
        const { srv, ws, peer } = await openPair();
        const pings = [];
        ws.on('ping', (p) => pings.push(Buffer.from(p).toString()));
        const got = nextClientMessage(ws);
        peer.frame(2, 'abc', { fin: false });
        peer.frame(9, 'ping!');
        peer.frame(0, 'def', { fin: false });
        peer.frame(0, 'ghi', { fin: true });
        const m = await got;
        assert.equal(m.data.toString(), 'abcdefghi');
        assert.equal(m.isBinary, true);
        assert.deepEqual(pings, ['ping!']);
        await peer.until(() => peer.frames.some((f) => f.op === 10));
        assert.equal(peer.frames.find((f) => f.op === 10).payload.toString(), 'ping!');
        // Frames split across TCP chunks, byte by byte.
        const got2 = nextClientMessage(ws);
        const bytes = Buffer.concat([serverFrame(1, Buffer.from('fra'), { fin: false }), serverFrame(0, Buffer.from('gmented'))]);
        for (const b of bytes) { peer.raw(Buffer.from([b])); await sleep(1); }
        assert.equal((await got2).data.toString(), 'fragmented');
        await ws.close();
        await srv.close();
    });

    test('data sent in the same chunk as the 101 response is delivered', async () => {
        const srv = await startWsServer({ handshake: () => ({ headers: {} }) });
        const peerP = srv.nextPeer();
        const ws = new WsClient({ port: srv.port, insecure: true });
        const first = new Promise((r) => ws.on('message', (d) => r(Buffer.from(d))));
        // The server writes the 101 and a frame back to back.
        srv.peers.length = 0;
        const connected = ws.connect();
        const peer = await peerP;
        peer.send(Buffer.from([1, 2, 3]));
        await connected;
        assert.deepEqual([...await first], [1, 2, 3]);
        await ws.close();
        await srv.close();
    });

    test('message larger than maxMessageBytes: closed with 1009 before buffering it', async () => {
        const { srv, ws, peer } = await openPair({ maxMessageBytes: 1000 });
        const closed = closeEvent(ws);
        const errors = [];
        ws.on('error', (e) => errors.push(e));
        peer.raw(serverFrame(2, Buffer.alloc(10), { len: 5000 }));   // header announces 5000 bytes
        const c = await closed;
        assert.equal(c.code, 1009);
        assert.equal(errors[0].closeCode, 1009);
        await peer.until(() => peer.closeFrame);
        assert.equal(peer.closeFrame.code, 1009);
        await srv.close();
    });

    test('fragmented message growing past maxMessageBytes: 1009', async () => {
        const { srv, ws, peer } = await openPair({ maxMessageBytes: 100 });
        const closed = closeEvent(ws);
        peer.frame(2, Buffer.alloc(60), { fin: false });
        peer.frame(0, Buffer.alloc(60), { fin: true });
        assert.equal((await closed).code, 1009);
        await srv.close();
    });

    test('server closing handshake: the code is echoed, the close event reports it', async () => {
        const { srv, ws, peer } = await openPair();
        const closed = closeEvent(ws);
        peer.close(4003, 'unauthorized');
        const c = await closed;
        assert.deepEqual(c, { code: 4003, reason: 'unauthorized' });
        assert.equal(ws.readyState, CLOSED);
        await peer.until(() => peer.frames.some((f) => f.op === 8));
        assert.equal(peer.frames.find((f) => f.op === 8).payload.readUInt16BE(0), 4003);
        assert.equal(ws.send(Buffer.from([1])), false);
        await srv.close();
    });

    test('client closing handshake resolves once the server answered', async () => {
        const { srv, ws, peer } = await openPair();
        const closed = closeEvent(ws);
        await ws.close(4001, 'done');
        assert.equal(peer.closeFrame.code, 4001);
        assert.equal(peer.closeFrame.reason, 'done');
        assert.deepEqual(await closed, { code: 4001, reason: '' });
        assert.throws(() => new WsClient({}).close(1005), RangeError);
        await srv.close();
    });

    test('connection lost without a close frame: 1006', async () => {
        const { srv, ws, peer } = await openPair();
        const closed = closeEvent(ws);
        peer.socket.destroy();
        assert.equal((await closed).code, 1006);
        await srv.close();
    });

    test('protocol violations by the server fail the connection with the right code', async () => {
        const cases = [
            [(p) => p.raw(serverFrame(2, Buffer.from('x'), { mask: true })), 1002, 'masked frame'],
            [(p) => p.raw(serverFrame(2, Buffer.from('x'), { rsv: 0x40 })), 1002, 'RSV1 without extension'],
            [(p) => p.raw(serverFrame(3, Buffer.from('x'))), 1002, 'unknown data opcode'],
            [(p) => p.raw(serverFrame(11, Buffer.from('x'))), 1002, 'unknown control opcode'],
            [(p) => p.raw(serverFrame(0, Buffer.from('x'))), 1002, 'continuation without a message'],
            [(p) => { p.frame(2, 'a', { fin: false }); p.frame(2, 'b'); }, 1002, 'new message inside a fragmented one'],
            [(p) => p.raw(serverFrame(9, Buffer.alloc(126))), 1002, 'control frame too long'],
            [(p) => p.raw(serverFrame(9, Buffer.from('x'), { fin: false })), 1002, 'fragmented control frame'],
            [(p) => p.raw(serverFrame(8, Buffer.from([3]))), 1002, 'close frame of one byte'],
            [(p) => p.raw(serverFrame(8, closeBody(1005))), 1002, 'reserved close code 1005'],
            [(p) => p.raw(serverFrame(8, closeBody(999))), 1002, 'close code 999'],
            [(p) => p.raw(serverFrame(8, Buffer.concat([closeBody(1000), Buffer.from([0xff])]))), 1007, 'close reason not UTF-8'],
            [(p) => p.raw(serverFrame(1, Buffer.from([0xc3, 0x28]))), 1007, 'text not UTF-8'],
            [(p) => p.raw(Buffer.from([0x82, 126, 0, 5, 1, 2, 3, 4, 5])), 1002, 'non-minimal 16-bit length'],
        ];
        for (const [act, code, what] of cases) {
            const { srv, ws, peer } = await openPair();
            const closed = closeEvent(ws);
            ws.on('error', () => {});
            act(peer);
            assert.equal((await closed).code, code, what);
            await srv.close();
        }
    });

    test('handshake refusals and mismatches reject connect()', async () => {
        const cases = [
            [{ status: 403, body: '{"error":"origin_refused"}' }, (e) => e.status === 403 && /403/.test(e.message) && e.body.includes('origin_refused')],
            [{ status: 503 }, (e) => e.status === 503],
            [{ headers: { 'Sec-WebSocket-Accept': 'bm9wZQ==' } }, (e) => /Accept/.test(e.message)],
            [{ headers: { 'Sec-WebSocket-Protocol': null } }, (e) => /subprotocol/.test(e.message)],
            [{ headers: { 'Sec-WebSocket-Protocol': 'other.v2' } }, (e) => /subprotocol/.test(e.message)],
            [{ headers: { 'Sec-WebSocket-Extensions': 'permessage-deflate' } }, (e) => /extension/.test(e.message)],
            [{ headers: { Upgrade: 'h2c' } }, (e) => /Upgrade/.test(e.message)],
        ];
        for (const [answer, check] of cases) {
            const srv = await startWsServer({ handshake: () => answer });
            await assert.rejects(WsClient.connect({ port: srv.port, insecure: true }), (e) => e.code === 'WS_HANDSHAKE' && check(e), JSON.stringify(answer));
            await srv.close();
        }
        const silent = await startWsServer({ handshake: () => ({ skip: true }) });
        const t0 = Date.now();
        await assert.rejects(WsClient.connect({ port: silent.port, insecure: true, handshakeTimeoutMs: 200 }), /handshake timeout/);
        assert.ok(Date.now() - t0 < 2000);
        await silent.close();
        await assert.rejects(WsClient.connect({ port: 1, insecure: true }), (e) => e.code === 'ECONNREFUSED');
        await assert.rejects(WsClient.connect({ port: 2, insecure: true, headers: { 'X-Bad': 'a\r\nb' } }), /invalid header/);
        // No subprotocol requested: none may be chosen.
        const plain = await startWsServer();
        const ws = await WsClient.connect({ port: plain.port, insecure: true, subprotocol: null });
        assert.equal(ws.protocol, '');
        await ws.close();
        await plain.close();
    });

    test('send before open throws; frames are built as RFC 6455 requires', () => {
        assert.throws(() => { const w = new WsClient({}); w.readyState = 0; w.send(Buffer.alloc(1)); }, /not open/);
        const f = buildFrame(2, Buffer.from([1, 2, 3]));
        assert.equal(f[0], 0x82);
        assert.equal(f[1], 0x80 | 3);
        assert.deepEqual([...f.subarray(6)].map((b, i) => b ^ f[2 + (i & 3)]), [1, 2, 3]);
        assert.equal(buildFrame(2, Buffer.alloc(300))[1], 0x80 | 126);
        assert.equal(buildFrame(2, Buffer.alloc(70000))[1], 0x80 | 127);
        assert.equal(isValidCloseCode(1000), true);
        assert.equal(isValidCloseCode(1006), false);
        assert.equal(isValidCloseCode(4302), true);
        assert.equal(isValidCloseCode(5000), false);
    });
});

// ---- TLS -----------------------------------------------------------------------------------------

function haveOpenssl() {
    try { execFileSync('openssl', ['version'], { stdio: 'ignore' }); return true; } catch { return false; }
}

describe('WsClient over TLS', { skip: !haveOpenssl() && 'openssl not found' }, () => {
    let dir, cert, key;
    after(() => { if (dir) fs.rmSync(dir, { recursive: true, force: true }); });
    test('connects with the CA, refuses an unknown certificate', async () => {
        dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-ws-tls-'));
        execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-days', '2',
            '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1',
            '-keyout', path.join(dir, 'key.pem'), '-out', path.join(dir, 'cert.pem')], { stdio: 'ignore' });
        cert = fs.readFileSync(path.join(dir, 'cert.pem'));
        key = fs.readFileSync(path.join(dir, 'key.pem'));
        const srv = await startWsServer({ tlsOptions: { cert, key } });
        const peerP = srv.nextPeer();
        const ws = await WsClient.connect({ port: srv.port, ca: cert });
        const peer = await peerP;
        ws.send(Buffer.from('over tls'));
        assert.equal((await peer.nextMessage()).data.toString(), 'over tls');
        await ws.close();
        const ws2 = await WsClient.connect({ host: 'localhost', port: srv.port, ca: cert });
        await ws2.close();
        await assert.rejects(WsClient.connect({ port: srv.port }), (e) => /self[- ]signed|unable to verify|certificate/i.test(e.message));
        await srv.close();
    });
});

// ---- ScacelithClient ------------------------------------------------------------------------------

const GAME = 123456789012345;
const TOKEN = 'sct_' + 'k'.repeat(43);

// A scripted fake server: decodes client frames, checks seq, answers with the codec.
async function fakeServer(script = {}) {
    const sessions = [];
    const sessionWaiters = [];
    const attach = (peer) => {
        const s = { peer, received: [], lastSeq: 0, badSeq: 0, send: (name, f) => peer.send(P.encode[name](f)) };
        sessions.push(s);
        for (const w of sessionWaiters.splice(0)) w(s);
        peer.onFrame = (f) => {
            if (f.op !== 2) return;
            const m = P.decode(f.payload, P.DECODE_C2S);
            if (m.seq !== s.lastSeq + 1) s.badSeq++;
            s.lastSeq = m.seq;
            s.received.push(m);
            if (m.type === P.MSG.Hello) {
                if (script.hello) script.hello(s, m);
                else if (m.token !== TOKEN) { s.send('Error', { ref: m.seq, code: P.enums.ErrorCode.Unauthorized, fatal: true, game: 0 }); peer.close(4003, 'unauthorized'); }
                else s.send('Welcome', { proto: 1, serverTime: Date.now(), userId: 17, username: 'alice', serverName: 'Fake', heartbeatMs: 10000, clientPingMs: 10000, maxMsgPerSec: 20, activeGame: 0 });
            } else if (m.type === P.MSG.C_Ping) {
                s.send('S_Pong', { nonce: m.nonce, serverTime: Date.now() + (script.clockAhead ?? 0) });
            } else if (script.onMessage) script.onMessage(s, m);
        };
    };
    const srv = await startWsServer({ onPeer: attach });
    const waitSession = () => {
        const n = sessions.length;
        return new Promise((resolve) => sessionWaiters.push(() => resolve(sessions[n])));
    };
    return { srv, sessions, waitSession, port: srv.port, close: () => srv.close() };
}

async function connected(script, clientOpts = {}) {
    const fs_ = await fakeServer(script);
    const sp = fs_.waitSession();
    const c = new ScacelithClient(clientOpts);
    const welcome = await c.connect({ port: fs_.port, insecure: true, token: TOKEN, clientName: 'sdk-test' });
    const s = await sp;
    return { fake: fs_, c, s, welcome };
}

describe('ScacelithClient', () => {
    test('connect: Hello with seq 1, protocol, schema hash, client name and token; resolves with Welcome', async () => {
        const { fake, c, s, welcome } = await connected();
        const hello = s.received[0];
        assert.equal(hello.type, P.MSG.Hello);
        assert.equal(hello.seq, 1);
        assert.equal(hello.proto, P.PROTOCOL_VERSION);
        assert.equal(hello.schema, P.SCHEMA_HASH);
        assert.equal(hello.client, 'sdk-test');
        assert.equal(hello.token, TOKEN);
        assert.equal(welcome.type, P.MSG.Welcome);
        assert.equal(c.state, 'ready');
        assert.equal(c.userId, 17);
        assert.equal(c.username, 'alice');
        assert.equal(c.serverName, 'Fake');
        assert.ok(Math.abs(c.clockOffsetMs) < 1000);
        await c.close();
        assert.equal(c.state, 'closed');
        await fake.close();
    });

    test('refused Hello: rejects with the error code and the close code', async () => {
        const fake = await fakeServer();
        const c = new ScacelithClient();
        await assert.rejects(c.connect({ port: fake.port, insecure: true, token: 'sct_wrongwrongwrong' }), (e) => e instanceof ScacelithError
            && e.errorCode === P.enums.ErrorCode.Unauthorized && e.errorName === 'Unauthorized' && e.fatal && e.closeCode === 4003);
        assert.equal(c.state, 'closed');
        // Closed without an Error message.
        const fake2 = await fakeServer({ hello: (s) => s.peer.close(4010, 'hello timeout') });
        await assert.rejects(new ScacelithClient().connect({ port: fake2.port, insecure: true, token: TOKEN }), (e) => e.closeCode === 4010 && e.errorCode === 0);
        // No answer at all.
        const fake3 = await fakeServer({ hello: () => {} });
        await assert.rejects(new ScacelithClient().connect({ port: fake3.port, insecure: true, token: TOKEN, timeoutMs: 300 }), /no Welcome in time/);
        // Upgrade refused.
        const refused = await startWsServer({ handshake: () => ({ status: 429 }) });
        await assert.rejects(new ScacelithClient().connect({ port: refused.port, insecure: true, token: TOKEN }), (e) => e.status === 429);
        // A Hello that cannot be encoded fails before connecting.
        await assert.rejects(new ScacelithClient().connect({ port: fake.port, insecure: true, token: '' }), /invalid Hello \(token bad length\)/);
        for (const f of [fake, fake2, fake3, refused]) await f.close();
    });

    test('server Ping is answered with Pong at once; seq numbers every client message', async () => {
        const { fake, c, s } = await connected();
        const pinged = c.waitFor('Ping');
        s.send('S_Ping', { nonce: 77, serverTime: Date.now() });
        assert.equal((await pinged).nonce, 77);
        await s.peer.until(() => s.received.some((m) => m.type === P.MSG.C_Pong));
        assert.deepEqual(s.received[1], { type: P.MSG.C_Pong, seq: 2, nonce: 77 });

        const seqs = [
            c.joinQueue('3+2', true), c.leaveQueue(), c.challenge('bob', 180, 2, true, P.enums.ColorPref.White),
            c.challenge({ target: '', baseSec: 600, incSec: 0 }), c.createPrivateGame(300, 5), c.acceptChallenge(5), c.declineChallenge(6),
            c.cancelChallenge(7), c.joinCode('AB12-CD'), c.move(GAME, 0, 1804, 0xdeadbeef, 1500, true), c.resign(GAME), c.offerDraw(GAME),
            c.answerDraw(GAME, false), c.claimDraw(GAME), c.abort(GAME), c.resync(GAME), c.rematch(GAME), c.rematch(GAME, false),
            c.send('QueueJoin', { category: '5+0', rated: false }),
        ];
        assert.deepEqual(seqs, Array.from({ length: seqs.length }, (_, i) => i + 3));
        await s.peer.until(() => s.received.length === 2 + seqs.length);
        assert.equal(s.badSeq, 0);
        const got = s.received.slice(2).map(({ type, seq, ...f }) => [P.messageName(type), f]);
        assert.deepEqual(got, [
            ['QueueJoin', { category: '3+2', rated: true }], ['QueueLeave', {}],
            ['ChallengeCreate', { target: 'bob', baseSec: 180, incSec: 2, rated: true, color: 1 }],
            ['ChallengeCreate', { target: '', baseSec: 600, incSec: 0, rated: false, color: 0 }],
            ['ChallengeCreate', { target: '', baseSec: 300, incSec: 5, rated: false, color: 0 }],
            ['ChallengeAccept', { id: 5 }], ['ChallengeDecline', { id: 6 }], ['ChallengeCancel', { id: 7 }], ['ChallengeJoinCode', { code: 'AB12-CD' }],
            ['Move', { game: GAME, ply: 0, move: 1804, posHash: 0xdeadbeef, thinkMs: 1500, drawOffer: true }],
            ['Resign', { game: GAME }], ['DrawOffer', { game: GAME }], ['DrawAnswer', { game: GAME, accept: false }], ['DrawClaim', { game: GAME }],
            ['Abort', { game: GAME }], ['Resync', { game: GAME }], ['Rematch', { game: GAME, accept: true }], ['Rematch', { game: GAME, accept: false }],
            ['QueueJoin', { category: '5+0', rated: false }],
        ]);
        // sendRaw does not consume a seq.
        const before = c.seq;
        c.sendRaw(Buffer.from([0x11, 0, 0, 0, 0]));
        await s.peer.until(() => s.peer.messages.length === 2 + seqs.length + 1);
        assert.deepEqual([...s.peer.messages.at(-1).data], [0x11, 0, 0, 0, 0]);
        assert.equal(c.seq, before);
        await c.close();
        await fake.close();
    });

    test('ping() measures the round trip and the server clock offset', async () => {
        const { fake, c } = await connected({ clockAhead: 5000 });
        const r = await c.ping();
        assert.ok(r.rttMs >= 0 && r.rttMs < 1000);
        assert.ok(Math.abs(r.offsetMs - 5000) < 200, String(r.offsetMs));
        assert.equal(c.rttMs, r.rttMs);
        assert.ok(Math.abs(c.serverNow() - Date.now() - 5000) < 200);
        await c.close();
        await fake.close();
    });

    test('game view: snapshot, moves, duplicates, events, end, ratings', async () => {
        const { fake, c, s } = await connected();
        const player = (userId, name) => ({ userId, name, rating: 1500, provisional: false });
        s.send('GameSnapshot', {
            game: GAME, gseq: 2, category: '3+2', baseMs: 180000, incMs: 2000, rated: true, white: player(17, 'alice'), black: player(18, 'bob'), you: 0,
            moves: [{ move: 1804, spentMs: 0, clockMs: 180000 }, { move: P.uciToMove('e7e5'), spentMs: 0, clockMs: 180000 }],
            running: 0, whiteMs: 180000, blackMs: 180000, serverTime: Date.now(), drawOffer: 2, status: 0, reason: 0,
            whiteConnected: true, blackConnected: true, graceMs: 18000, firstMoveMs: 0, startedAt: Date.now(), rematch: 2,
        });
        const snap = await c.waitFor('GameSnapshot');
        assert.equal(snap.game, GAME);
        assert.equal(c.currentGame, GAME);
        const mm = { game: GAME, gseq: 3, ply: 2, move: P.uciToMove('g1f3'), flags: 0, spentMs: 2500, whiteMs: 179500, blackMs: 180000, serverTime: Date.now(), drawOffer: true, firstMoveMs: 0 };
        const made = c.waitFor('MoveMade', (m) => m.ply === 2);
        s.send('MoveMade', mm);
        await made;
        s.send('MoveMade', mm);    // idempotent resend
        s.send('GameEvent', { game: GAME, gseq: 4, kind: P.enums.GameEventKind.PlayerDisconnected, color: 1, arg: 18000 });
        s.send('MoveRejected', { game: GAME, ply: 5, move: 1, code: P.enums.ErrorCode.Desync });
        const g = c.games.get(GAME);
        await c.waitFor('MoveRejected');
        assert.equal(g.moves.length, 3);
        assert.equal(g.ply, 3);
        assert.deepEqual(g.moves[2], { move: mm.move, spentMs: 2500, clockMs: 179500 });
        assert.equal(g.drawOffer, 0);           // offered by White with the move
        assert.equal(g.running, 1);             // Black to move
        assert.equal(g.blackConnected, false);
        assert.equal(g.graceMs, 18000);
        assert.equal(g.lastRejected.code, P.enums.ErrorCode.Desync);
        assert.equal(g.gseq, 4);
        s.send('MoveMade', { ...mm, ply: 7, gseq: 5 });    // a gap: marked for resync
        s.send('GameEnd', { game: GAME, gseq: 6, status: 1, reason: 2, whiteMs: 170000, blackMs: 160000, serverTime: Date.now() });
        s.send('RatingUpdate', { game: GAME, category: '3+2', white: { before: 1500, after: 1516, games: 1, provisional: true }, black: { before: 1500, after: 1484, games: 1, provisional: true } });
        await c.waitFor('RatingUpdate');
        assert.equal(g.desync, true);
        assert.equal(g.status, 1);
        assert.equal(g.reason, 2);
        assert.equal(g.running, 2);
        assert.equal(g.ratings.white.after, 1516);
        await c.close();
        await fake.close();
    });

    test('waitFor: predicate, timeout, history with since, close', async () => {
        const { fake, c, s } = await connected();
        const mark = c.mark();
        s.send('Ack', { ref: 1 });
        s.send('Ack', { ref: 2 });
        await c.waitFor('Ack', (m) => m.ref === 2);
        // Already received, found through the history.
        assert.equal((await c.waitFor('Ack', (m) => m.ref === 1, 100, { since: mark })).ref, 1);
        await assert.rejects(c.waitFor('Ack', (m) => m.ref === 3, 50), /timeout waiting for Ack/);
        const seen = [];
        c.on('message', (m) => seen.push(m.type));
        c.on('Notice', (m) => seen.push('notice:' + m.code));
        s.send('Notice', { code: 1, arg: 5000 });
        await c.waitFor('Notice');
        assert.deepEqual(seen, ['notice:1', P.MSG.Notice]);
        const pending = c.waitFor('GameEnd', null, 5000);
        const closing = c.waitFor('close');
        s.peer.close(4008, 'shutting down');
        await assert.rejects(pending, (e) => e.closeCode === 4008);
        assert.deepEqual(await closing, { code: 4008, reason: 'shutting down' });
        assert.deepEqual(await c.waitFor('close'), { code: 4008, reason: 'shutting down' });
        await assert.rejects(c.waitFor('Ack'), /not connected/);
        await fake.close();
    });

    test('expectAck resolves on Ack{ref} and rejects on Error{ref}', async () => {
        const { fake, c } = await connected({
            onMessage: (s, m) => {
                if (m.type === P.MSG.QueueLeave) s.send('Ack', { ref: m.seq });
                if (m.type === P.MSG.QueueJoin) s.send('Error', { ref: m.seq, code: P.enums.ErrorCode.InvalidCategory, fatal: false, game: 0 });
            },
        });
        assert.equal((await c.expectAck(c.leaveQueue())).type, P.MSG.Ack);
        await assert.rejects(c.expectAck(c.joinQueue('9+9', true)), (e) => e.errorName === 'InvalidCategory' && e.errorCode === 107);
        assert.equal(c.lastError.code, 107);
        await assert.rejects(c.expectAck(c.resync(1), 50), /no answer/);
        await c.close();
        await fake.close();
    });

    test('a malformed or text server message closes the connection (1002 / 1003)', async () => {
        for (const [bad, code] of [[(p) => p.send(Buffer.from([0x80, 1])), 1002], [(p) => p.send(P.encode.Move({ seq: 1, game: 1, ply: 0, move: 1, posHash: 1, thinkMs: 1, drawOffer: false })), 1002], [(p) => p.sendText('hello'), 1003]]) {
            const { fake, c, s } = await connected();
            const errors = [];
            c.on('protocolError', (e) => errors.push(e));
            const closed = c.waitFor('close');
            bad(s.peer);
            await closed;
            assert.equal(errors.length, 1);
            await s.peer.until(() => s.peer.closeFrame);
            assert.equal(s.peer.closeFrame.code, code);
            await fake.close();
        }
    });

    test('light mode: no history, no game tracking, manual Pong', async () => {
        const { fake, c, s } = await connected(undefined, { history: 0, trackGames: false, autoPong: false });
        s.send('S_Ping', { nonce: 1, serverTime: 0 });
        s.send('Ack', { ref: 9 });
        await c.waitFor('Ack');
        assert.equal(s.received.length, 1);
        assert.equal(c._hist, null);
        assert.equal(c.games.size, 0);
        await c.close();
        await fake.close();
    });
});
