// RFC 6455 WebSocket server side, written for this protocol only: binary messages, no extension,
// small messages, many connections. What it deliberately does not do (and refuses):
//   - text frames (the protocol is binary: close 1003), extensions / permessage-deflate (RSV bits:
//     close 1002; the Sec-WebSocket-Extensions offer is ignored, so none is ever negotiated);
//   - unmasked client frames (1002), reserved opcodes (1002), fragmented or oversized control
//     frames (1002), non-minimal lengths (1002);
//   - messages above maxMessageBytes: the length is read from the frame header and checked before
//     a single payload byte is buffered (1009), fragments included (their sum is checked, and at
//     most MAX_FRAGMENTS pieces are accepted).
//
// Handshake: `handleUpgrade(req, socket, head)` serves Node's http 'upgrade' event;
// `handleSocket(socket)` takes a raw TCP/TLS socket and parses the HTTP upgrade request itself
// (a strict, bounded parser: 8 KB, 64 header lines, CRLF only, no obs-fold), which is what the
// dedicated WebSocket port uses: no http.Server per connection, faster accepts. With a `guard`
// (net/ipguard.js, the shard's protection per address), an upgrade request first takes its token
// of the address's request budget like any HTTP request, before any other check and before the
// admission IPC to the primary: a blocked address, or one over HTTP_RATE_PER_IP, gets HTTP 429
// rate_limited with Retry-After. The game honours Retry-After when /api/v1/info answers 429; on a
// refused upgrade it retries with its own backoff (reconnectDelayMs).
//
// Hot path (per message): no allocation for complete frames (the payload is unmasked in place and
// handed out as a view of the socket chunk); a frame split across TCP reads is appended to a
// private buffer that grows by doubling up to the frame size (linear copying; it holds at most
// twice the bytes received, or 64, and never more than the header size + maxMessageBytes).
// Outgoing messages are written as header + payload (the payload is never copied); writes made in
// the same tick are corked and leave in one writev. The bytes queued in user space for a client
// are watched (socket.writableLength: bytes the kernel's send buffer has not accepted yet; the
// kernel buffer comes on top): when a send would take them above sendBufferLimit, the connection
// is closed with 4303 (SlowConsumer).
//
// The payload given to onMessage is a view of the received bytes (the socket chunk, a private
// reassembly buffer, or a copy for fragmented messages). It stays valid after the call because
// those buffers are never reused, but keeping it retains the whole underlying buffer, so a
// consumer that stores data copies it.

import crypto from 'node:crypto';
import { performance } from 'node:perf_hooks';
import { now as clockNow } from '../game/clock.js';
import { metrics as defaultRegistry } from '../metrics.js';
import { CloseCode, WS_SUBPROTOCOL } from '../protocol/index.js';

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';
const KEY_RE = /^[A-Za-z0-9+/]{21}[AQgw]==$/;          // base64 of exactly 16 bytes
const TOKEN_RE = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;
const BAD_VALUE_RE = /[\x00-\x08\x0a-\x1f\x7f]/;
const REQUEST_LINE_RE = /^([A-Z]{1,16}) (\S{1,2048}) HTTP\/(\d)\.(\d)$/;

export const OP_CONT = 0, OP_TEXT = 1, OP_BINARY = 2, OP_CLOSE = 8, OP_PING = 9, OP_PONG = 10;
const MAX_FRAGMENTS = 64;
const MAX_HEADER_LINES = 64;
const utf8 = new TextDecoder('utf-8', { fatal: true });

const STATUS_TEXT = {
    400: 'Bad Request', 403: 'Forbidden', 404: 'Not Found', 405: 'Method Not Allowed', 408: 'Request Timeout',
    426: 'Upgrade Required', 429: 'Too Many Requests', 431: 'Request Header Fields Too Large', 503: 'Service Unavailable',
};

function noop() {}

/**
 * Close codes a peer may send (RFC 6455 7.4, IANA registry): 1000-1003, 1007-1014, 3000-4999.
 * @param {number} code
 */
export function isValidCloseCode(code) {
    return (code >= 1000 && code <= 1003) || (code >= 1007 && code <= 1014) || (code >= 3000 && code <= 4999);
}

/** Sec-WebSocket-Accept value for a key. */
export function acceptKey(key) {
    return crypto.createHash('sha1').update(key + GUID).digest('base64');
}

// Comma-separated header tokens, lower-cased.
function tokens(v) {
    if (!v) return [];
    return String(v).split(',').map((s) => s.trim().toLowerCase()).filter(Boolean);
}

// XOR-unmasks buf[start, start+len) in place with the 4-byte key at buf[maskOff].
function unmask(buf, maskOff, start, len) {
    const m0 = buf[maskOff], m1 = buf[maskOff + 1], m2 = buf[maskOff + 2], m3 = buf[maskOff + 3];
    const end = start + len, end4 = start + (len & ~3);
    let i = start;
    for (; i < end4; i += 4) {
        buf[i] ^= m0; buf[i + 1] ^= m1; buf[i + 2] ^= m2; buf[i + 3] ^= m3;
    }
    if (i < end) buf[i++] ^= m0;
    if (i < end) buf[i++] ^= m1;
    if (i < end) buf[i] ^= m2;
}

// Server frame header (FIN set, never masked).
function frameHeader(op, len) {
    let h;
    if (len < 126) { h = Buffer.allocUnsafe(2); h[1] = len; }
    else if (len < 65536) { h = Buffer.allocUnsafe(4); h[1] = 126; h.writeUInt16BE(len, 2); }
    else { h = Buffer.allocUnsafe(10); h[1] = 127; h.writeUInt32BE(Math.floor(len / 0x100000000), 2); h.writeUInt32BE(len >>> 0, 6); }
    h[0] = 0x80 | op;
    return h;
}

/**
 * Parses the head of an HTTP/1.x request (everything before the blank line, without it).
 * Duplicate headers are joined with ", " (as Node does for unknown headers, so a repeated
 * Sec-WebSocket-Key never validates). Returns null when the request is malformed.
 * @param {Buffer} buf
 */
export function parseRequestHead(buf) {
    const text = buf.toString('latin1');
    const lines = text.split('\r\n');
    if (lines.length > MAX_HEADER_LINES + 1) return null;
    const m = REQUEST_LINE_RE.exec(lines[0]);
    if (!m) return null;
    const headers = Object.create(null);
    for (let i = 1; i < lines.length; i++) {
        const line = lines[i];
        const c = line.indexOf(':');
        if (c <= 0) return null;
        const name = line.slice(0, c);
        if (!TOKEN_RE.test(name)) return null;             // also refuses obs-fold (leading space)
        const value = line.slice(c + 1).replace(/^[ \t]+|[ \t]+$/g, '');
        if (BAD_VALUE_RE.test(value)) return null;
        const k = name.toLowerCase();
        headers[k] = headers[k] === undefined ? value : headers[k] + ', ' + value;
    }
    return { method: m[1], url: m[2], httpVersion: `${m[3]}.${m[4]}`, httpVersionMajor: +m[3], httpVersionMinor: +m[4], headers };
}

// ---- socket event handlers shared by every connection (no closure per connection) ------------
function onSocketData(chunk) { this._ws._onData(chunk); }
function onSocketClose() { this._ws._onSocketClose(); }
function onSocketError() { /* 'close' follows */ }
function onSocketEnd() {
    // The peer half-closed: finish our side too (no data can arrive any more). Like every closing
    // path, destroyed after closeTimeoutMs at the latest: a peer that no longer reads would keep
    // the end() from finishing, and close() does nothing once _closeSent is set.
    const c = this._ws;
    if (!c._closeSent) {
        c._closeSent = true; c._closeCode = c._closeCode || 1006; c.state = 'closing';
        setTimeout(destroySocket, c._server.closeTimeoutMs, this).unref();
    }
    this.end();
}
function uncorkConnection(conn) {
    conn._corked = false;
    conn._socket.uncork();
}
function destroySocket(socket) { socket.destroy(); }

/**
 * One WebSocket connection (DESIGN 5.8). The router sets `onMessage(conn, buf)` and
 * `onClose(conn, code, reason)` in WsServer's onConnection callback, and may keep its own state
 * in `ctx`.
 */
export class WsConnection {
    constructor(server, socket, ip) {
        /** @type {number} u32, unique among the open connections of this server */
        this.id = server._allocId();
        this.ip = ip;
        this.userId = 0;
        this.username = '';
        /** @type {'hello'|'ready'|'closing'|'closed'} */
        this.state = 'hello';
        this.rttMs = 0;
        // Monotonic epoch milliseconds (clock.js now()), compared by the router's heartbeat sweep
        // and hello deadlines on the same clock: a step of the wall clock neither closes every
        // connection as silent nor holds the pings and timeouts back.
        this.openedAt = clockNow();
        this.lastRecvAt = this.openedAt;
        this.onMessage = null;
        this.onClose = null;
        this.ctx = null;
        this._server = server;
        this._socket = socket;
        this._rest = null;          // partial frame (copy), at most header + maxMessageBytes
        this._restLen = 0;          // bytes of it received
        this._restNeed = 0;
        this._frag = null;          // fragments of a message in progress
        this._fragLen = 0;
        this._fragCount = 0;
        this._closeSent = false;
        this._closeRecv = false;
        this._failed = false;
        this._closed = false;
        this._closeCode = 0;
        this._closeReason = '';
        this._corked = false;
        this._pingTokens = server.pingBurst;
        this._pingAt = this.openedAt;
        this._admitted = false;
        socket._ws = this;
        socket.on('data', onSocketData);
        socket.on('close', onSocketClose);
        socket.on('error', onSocketError);
        socket.on('end', onSocketEnd);
    }

    /** Bytes queued in user space for this client (not yet handed to the kernel). */
    get bufferedBytes() { return this._socket.writableLength; }

    /**
     * Sends one binary message. Returns false when the connection is closing/closed, or when the
     * client does not read fast enough (then the connection is closed with 4303).
     * @param {Buffer} buf
     * @returns {boolean}
     */
    sendFrame(buf) {
        if (this._closeSent) return false;
        const sock = this._socket;
        const len = buf.length;
        const srv = this._server;
        if (sock.writableLength + len > srv.sendBufferLimit) {
            srv._slow.inc();
            this.close(CloseCode.SlowConsumer, 'slow consumer');
            return false;
        }
        const h = frameHeader(OP_BINARY, len);
        if (!this._corked) { this._corked = true; sock.cork(); process.nextTick(uncorkConnection, this); }
        sock.write(h);
        sock.write(buf);
        srv._bytesOut.inc(h.length + len);
        srv._countOut(buf[0]);
        return true;
    }

    /**
     * Starts the closing handshake: sends a close frame and half-closes the TCP connection (the
     * peer can still send its own close frame; no RST that could discard the last frames we
     * sent). The socket is destroyed after closeTimeoutMs at the latest.
     * @param {number} [code]
     * @param {string} [reason] at most 123 bytes
     */
    close(code = CloseCode.Normal, reason = '') {
        if (this._closeSent || this._closed) return;
        this._closeSent = true;
        this._closeCode = code;
        this._closeReason = reason;
        this.state = 'closing';
        this._writeClose(code, reason);
        const sock = this._socket;
        sock.end();
        setTimeout(destroySocket, this._server.closeTimeoutMs, sock).unref();
    }

    /** Destroys the connection at once (no closing handshake). */
    terminate(code = 1006) {
        if (this._closed) return;
        if (!this._closeCode) this._closeCode = code;
        this._closeSent = true;
        this.state = 'closing';
        this._socket.destroy();
    }

    _writeClose(code, reason) {
        const sock = this._socket;
        if (sock.destroyed || sock.writableEnded) return;
        let payload;
        if (code === 1005 || code === 1006 || !code) payload = Buffer.alloc(0);
        else {
            const r = Buffer.from(String(reason || ''), 'utf8').subarray(0, 123);
            payload = Buffer.allocUnsafe(2 + r.length);
            payload.writeUInt16BE(code, 0);
            r.copy(payload, 2);
        }
        if (!this._corked) { this._corked = true; sock.cork(); process.nextTick(uncorkConnection, this); }
        sock.write(frameHeader(OP_CLOSE, payload.length));
        if (payload.length) sock.write(payload);
    }

    _writeControl(op, payload) {
        const sock = this._socket;
        if (this._closeSent || sock.destroyed) return;
        if (!this._corked) { this._corked = true; sock.cork(); process.nextTick(uncorkConnection, this); }
        sock.write(frameHeader(op, payload.length));
        if (payload.length) sock.write(Buffer.from(payload));   // tiny control payload: copy (the chunk may be long-lived)
    }

    _fail(code, why) {
        this._failed = true;
        this._server._protocolErrors.inc();
        this._server.log?.debug?.('ws protocol error', { connId: this.id, why, code });
        this.close(code, why);
        return false;
    }

    _onData(chunk) {
        if (this._failed || this._closed) return;
        this.lastRecvAt = clockNow();
        this._server._bytesIn.inc(chunk.length);
        while (this._rest !== null && chunk.length > 0) {
            // Appended to the partial frame; parsed again only once the header or the frame it
            // waits for is complete (nothing it checks changes in between). The buffer grows by
            // doubling, never beyond what is needed, and is never written again once parsed (the
            // payloads handed out are views of it).
            let rest = this._rest;
            const have = this._restLen, need = this._restNeed;
            const take = Math.min(need - have, chunk.length);
            if (have + take > rest.length) {
                const grown = Buffer.allocUnsafe(Math.min(need, Math.max(2 * rest.length, have + take)));
                rest.copy(grown, 0, 0, have);
                rest = this._rest = grown;
            }
            chunk.copy(rest, have, 0, take);
            this._restLen = have + take;
            chunk = chunk.subarray(take);
            if (this._restLen < need) return;
            this._rest = null;
            if (!this._parse(rest.subarray(0, need))) return;
        }
        if (chunk.length > 0) this._parse(chunk);
    }

    _saveRest(buf, off, need) {
        // Copy (bounded: need <= 14 + maxMessageBytes), with room for what comes next but at most
        // twice the bytes received: the announced length is not reserved before it arrives.
        const n = buf.length - off;
        this._rest = Buffer.allocUnsafe(Math.min(need, Math.max(64, 2 * n)));
        buf.copy(this._rest, 0, off);
        this._restLen = n;
        this._restNeed = need;
        return true;
    }

    // Parses every complete frame of buf; keeps a partial one. Returns false when the connection
    // failed or closed.
    _parse(buf) {
        const n = buf.length;
        const max = this._server.maxMessageBytes;
        let off = 0;
        while (off < n) {
            const avail = n - off;
            if (avail < 2) return this._saveRest(buf, off, 2);
            const b0 = buf[off], b1 = buf[off + 1];
            const fin = (b0 & 0x80) !== 0;
            const op = b0 & 0x0f;
            if (b0 & 0x70) return this._fail(CloseCode.ProtocolError, 'reserved bits');
            if ((b1 & 0x80) === 0) return this._fail(CloseCode.ProtocolError, 'unmasked frame');
            let len = b1 & 0x7f;
            if (op >= 8) {
                if (op > OP_PONG) return this._fail(CloseCode.ProtocolError, 'reserved opcode');
                if (!fin) return this._fail(CloseCode.ProtocolError, 'fragmented control frame');
                if (len > 125) return this._fail(CloseCode.ProtocolError, 'control frame too long');
            } else if (op === OP_TEXT) {
                return this._fail(CloseCode.Unsupported, 'text frames are not accepted');
            } else if (op !== OP_BINARY && op !== OP_CONT) {
                return this._fail(CloseCode.ProtocolError, 'reserved opcode');
            }
            const hl = len === 126 ? 8 : len === 127 ? 14 : 6;
            if (avail < hl) return this._saveRest(buf, off, hl);
            if (len === 126) {
                len = buf.readUInt16BE(off + 2);
                if (len < 126) return this._fail(CloseCode.ProtocolError, 'non-minimal length');
            } else if (len === 127) {
                const hi = buf.readUInt32BE(off + 2), lo = buf.readUInt32BE(off + 6);
                if (hi > 0x1fffff) return this._fail(CloseCode.ProtocolError, 'length above 2^53');
                if (hi === 0 && lo < 65536) return this._fail(CloseCode.ProtocolError, 'non-minimal length');
                len = hi * 0x100000000 + lo;
            }
            // Size limit from the header, before buffering anything.
            if (op < 8 && (op === OP_CONT ? this._fragLen : 0) + len > max) {
                this._server._tooBig.inc();
                return this._fail(CloseCode.TooBig, 'message too big');
            }
            const total = hl + len;
            if (avail < total) return this._saveRest(buf, off, total);
            const p = off + hl;
            if (len) unmask(buf, p - 4, p, len);
            off += total;
            if (!this._frame(fin, op, buf.subarray(p, p + len))) return false;
        }
        return true;
    }

    _frame(fin, op, payload) {
        const srv = this._server;
        switch (op) {
            case OP_BINARY:
                if (this._frag !== null) return this._fail(CloseCode.ProtocolError, 'expected a continuation frame');
                if (this._closeSent) return true;                         // data after our close: ignored
                if (fin) { this._deliver(payload); return !this._closed && !this._failed; }
                this._frag = [Buffer.from(payload)];
                this._fragLen = payload.length;
                this._fragCount = 1;
                return true;
            case OP_CONT: {
                if (this._frag === null) return this._fail(CloseCode.ProtocolError, 'unexpected continuation frame');
                if (++this._fragCount > MAX_FRAGMENTS) return this._fail(CloseCode.TooBig, 'too many fragments');
                if (payload.length) { this._frag.push(Buffer.from(payload)); this._fragLen += payload.length; }
                if (!fin) return true;
                const msg = this._frag.length === 1 ? this._frag[0] : Buffer.concat(this._frag, this._fragLen);
                this._frag = null;
                this._fragLen = 0;
                if (this._closeSent) return true;
                this._deliver(msg);
                return !this._closed && !this._failed;
            }
            case OP_PING: {
                // Bounded pong rate: a ping flood costs the flooder, not us.
                const now = this.lastRecvAt;
                this._pingTokens = Math.min(srv.pingBurst, this._pingTokens + (now - this._pingAt) * srv.pingRate / 1000);
                this._pingAt = now;
                if (this._pingTokens >= 1) { this._pingTokens -= 1; this._writeControl(OP_PONG, payload); }
                else srv._pingsDropped.inc();
                return true;
            }
            case OP_PONG:
                return true;
            case OP_CLOSE: {
                let code = 1005, reason = '';
                if (payload.length === 1) return this._fail(CloseCode.ProtocolError, 'bad close payload');
                if (payload.length >= 2) {
                    code = payload.readUInt16BE(0);
                    if (!isValidCloseCode(code)) return this._fail(CloseCode.ProtocolError, 'invalid close code');
                    try { reason = utf8.decode(payload.subarray(2)); } catch { return this._fail(1007, 'close reason not UTF-8'); }
                }
                this._closeRecv = true;
                if (!this._closeSent) {
                    // Peer-initiated: echo the code, then half-close.
                    this._closeSent = true;
                    this._closeCode = code;
                    this._closeReason = reason;
                    this.state = 'closing';
                    this._writeClose(code === 1005 ? 1000 : code, '');
                    this._socket.end();
                    setTimeout(destroySocket, srv.closeTimeoutMs, this._socket).unref();
                }
                return false;
            }
        }
        return this._fail(CloseCode.ProtocolError, 'reserved opcode');
    }

    _deliver(payload) {
        const srv = this._server;
        srv._msgsIn.inc();
        const cb = this.onMessage;
        if (cb === null) return;
        try {
            cb(this, payload);
        } catch (e) {
            srv.log?.error?.('message handler failed', { connId: this.id, err: e });
            this.close(CloseCode.Internal, 'internal error');
        }
    }

    _onSocketClose() {
        if (this._closed) return;
        this._closed = true;
        this.state = 'closed';
        const code = this._closeCode || 1006;
        this._rest = null;
        this._frag = null;
        const srv = this._server;
        srv._onConnectionClosed(this, code);
        const cb = this.onClose;
        if (cb !== null) {
            try { cb(this, code, this._closeReason); } catch (e) { srv.log?.error?.('close handler failed', { connId: this.id, err: e }); }
        }
    }
}

/**
 * WebSocket server (DESIGN 5.8). Transport-agnostic: listeners hand it upgrade requests
 * (`handleUpgrade`) or raw sockets (`handleSocket`).
 */
export class WsServer {
    /**
     * @param {object} o
     * @param {number} [o.maxMessageBytes] largest client message (WS_MAX_MESSAGE_BYTES)
     * @param {string} [o.subprotocol] required Sec-WebSocket-Protocol token
     * @param {string[]} [o.allowOrigins] Origin values accepted; requests carrying any other Origin are refused
     * @param {string} [o.path] request path ('/ws')
     * @param {number} [o.sendBufferLimit] WS_SEND_BUFFER_LIMIT
     * @param {(conn: WsConnection, req: object) => void} o.onConnection
     * @param {{acquire(ip: string, req: object): (boolean|{ok:boolean,status?:number,error?:string}|Promise<any>), release(ip: string): void}} [o.admission]
     *        connection admission (per-IP / global limits), called last, after every check passed
     * @param {(req: object, socket: object) => string} [o.clientIp] client address (proxy aware)
     * @param {(type: number) => string} [o.messageLabel] metric label of an outgoing message (by first byte)
     * @param {Record<string, string>} [o.upgradeHeaders] extra headers of every 101 response (the
     *        shards send Scacelith-Server-Id, so that a client checks whom it talks to before Hello)
     * @param {object} [o.log] logger
     * @param {object} [o.registry] metrics registry
     * @param {import('./ipguard.js').IpGuard|null} [o.guard] protection per address, checked first
     */
    constructor({
        maxMessageBytes = 512, subprotocol = WS_SUBPROTOCOL, allowOrigins = [], path = '/ws',
        sendBufferLimit = 262144, onConnection, admission = null, clientIp = null, messageLabel = null,
        upgradeHeaders = null, log = null, registry = defaultRegistry, handshakeTimeoutMs = 10000,
        maxHeaderBytes = 8192, closeTimeoutMs = 2000, pingRate = 2, pingBurst = 5, guard = null,
    } = {}) {
        this.maxMessageBytes = maxMessageBytes;
        this.subprotocol = subprotocol;
        this._upgradeExtra = '';
        for (const [name, value] of Object.entries(upgradeHeaders || {})) {
            const v = String(value);
            if (!TOKEN_RE.test(name) || BAD_VALUE_RE.test(v)) throw new Error(`invalid upgrade header ${name}`);
            this._upgradeExtra += `${name}: ${v}\r\n`;
        }
        this.allowOrigins = new Set((allowOrigins || []).map((o) => String(o).trim().toLowerCase()).filter(Boolean));
        this.path = path;
        this.sendBufferLimit = sendBufferLimit;
        this.onConnection = onConnection;
        this.admission = admission;
        this.guard = guard;
        this.clientIp = clientIp || ((req, socket) => socket.remoteAddress || '');
        this.log = log;
        this.handshakeTimeoutMs = handshakeTimeoutMs;
        this.maxHeaderBytes = maxHeaderBytes;
        this.closeTimeoutMs = closeTimeoutMs;
        this.pingRate = pingRate;
        this.pingBurst = pingBurst;
        /** When false, new upgrades are refused with 503 (shutdown). */
        this.accepting = true;
        /** @type {Map<number, WsConnection>} */
        this.connections = new Map();
        this._nextId = 1;

        const r = registry;
        this._accepted = r.counter('scacelith_ws_handshakes_accepted_total', 'WebSocket upgrades accepted');
        this._rejected = r.counter('scacelith_ws_handshakes_rejected_total', 'WebSocket upgrades refused', ['reason']);
        this._handshakeMs = r.histogram('scacelith_ws_handshake_ms', 'Upgrade request to 101 response (admission included)', [0.5, 1, 2, 5, 10, 25, 50, 100, 250]);
        this._bytesIn = r.counter('scacelith_ws_bytes_in_total', 'Bytes received on WebSocket connections');
        this._bytesOut = r.counter('scacelith_ws_bytes_out_total', 'Bytes queued on WebSocket connections');
        this._msgsIn = r.counter('scacelith_ws_frames_in_total', 'WebSocket data messages received');
        this._slow = r.counter('scacelith_ws_slow_consumers_total', 'Connections closed because the client did not read');
        this._tooBig = r.counter('scacelith_ws_too_big_total', 'Connections closed for an oversized message');
        this._protocolErrors = r.counter('scacelith_ws_protocol_errors_total', 'Connections failed for a WebSocket protocol violation');
        this._pingsDropped = r.counter('scacelith_ws_pings_dropped_total', 'WebSocket pings not answered (rate limit)');
        this._closes = r.counter('scacelith_ws_closes_total', 'Closed WebSocket connections by close code', ['code']);
        this._outByType = r.counter('scacelith_ws_messages_out_total', 'Messages sent by type', ['type']);
        this._outChildren = new Array(256).fill(null);
        this._messageLabel = messageLabel || ((t) => '0x' + t.toString(16).padStart(2, '0'));
        this._closeChildren = new Map();
    }

    /** Open connections. */
    get size() { return this.connections.size; }

    _countOut(type) {
        let c = this._outChildren[type];
        if (c === null) c = this._outChildren[type] = this._outByType.labels(this._messageLabel(type));
        c.inc();
    }

    _allocId() {
        for (;;) {
            const id = this._nextId;
            this._nextId = id >= 0xffffffff ? 1 : id + 1;
            if (!this.connections.has(id)) return id;
        }
    }

    _onConnectionClosed(conn, code) {
        this.connections.delete(conn.id);
        const label = (code >= 1000 && code <= 1015) || (code >= 4000 && code <= 4399) ? String(code) : 'other';
        let c = this._closeChildren.get(label);
        if (!c) { c = this._closes.labels(label); this._closeChildren.set(label, c); }
        c.inc();
        if (conn._admitted && this.admission) {
            conn._admitted = false;
            try { this.admission.release(conn.ip); } catch { /* ignore */ }
        }
    }

    /** Sends a close frame to every connection. */
    closeAll(code = CloseCode.GoingAway, reason = '') {
        for (const c of [...this.connections.values()]) c.close(code, reason);
    }

    /**
     * Takes a raw socket (TCP or TLS) whose first bytes are an HTTP upgrade request.
     * @param {import('node:net').Socket} socket
     */
    handleSocket(socket) {
        socket.setNoDelay(true);
        socket.on('error', noop);
        let buf = null;
        const timer = setTimeout(() => {
            socket.removeListener('data', onData);
            this._reject(socket, 408, 'timeout', null);
        }, this.handshakeTimeoutMs);
        timer.unref();
        const onData = (chunk) => {
            buf = buf === null ? chunk : Buffer.concat([buf, chunk]);
            const end = buf.indexOf('\r\n\r\n', Math.max(0, buf.length - chunk.length - 3), 'latin1');
            if (end < 0) {
                if (buf.length > this.maxHeaderBytes) { clearTimeout(timer); socket.removeListener('data', onData); this._reject(socket, 431, 'headers_too_large', null); }
                return;
            }
            clearTimeout(timer);
            socket.removeListener('data', onData);
            socket.pause();
            if (end + 4 > this.maxHeaderBytes) { this._reject(socket, 431, 'headers_too_large', null); return; }
            const req = parseRequestHead(buf.subarray(0, end));
            if (!req) { this._reject(socket, 400, 'bad_request', null); return; }
            req.socket = socket;
            this.handleUpgrade(req, socket, buf.subarray(end + 4));
        };
        socket.on('data', onData);
    }

    /**
     * Validates an upgrade request and, when everything is right, answers 101 and creates the
     * connection. Refusals are small HTTP responses with a JSON body, then the socket is closed.
     * @param {{method:string,url:string,httpVersion:string,headers:object}} req
     * @param {import('node:net').Socket} socket
     * @param {Buffer} head bytes already read after the request head
     */
    handleUpgrade(req, socket, head) {
        const t0 = performance.now();
        socket.on('error', noop);
        let ip = null;
        if (this.guard !== null) {
            ip = this.clientIp(req, socket);
            const r = this.guard.request(this.guard.keysOf(ip, socket));
            if (r !== null) {
                const s = Math.max(1, Math.ceil(r.retryAfterMs / 1000));
                return this._reject(socket, 429, 'rate_limited', `Retry-After: ${s}\r\n`,
                    { error: 'rate_limited', message: 'Too many requests; try again later.', retryAfter: s });
            }
        }
        const h = req.headers;
        if (!this.accepting) return this._reject(socket, 503, 'shutting_down', null);
        if (req.method !== 'GET') return this._reject(socket, 405, 'method_not_allowed', 'Allow: GET\r\n');
        if (req.httpVersion !== '1.1') return this._reject(socket, 400, 'http_version', null);
        const q = req.url.indexOf('?');
        const pathname = q >= 0 ? req.url.slice(0, q) : req.url;
        if (pathname !== this.path) return this._reject(socket, 404, 'not_found', null);
        if (!h.host) return this._reject(socket, 400, 'bad_upgrade', null);
        if (!tokens(h.upgrade).includes('websocket') || !tokens(h.connection).includes('upgrade')) return this._reject(socket, 400, 'bad_upgrade', null);
        if (h['sec-websocket-version'] !== '13') return this._reject(socket, 426, 'unsupported_version', 'Sec-WebSocket-Version: 13\r\n');
        const key = h['sec-websocket-key'];
        if (typeof key !== 'string' || !KEY_RE.test(key)) return this._reject(socket, 400, 'bad_key', null);
        const origin = h.origin ?? h['sec-websocket-origin'];
        if (origin !== undefined && !this.allowOrigins.has(String(origin).trim().toLowerCase())) return this._reject(socket, 403, 'origin_forbidden', null);
        if (!tokens(h['sec-websocket-protocol']).includes(this.subprotocol)) {
            return this._reject(socket, 426, 'unsupported_protocol', null, { error: 'unsupported_protocol', supported: [this.subprotocol] });
        }
        if (ip === null) ip = this.clientIp(req, socket);
        const adm = this.admission;
        if (!adm) return this._accept(req, socket, head, key, ip, false, t0);
        let r;
        try { r = adm.acquire(ip, req); } catch (e) { this.log?.error?.('admission failed', { err: e }); return this._reject(socket, 503, 'admission_error', null); }
        if (r && typeof r.then === 'function') {
            r.then((v) => this._admitted(req, socket, head, key, ip, v, t0),
                (e) => { this.log?.warn?.('admission failed', { err: e }); this._reject(socket, 503, 'admission_error', null); });
            return undefined;
        }
        return this._admitted(req, socket, head, key, ip, r, t0);
    }

    _admitted(req, socket, head, key, ip, r, t0) {
        const ok = r === true || (r && r.ok);
        if (!ok) {
            if (r && r.ok === false && socket.destroyed) return undefined;
            const status = (r && r.status) || 429;
            return this._reject(socket, status, (r && r.error) || 'too_many_connections', null);
        }
        if (socket.destroyed || !this.accepting) {
            try { this.admission.release(ip); } catch { /* ignore */ }
            if (!socket.destroyed) this._reject(socket, 503, 'shutting_down', null);
            return undefined;
        }
        return this._accept(req, socket, head, key, ip, true, t0);
    }

    _accept(req, socket, head, key, ip, admitted, t0) {
        socket.setNoDelay(true);
        socket.setTimeout(0);
        socket.write(
            'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n' +
            `Sec-WebSocket-Accept: ${acceptKey(key)}\r\nSec-WebSocket-Protocol: ${this.subprotocol}\r\n${this._upgradeExtra}\r\n`);
        const conn = new WsConnection(this, socket, ip);
        conn._admitted = admitted;
        this.connections.set(conn.id, conn);
        this._accepted.inc();
        this._handshakeMs.observe(performance.now() - t0);
        try {
            this.onConnection?.(conn, req);
        } catch (e) {
            this.log?.error?.('onConnection failed', { err: e });
            conn.close(CloseCode.Internal, 'internal error');
        }
        if (head && head.length) conn._onData(Buffer.from(head));
        socket.resume();
        return conn;
    }

    _reject(socket, status, error, extraHeaders, body) {
        this._rejected.labels(error).inc();
        if (socket.destroyed) return undefined;
        const json = JSON.stringify(body || { error });
        try {
            socket.end(`HTTP/1.1 ${status} ${STATUS_TEXT[status] || 'Error'}\r\nConnection: close\r\nCache-Control: no-store\r\n` +
                `Content-Type: application/json\r\nContent-Length: ${Buffer.byteLength(json)}\r\n${extraHeaders || ''}\r\n${json}`);
        } catch { /* ignore */ }
        setTimeout(destroySocket, 1000, socket).unref();
        return undefined;
    }
}
