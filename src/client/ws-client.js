// Minimal RFC 6455 WebSocket client over node:tls (node:net with `insecure: true`), no dependency.
// Used by the Node SDK (client.js), the integration tests and the load generator, which opens
// tens of thousands of connections from a few processes: per connection it keeps a handful of
// fields, no timer while open (the handshake and the closing handshake use the socket's own idle
// timeout), and mask keys come from a shared random pool.
//
//   const ws = await WsClient.connect({ host, port, ca, subprotocol: 'scacelith.v1' });
//   ws.on('message', (data, isBinary) => ...);   // data: Buffer (a view: copy it to keep it)
//   ws.on('close', (code, reason) => ...);
//   ws.send(buffer);  ws.close(1000, 'bye');
//
// Events: 'open', 'message' (data, isBinary), 'ping' (payload), 'pong' (payload),
// 'close' (code, reason) exactly once, 'error' (err). Without an 'error' listener errors are
// silent (the 'close' event still reports them: 1006 = connection lost, 1002/1007/1009 = the
// server broke the protocol and this client failed the connection).

import net from 'node:net';
import tls from 'node:tls';
import crypto from 'node:crypto';
import { isUtf8 } from 'node:buffer';

export const CONNECTING = 0;
export const OPEN = 1;
export const CLOSING = 2;
export const CLOSED = 3;

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';
const MAX_HEADER_BYTES = 16384;
const EMPTY = Buffer.alloc(0);

// Mask keys: 4 bytes per frame from a pool refilled 1024 frames at a time.
let maskPool = EMPTY;
let maskPos = 0;
function writeMask(frame, at) {
    if (maskPos + 4 > maskPool.length) { maskPool = crypto.randomBytes(4096); maskPos = 0; }
    frame[at] = maskPool[maskPos];
    frame[at + 1] = maskPool[maskPos + 1];
    frame[at + 2] = maskPool[maskPos + 2];
    frame[at + 3] = maskPool[maskPos + 3];
    maskPos += 4;
}

/**
 * Builds one masked client frame.
 * @param {number} opcode 0 continuation, 1 text, 2 binary, 8 close, 9 ping, 10 pong
 * @param {Uint8Array} payload
 * @param {boolean} [fin]
 * @returns {Buffer}
 */
export function buildFrame(opcode, payload, fin = true) {
    const len = payload.length;
    const hdr = len < 126 ? 2 : len < 65536 ? 4 : 10;
    const frame = Buffer.allocUnsafe(hdr + 4 + len);
    frame[0] = (fin ? 0x80 : 0) | opcode;
    if (len < 126) frame[1] = 0x80 | len;
    else if (len < 65536) { frame[1] = 0x80 | 126; frame[2] = len >>> 8; frame[3] = len & 0xff; }
    else { frame[1] = 0x80 | 127; frame.writeUInt32BE(Math.floor(len / 4294967296), 2); frame.writeUInt32BE(len >>> 0, 6); }
    writeMask(frame, hdr);
    const m = hdr, p = hdr + 4;
    for (let i = 0; i < len; i++) frame[p + i] = payload[i] ^ frame[m + (i & 3)];
    return frame;
}

/** Close codes a peer may send (RFC 6455 section 7.4 plus the registered 1012-1014). */
export function isValidCloseCode(code) {
    return (code >= 1000 && code <= 1003) || (code >= 1007 && code <= 1014) || (code >= 3000 && code <= 4999);
}

function handshakeError(message, extra = {}) {
    const e = new Error(`websocket: ${message}`);
    e.code = 'WS_HANDSHAKE';
    Object.assign(e, extra);
    return e;
}

/** A WebSocket client connection. */
export class WsClient {
    /**
     * @param {object} options
     * @param {string} [options.host] default '127.0.0.1'
     * @param {number} [options.port] default 443 (80 with insecure)
     * @param {string} [options.path] default '/ws'
     * @param {string|Buffer|Array} [options.ca] trusted CA certificate(s) (PEM)
     * @param {boolean} [options.rejectUnauthorized] default true
     * @param {string} [options.servername] TLS SNI / certificate name (default: host unless an IP)
     * @param {object} [options.headers] extra request headers
     * @param {boolean} [options.insecure] plain TCP (TLS_MODE=off development servers)
     * @param {string|null} [options.subprotocol] requested and required subprotocol (null: none)
     * @param {number} [options.maxMessageBytes] largest incoming message (default 1 MiB), else close 1009
     * @param {number} [options.handshakeTimeoutMs] default 10000
     * @param {number} [options.closeTimeoutMs] wait for the peer's close before dropping the socket (default 3000)
     * @param {string} [options.localAddress] source address (load generators spread over 127.0.0.x)
     * @param {object} [options.tls] extra node:tls connect options
     */
    constructor(options = {}) {
        this.options = options;
        this.readyState = CLOSED;
        this.protocol = '';
        this.socket = null;
        this.closeCode = 0;
        this.closeReason = '';
        this.headers = null;      // response headers of the upgrade
        this._ls = null;          // { event: [fn] }, created on first on()
        this._buf = null;         // unparsed bytes
        this._frags = null;       // fragments of the message being received
        this._fragLen = 0;
        this._fragOp = 0;
        this._closeSent = false;
        this._closeRecv = false;
        this._pending = null;     // { resolve, reject } of connect()
    }

    /** Connects and resolves with the open client. */
    static connect(options) { return new WsClient(options).connect(); }

    get bufferedAmount() { return this.socket ? this.socket.writableLength : 0; }

    on(event, fn) {
        const ls = this._ls || (this._ls = Object.create(null));
        (ls[event] || (ls[event] = [])).push(fn);
        return this;
    }

    off(event, fn) {
        const a = this._ls && this._ls[event];
        if (a) { const i = a.indexOf(fn); if (i >= 0) a.splice(i, 1); }
        return this;
    }

    once(event, fn) {
        const w = (...args) => { this.off(event, w); fn(...args); };
        return this.on(event, w);
    }

    _emit(event, a, b) {
        const ls = this._ls && this._ls[event];
        if (!ls) return;
        if (ls.length === 1) { ls[0](a, b); return; }
        for (const fn of ls.slice()) fn(a, b);
    }

    /** Opens the connection: TCP/TLS, HTTP upgrade, checks. Resolves with this client. */
    connect() {
        if (this.readyState !== CLOSED || this.socket) return Promise.reject(new Error('websocket: already connected'));
        const o = this.options;
        const host = o.host ?? '127.0.0.1';
        const port = o.port ?? (o.insecure ? 80 : 443);
        const path = o.path ?? '/ws';
        const subprotocol = o.subprotocol === undefined ? 'scacelith.v1' : o.subprotocol;
        const key = crypto.randomBytes(16).toString('base64');
        const lines = [`GET ${path} HTTP/1.1`, `Host: ${net.isIPv6(host) ? `[${host}]` : host}:${port}`, 'Upgrade: websocket', 'Connection: Upgrade',
            `Sec-WebSocket-Key: ${key}`, 'Sec-WebSocket-Version: 13'];
        if (subprotocol) lines.push(`Sec-WebSocket-Protocol: ${subprotocol}`);
        for (const [k, v] of Object.entries(o.headers || {})) {
            if (/[\r\n:]/.test(k) || /[\r\n]/.test(String(v))) return Promise.reject(new Error(`websocket: invalid header ${k}`));
            lines.push(`${k}: ${v}`);
        }
        const request = lines.join('\r\n') + '\r\n\r\n';
        this.readyState = CONNECTING;
        this.closeCode = 0;
        this.closeReason = '';
        this._closeSent = this._closeRecv = false;
        this._buf = this._frags = null;

        return new Promise((resolve, reject) => {
            this._pending = { resolve, reject };
            const base = { host, port };
            if (o.localAddress) base.localAddress = o.localAddress;
            let socket;
            if (o.insecure) socket = net.connect(base);
            else {
                const t = { ...base, ...(o.tls || {}), rejectUnauthorized: o.rejectUnauthorized !== false };
                if (o.ca) t.ca = o.ca;
                const sn = o.servername ?? (net.isIP(host) ? undefined : host);
                if (sn) t.servername = sn;
                socket = tls.connect(t);
            }
            this.socket = socket;
            let head = null;
            const onHead = (chunk) => {
                head = head ? Buffer.concat([head, chunk]) : chunk;
                const end = head.indexOf('\r\n\r\n');
                if (end < 0) {
                    if (head.length > MAX_HEADER_BYTES) this._abortHandshake(handshakeError('response headers too large'));
                    return;
                }
                socket.off('data', onHead);
                const rest = head.subarray(end + 4);
                const err = this._checkResponse(head.subarray(0, end).toString('latin1'), key, subprotocol, rest);
                head = null;
                if (err) { this._abortHandshake(err); return; }
                socket.setTimeout(0);
                socket.off('timeout', onTimeout);
                socket.setNoDelay(true);
                socket.on('data', (c) => this._onData(c));
                this.readyState = OPEN;
                this._pending = null;
                resolve(this);
                this._emit('open');
                if (rest.length) this._onData(rest);
            };
            socket.on('data', onHead);
            socket.on('error', (e) => {
                if (this._pending) { this._pending.reject(e); this._pending = null; }
                this._emit('error', e);
            });
            socket.on('close', () => this._onSocketClose());
            const onTimeout = () => this._abortHandshake(handshakeError('handshake timeout'));
            socket.setTimeout(o.handshakeTimeoutMs ?? 10000);
            socket.on('timeout', onTimeout);
            socket.write(request);
        });
    }

    _checkResponse(text, key, subprotocol, rest) {
        const [statusLine, ...headerLines] = text.split('\r\n');
        const m = /^HTTP\/1\.1 (\d{3})(?: (.*))?$/.exec(statusLine);
        if (!m) return handshakeError(`bad status line ${JSON.stringify(statusLine.slice(0, 80))}`);
        const headers = {};
        for (const l of headerLines) {
            const i = l.indexOf(':');
            if (i <= 0) return handshakeError('bad header line');
            const k = l.slice(0, i).trim().toLowerCase(), v = l.slice(i + 1).trim();
            headers[k] = headers[k] !== undefined ? `${headers[k]}, ${v}` : v;
        }
        const status = Number(m[1]);
        if (status !== 101) {
            return handshakeError(`upgrade refused: HTTP ${status}${m[2] ? ' ' + m[2] : ''}`, { status, headers, body: rest.subarray(0, 4096).toString('utf8') });
        }
        if ((headers.upgrade || '').toLowerCase() !== 'websocket') return handshakeError('missing Upgrade: websocket', { status, headers });
        if (!(headers.connection || '').toLowerCase().split(/\s*,\s*/).includes('upgrade')) return handshakeError('missing Connection: Upgrade', { status, headers });
        const expected = crypto.createHash('sha1').update(key + GUID).digest('base64');
        if (headers['sec-websocket-accept'] !== expected) return handshakeError('bad Sec-WebSocket-Accept', { status, headers });
        if (headers['sec-websocket-extensions']) return handshakeError('unexpected extension', { status, headers });
        const chosen = headers['sec-websocket-protocol'];
        if (subprotocol) {
            if (chosen !== subprotocol) return handshakeError(`subprotocol ${JSON.stringify(chosen ?? null)} instead of ${subprotocol}`, { status, headers });
        } else if (chosen) return handshakeError(`unrequested subprotocol ${chosen}`, { status, headers });
        this.protocol = chosen || '';
        this.headers = headers;
        return null;
    }

    _abortHandshake(err) {
        if (this._pending) { this._pending.reject(err); this._pending = null; }
        this.closeCode = 1006;
        if (this.socket) this.socket.destroy();
    }

    // ---- receiving ----

    _onData(chunk) {
        if (this._closeRecv) return;
        let buf = this._buf ? Buffer.concat([this._buf, chunk]) : chunk;
        this._buf = null;
        const max = this.options.maxMessageBytes ?? 1048576;
        let off = 0;
        while (buf.length - off >= 2) {
            const b0 = buf[off], b1 = buf[off + 1];
            const fin = (b0 & 0x80) !== 0, op = b0 & 0x0f;
            if (b0 & 0x70) return this._fail(1002, 'reserved bits set');
            if (b1 & 0x80) return this._fail(1002, 'masked frame from the server');
            let len = b1 & 0x7f, hdr = 2;
            if (len === 126) {
                if (buf.length - off < 4) break;
                len = (buf[off + 2] << 8) | buf[off + 3];
                hdr = 4;
                if (len < 126) return this._fail(1002, 'non-minimal length');
            } else if (len === 127) {
                if (buf.length - off < 10) break;
                const hi = buf.readUInt32BE(off + 2), lo = buf.readUInt32BE(off + 6);
                if (hi > 0x1fffff) return this._fail(1009, 'frame too big');
                len = hi * 4294967296 + lo;
                hdr = 10;
                if (len < 65536) return this._fail(1002, 'non-minimal length');
            }
            if (op >= 8) {
                if (op > 10) return this._fail(1002, `unknown opcode ${op}`);
                if (!fin) return this._fail(1002, 'fragmented control frame');
                if (len > 125) return this._fail(1002, 'control frame too long');
            } else {
                if (op > 2) return this._fail(1002, `unknown opcode ${op}`);
                if (op === 0 && !this._frags) return this._fail(1002, 'continuation without a message');
                if (op !== 0 && this._frags) return this._fail(1002, 'new message inside a fragmented one');
                if ((op === 0 ? this._fragLen : 0) + len > max) return this._fail(1009, 'message too big');
            }
            if (buf.length - off < hdr + len) break;
            const payload = buf.subarray(off + hdr, off + hdr + len);
            off += hdr + len;
            if (op >= 8) {
                this._onControl(op, payload);
                if (this._closeRecv) return;
            } else if (op === 0) {
                this._frags.push(payload);
                this._fragLen += len;
                if (fin) {
                    const data = Buffer.concat(this._frags, this._fragLen);
                    const isBinary = this._fragOp === 2;
                    this._frags = null;
                    this._fragLen = 0;
                    if (!this._deliver(data, isBinary)) return;
                }
            } else if (!fin) {
                this._frags = [payload];
                this._fragLen = len;
                this._fragOp = op;
            } else if (!this._deliver(payload, op === 2)) return;
            if (this.readyState === CLOSED) return;
        }
        if (off < buf.length) this._buf = buf.subarray(off);
    }

    _deliver(data, isBinary) {
        if (!isBinary && !isUtf8(data)) { this._fail(1007, 'text message is not UTF-8'); return false; }
        if (this.readyState === OPEN || this.readyState === CLOSING) this._emit('message', data, isBinary);
        return true;
    }

    _onControl(op, payload) {
        if (op === 9) {
            if (this.readyState === OPEN) this.socket.write(buildFrame(10, payload));
            this._emit('ping', payload);
        } else if (op === 10) {
            this._emit('pong', payload);
        } else {
            let code = 1005, reason = '';
            if (payload.length === 1) return this._fail(1002, 'close frame of one byte');
            if (payload.length >= 2) {
                code = (payload[0] << 8) | payload[1];
                if (!isValidCloseCode(code)) return this._fail(1002, `invalid close code ${code}`);
                const r = payload.subarray(2);
                if (!isUtf8(r)) return this._fail(1007, 'close reason is not UTF-8');
                reason = r.toString('utf8');
            }
            this._closeRecv = true;
            this.closeCode = code;
            this.closeReason = reason;
            if (!this._closeSent) {
                this._closeSent = true;
                this.socket.write(buildFrame(8, code === 1005 ? EMPTY : payload.subarray(0, 2)));
            }
            this.readyState = CLOSING;
            this.socket.end();
            this._armCloseTimer();
        }
    }

    // Fails the connection: close frame with the code, then drop the socket.
    _fail(code, reason) {
        this._buf = null;
        this._frags = null;
        const err = new Error(`websocket: ${reason}`);
        err.code = 'WS_PROTOCOL';
        err.closeCode = code;
        this._emit('error', err);
        if (this.readyState === OPEN || this.readyState === CLOSING) {
            if (!this._closeSent) {
                this._closeSent = true;
                this.socket.write(buildFrame(8, closePayload(code, reason)));
            }
            this.closeCode = code;
            this.closeReason = reason;
            this._closeRecv = true;
            this.readyState = CLOSING;
            this.socket.end();
            this._armCloseTimer();
        }
    }

    _armCloseTimer() {
        const s = this.socket;
        if (s) s.setTimeout(this.options.closeTimeoutMs ?? 3000, () => s.destroy());
    }

    _onSocketClose() {
        const wasOpen = this.readyState === OPEN || this.readyState === CLOSING;
        this.readyState = CLOSED;
        this.socket = null;
        this._buf = this._frags = null;
        if (!this._closeRecv) { this.closeCode = 1006; this.closeReason = ''; }
        if (this._pending) {
            this._pending.reject(handshakeError('connection closed during the handshake'));
            this._pending = null;
        }
        // 'close' reports the end of an open connection; a failed connect() only rejects.
        if (wasOpen) this._emit('close', this.closeCode, this.closeReason);
    }

    // ---- sending ----

    /**
     * Sends one message: binary for a Buffer / Uint8Array, text for a string.
     * @returns {boolean} false when not open (dropped) or when the socket buffer is full (queued)
     */
    send(data) {
        if (this.readyState !== OPEN) {
            if (this.readyState === CONNECTING) throw new Error('websocket: not open yet');
            return false;
        }
        if (typeof data === 'string') return this.socket.write(buildFrame(1, Buffer.from(data, 'utf8')));
        return this.socket.write(buildFrame(2, data));
    }

    /** Sends a raw, already framed chunk (protocol-abuse tests). */
    sendFrameBytes(bytes) {
        if (this.readyState !== OPEN) return false;
        return this.socket.write(bytes);
    }

    ping(payload = EMPTY) {
        if (this.readyState !== OPEN) return false;
        return this.socket.write(buildFrame(9, payload));
    }

    /**
     * Starts the closing handshake. Resolves once the connection is closed.
     * @param {number} [code] a valid close code (1000-1003, 1007-1014, 3000-4999)
     * @param {string} [reason] at most 123 bytes
     */
    close(code = 1000, reason = '') {
        if (!isValidCloseCode(code)) throw new RangeError(`websocket: close code ${code}`);
        if (this.readyState === CONNECTING) {
            // A connection that never opened emits no 'close': wait for the socket's own (after
            // _onSocketClose, its first listener).
            const s = this.socket;
            this._abortHandshake(handshakeError('closed by the client'));
            return s ? new Promise((resolve) => s.once('close', () => resolve())) : Promise.resolve();
        }
        const done = new Promise((resolve) => {
            if (this.readyState === CLOSED) { resolve(); return; }
            this.once('close', () => resolve());
        });
        if (this.readyState === OPEN) {
            this._closeSent = true;
            this.readyState = CLOSING;
            this.socket.write(buildFrame(8, closePayload(code, reason)));
            this._armCloseTimer();
        }
        return done;
    }

    /** Drops the connection without a closing handshake. */
    terminate() { if (this.socket) this.socket.destroy(); }
}

function closePayload(code, reason) {
    let r = Buffer.from(String(reason), 'utf8');
    if (r.length > 123) {
        let n = 123;
        while (n > 0 && (r[n] & 0xc0) === 0x80) n--;      // do not split a UTF-8 sequence
        r = r.subarray(0, n);
    }
    const p = Buffer.allocUnsafe(2 + r.length);
    p[0] = code >>> 8;
    p[1] = code & 0xff;
    r.copy(p, 2);
    return p;
}
