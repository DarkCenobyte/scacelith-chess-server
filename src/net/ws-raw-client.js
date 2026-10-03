// Minimal RFC 6455 client for the unit tests (not used by the server). It can also
// send deliberately broken frames (unmasked, RSV bits, text, fragmented, oversized...) to exercise
// the server's parser.

import crypto from 'node:crypto';
import net from 'node:net';
import tls from 'node:tls';

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

/**
 * Encodes one client frame.
 * @param {number} opcode
 * @param {Buffer} payload
 * @param {{fin?: boolean, mask?: boolean, rsv?: number, maskKey?: Buffer, fakeLength?: number}} [o]
 */
export function encodeClientFrame(opcode, payload, { fin = true, mask = true, rsv = 0, maskKey = null, fakeLength } = {}) {
    const len = fakeLength ?? payload.length;
    const hl = (len < 126 ? 2 : len < 65536 ? 4 : 10) + (mask ? 4 : 0);
    const out = Buffer.allocUnsafe(hl + payload.length);
    out[0] = (fin ? 0x80 : 0) | ((rsv & 7) << 4) | (opcode & 0x0f);
    let o = 2;
    if (len < 126) out[1] = len;
    else if (len < 65536) { out[1] = 126; out.writeUInt16BE(len, 2); o = 4; }
    else { out[1] = 127; out.writeUInt32BE(Math.floor(len / 0x100000000), 2); out.writeUInt32BE(len >>> 0, 6); o = 10; }
    if (mask) {
        out[1] |= 0x80;
        const k = maskKey || crypto.randomBytes(4);
        k.copy(out, o);
        o += 4;
        for (let i = 0; i < payload.length; i++) out[o + i] = payload[i] ^ k[i & 3];
    } else {
        payload.copy(out, o);
    }
    return out;
}

/** A client connection after a successful handshake. */
export class RawWsClient {
    constructor(socket, head) {
        this.socket = socket;
        this.messages = [];
        this._waiters = [];
        this._buf = Buffer.alloc(0);
        this._frag = null;
        this.closeCode = 0;
        this.closeReason = '';
        this.pongs = [];
        this.onMessage = null;        // when set, messages are not queued
        this.closed = new Promise((resolve) => { this._resolveClosed = resolve; });
        this._closeFrameSeen = false;
        socket.on('data', (c) => this._onData(c));
        socket.on('close', () => {
            if (!this._closeFrameSeen) this.closeCode = 1006;
            this._resolveClosed({ code: this.closeCode, reason: this.closeReason });
            for (const w of this._waiters.splice(0)) w.reject(new Error(`closed ${this.closeCode}`));
        });
        socket.on('error', () => {});
        if (head && head.length) this._onData(head);
    }

    _onData(chunk) {
        this._buf = this._buf.length ? Buffer.concat([this._buf, chunk]) : chunk;
        for (;;) {
            const b = this._buf;
            if (b.length < 2) return;
            let len = b[1] & 0x7f, o = 2;
            if (len === 126) { if (b.length < 4) return; len = b.readUInt16BE(2); o = 4; }
            else if (len === 127) { if (b.length < 10) return; len = b.readUInt32BE(2) * 0x100000000 + b.readUInt32BE(6); o = 10; }
            if (b[1] & 0x80) o += 4;       // servers never mask; tolerate for tests
            if (b.length < o + len) return;
            const fin = (b[0] & 0x80) !== 0, op = b[0] & 0x0f;
            const payload = Buffer.from(b.subarray(o, o + len));
            this._buf = b.subarray(o + len);
            this._frame(fin, op, payload);
        }
    }

    _frame(fin, op, payload) {
        if (op === 8) {
            this._closeFrameSeen = true;
            this.closeCode = payload.length >= 2 ? payload.readUInt16BE(0) : 1005;
            this.closeReason = payload.subarray(2).toString('utf8');
            if (!this.socket.writableEnded) {
                try { this.socket.write(encodeClientFrame(8, payload.subarray(0, 2))); } catch { /* ignore */ }
                this.socket.end();
            }
            return;
        }
        if (op === 9) { this.socket.write(encodeClientFrame(10, payload)); return; }
        if (op === 10) { this.pongs.push(payload); return; }
        if (op === 0 || op === 1 || op === 2) {
            if (!fin) { this._frag = (this._frag || []).concat(payload); return; }
            const msg = this._frag ? Buffer.concat([...this._frag, payload]) : payload;
            this._frag = null;
            if (this.onMessage) { this.onMessage(msg); return; }
            const w = this._waiters.shift();
            if (w) { clearTimeout(w.timer); w.resolve(msg); } else this.messages.push(msg);
        }
    }

    /** Next message (queued or future). */
    next(timeoutMs = 2000) {
        if (this.messages.length) return Promise.resolve(this.messages.shift());
        return new Promise((resolve, reject) => {
            const w = { resolve, reject, timer: null };
            w.timer = setTimeout(() => {
                const i = this._waiters.indexOf(w);
                if (i >= 0) this._waiters.splice(i, 1);
                reject(new Error('timeout waiting for a message'));
            }, timeoutMs);
            this._waiters.push(w);
        });
    }

    /** Sends one binary message (masked). */
    send(buf) { return this.socket.write(encodeClientFrame(2, buf)); }
    /** Sends a frame with explicit options (see encodeClientFrame). */
    sendFrame(opcode, payload, opts) { return this.socket.write(encodeClientFrame(opcode, payload, opts)); }
    /** Writes raw bytes. */
    sendRaw(bytes) { return this.socket.write(bytes); }
    ping(payload = Buffer.alloc(0)) { return this.socket.write(encodeClientFrame(9, payload)); }
    close(code = 1000, reason = '') {
        const r = Buffer.from(reason);
        const p = Buffer.allocUnsafe(2 + r.length);
        p.writeUInt16BE(code, 0);
        r.copy(p, 2);
        this.socket.write(encodeClientFrame(8, p));
    }
    destroy() { this.socket.destroy(); }
}

/**
 * Opens a WebSocket connection. Resolves with a RawWsClient on 101; rejects with an Error
 * carrying {status, headers, body} otherwise.
 * @param {object} o
 * @param {string} [o.host]
 * @param {number} o.port
 * @param {string} [o.path]
 * @param {string[]|null} [o.protocols]
 * @param {object} [o.headers] extra/overriding request headers (value null removes one)
 * @param {object|null} [o.tls] tls.connect options (enables TLS)
 * @param {string} [o.method]
 * @param {string} [o.httpVersion]
 * @param {number} [o.timeoutMs]
 */
export function connectWs({ host = '127.0.0.1', port, path = '/ws', protocols = ['scacelith.v1'], headers = {}, tls: tlsOpts = null, method = 'GET', httpVersion = '1.1', timeoutMs = 5000 } = {}) {
    return new Promise((resolve, reject) => {
        const key = crypto.randomBytes(16).toString('base64');
        const socket = tlsOpts ? tls.connect({ host, port, servername: tlsOpts.servername ?? (net.isIP(host) ? undefined : host), ...tlsOpts }) : net.connect({ host, port });
        socket.setNoDelay(true);
        const timer = setTimeout(() => { socket.destroy(); reject(new Error('handshake timeout')); }, timeoutMs);
        const h = {
            Host: `${host}:${port}`, Upgrade: 'websocket', Connection: 'Upgrade', 'Sec-WebSocket-Key': key, 'Sec-WebSocket-Version': '13',
            ...(protocols ? { 'Sec-WebSocket-Protocol': protocols.join(', ') } : {}),
            ...headers,
        };
        let req = `${method} ${path} HTTP/${httpVersion}\r\n`;
        for (const [k, v] of Object.entries(h)) if (v !== null && v !== undefined) req += `${k}: ${v}\r\n`;
        req += '\r\n';
        socket.once(tlsOpts ? 'secureConnect' : 'connect', () => socket.write(req));
        let buf = Buffer.alloc(0);
        const onData = (chunk) => {
            buf = Buffer.concat([buf, chunk]);
            const end = buf.indexOf('\r\n\r\n');
            if (end < 0) return;
            const headText = buf.subarray(0, end).toString('latin1');
            const lines = headText.split('\r\n');
            const status = +lines[0].split(' ')[1];
            const hdrs = {};
            for (const l of lines.slice(1)) { const c = l.indexOf(':'); hdrs[l.slice(0, c).toLowerCase()] = l.slice(c + 1).trim(); }
            if (status !== 101) {
                const len = +(hdrs['content-length'] || 0);
                if (buf.length < end + 4 + len) return;
                socket.removeListener('data', onData);
                clearTimeout(timer);
                const body = buf.subarray(end + 4, end + 4 + len).toString('utf8');
                socket.destroy();
                const err = new Error(`HTTP ${status}`);
                err.status = status; err.headers = hdrs; err.body = body;
                reject(err);
                return;
            }
            socket.removeListener('data', onData);
            clearTimeout(timer);
            const expected = crypto.createHash('sha1').update(key + GUID).digest('base64');
            if (hdrs['sec-websocket-accept'] !== expected) { socket.destroy(); reject(new Error('bad Sec-WebSocket-Accept')); return; }
            const client = new RawWsClient(socket, buf.subarray(end + 4));
            client.protocol = hdrs['sec-websocket-protocol'];
            client.headers = hdrs;
            resolve(client);
        };
        socket.on('data', onData);
        socket.on('error', (e) => { clearTimeout(timer); reject(e); });
    });
}
