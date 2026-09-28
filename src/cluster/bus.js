// Shard bus: relays game traffic between shards (DESIGN 1, 5.8, 9). A player's connection may live
// on any shard while the game lives on its host shard: C2S game frames go to the host (ToHost),
// the host's S2C frames come back (ToConn) and are written to the socket unchanged.
//
// Frame (little-endian), 21-byte header:
//   u32 len        bytes after this field (17 + payload length)
//   u8  kind       1 ToHost, 2 ToConn, 3 Control
//   u32 connId     connection on the source shard (ToHost, Control) or the destination (ToConn)
//   u32 userId     authenticated user of that connection
//   u64 gameId     id53 (0 when not applicable)
//   ... payload    ToHost: the client's raw C2S frame; ToConn: an encoded S2C frame;
//                  Control: u8 op + op data (Hello: u8 shard, 32-byte token; Rtt: u16 ms;
//                  Close: u16 close code; Attach / Detach / Forfeit / RematchDecline: none)
//
// Links are one-way: each shard connects to a peer when it first has something to send to it,
// and receives on the connections its peers opened. The first frame on a link is Control/Hello
// with the sender's shard number and a token derived from SERVER_SECRET; anything else, or a bad
// token, closes the link. Outgoing frames are appended to one buffer per peer and written once per
// event-loop turn (setImmediate): one write for a whole burst, and no per-frame allocation. While
// a link is down, frames wait (up to maxQueueBytes per peer, then they are dropped and counted)
// and the link reconnects with a backoff (50 ms doubling to 2 s).
//
// Transport interface (so that a TCP+mTLS implementation can join shards of several machines):
//   transport.listen(selfShard, onSocket(socket)) -> Promise<void>   accept links from peers
//   transport.connect(peerShard) -> { socket, readyEvent }            open a link ('connect' / 'secureConnect')
//   transport.close() -> Promise<void>
//   transport.describe(shard) -> string                               for logs
// unixTransport (Unix domain sockets in runDir, named pipes on Windows) is the single-machine
// implementation; tcpTransport (plain TCP or mutual TLS) the multi-machine one.

import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import tls from 'node:tls';
import { metrics as defaultRegistry } from '../metrics.js';

export const BusKind = Object.freeze({ ToHost: 1, ToConn: 2, Control: 3 });
export const BusOp = Object.freeze({ Hello: 1, Attach: 2, Detach: 3, Rtt: 4, Forfeit: 5, Close: 6, RematchDecline: 7 });
export const BUS_HEADER_BYTES = 21;
const TOKEN_BYTES = 32;

/**
 * Link authentication token of a deployment.
 * @param {Buffer} secret SERVER_SECRET
 * @param {string} serverId
 */
export function busToken(secret, serverId) {
    return crypto.createHmac('sha256', secret || Buffer.alloc(0)).update(`scacelith-bus-v1:${serverId}`).digest();
}

/**
 * Single-machine transport: Unix domain sockets (`<runDir>/bus-<shard>.sock`, mode 0600) or, on
 * Windows, named pipes (`\\.\pipe\scacelith-<serverId>-<shard>`).
 * @param {{ runDir: string, serverId: string, platform?: string }} o
 */
export function unixTransport({ runDir, serverId, platform = process.platform }) {
    let server = null;
    const pathOf = (shard) => {
        if (platform === 'win32') return `\\\\.\\pipe\\scacelith-${serverId}-${shard}`;
        const p = path.join(runDir, `bus-${shard}.sock`);
        if (Buffer.byteLength(p) <= 100) return p;           // sun_path is 104-108 bytes
        return path.join(os.tmpdir(), `scacelith-${String(serverId).slice(0, 8)}-bus-${shard}.sock`);
    };
    return {
        kind: 'unix',
        describe: pathOf,
        listen(shard, onSocket) {
            const p = pathOf(shard);
            if (platform !== 'win32') {
                try { if (fs.statSync(p).isSocket()) fs.unlinkSync(p); } catch { /* absent */ }
            }
            server = net.createServer(onSocket);
            return new Promise((resolve, reject) => {
                server.once('error', reject);
                server.listen(p, () => {
                    server.removeListener('error', reject);
                    if (platform !== 'win32') { try { fs.chmodSync(p, 0o600); } catch { /* ignore */ } }
                    resolve();
                });
            });
        },
        connect(shard) { return { socket: net.connect(pathOf(shard)), readyEvent: 'connect' }; },
        close() {
            return new Promise((resolve) => {
                if (!server) return resolve();
                const s = server;
                server = null;
                s.close(() => resolve());
                return undefined;
            });
        },
    };
}

/**
 * Multi-machine transport over TCP, with mutual TLS when `tls` is given (every shard presents a
 * certificate of the deployment's private CA and verifies its peer's).
 * @param {{ addressOf: (shard: number) => {host: string, port: number}, bindHost?: string,
 *           tls?: { key: Buffer|string, cert: Buffer|string, ca: Buffer|string } | null }} o
 */
export function tcpTransport({ addressOf, bindHost = '0.0.0.0', tls: tlsOpts = null }) {
    let server = null;
    return {
        kind: tlsOpts ? 'tcp+mtls' : 'tcp',
        describe: (shard) => { const a = addressOf(shard); return `${a.host}:${a.port}`; },
        listen(shard, onSocket) {
            const { port } = addressOf(shard);
            server = tlsOpts
                ? tls.createServer({ ...tlsOpts, requestCert: true, rejectUnauthorized: true, minVersion: 'TLSv1.3' }, onSocket)
                : net.createServer(onSocket);
            return new Promise((resolve, reject) => {
                server.once('error', reject);
                server.listen(port, bindHost, () => { server.removeListener('error', reject); resolve(); });
            });
        },
        connect(shard) {
            const { host, port } = addressOf(shard);
            if (!tlsOpts) return { socket: net.connect({ host, port, noDelay: true }), readyEvent: 'connect' };
            const socket = tls.connect({ host, port, ...tlsOpts, minVersion: 'TLSv1.3', checkServerIdentity: () => undefined });
            return { socket, readyEvent: 'secureConnect' };
        },
        close() {
            return new Promise((resolve) => {
                if (!server) return resolve();
                const s = server;
                server = null;
                s.close(() => resolve());
                return undefined;
            });
        },
    };
}

function writeHeader(buf, o, len, kind, connId, userId, gameId) {
    buf.writeUInt32LE(len, o);
    buf[o + 4] = kind;
    buf.writeUInt32LE(connId >>> 0, o + 5);
    buf.writeUInt32LE(userId >>> 0, o + 9);
    const g = gameId || 0;
    buf.writeUInt32LE(g % 0x100000000, o + 13);
    buf.writeUInt32LE(Math.floor(g / 0x100000000), o + 17);
}

class OutLink {
    constructor(peer) {
        this.peer = peer;
        this.socket = null;
        this.connected = false;
        this.buf = null;
        this.len = 0;
        this.backoff = 0;
        this.timer = null;
        this.everConnected = false;
    }
}

class InLink {
    constructor(socket) {
        this.socket = socket;
        this.peer = -1;
        this.rest = null;
        this.restNeed = 0;
    }
}

/**
 * The bus endpoint of one shard.
 */
export class Bus {
    /**
     * @param {object} o
     * @param {number} o.shard this shard
     * @param {object} o.transport see the transport interface above
     * @param {Buffer} o.token busToken(secret, serverId)
     * @param {(kind:number, fromShard:number, connId:number, userId:number, gameId:number, payload:Buffer) => void} o.onMessage
     * @param {(peer:number, reconnected:boolean) => void} [o.onLinkUp] an outgoing link (re)connected
     * @param {object} [o.log]
     * @param {object} [o.registry]
     * @param {number} [o.maxQueueBytes] per peer, while the link is down or the peer does not read
     * @param {number} [o.maxFrameBytes]
     */
    constructor({ shard, transport, token, onMessage, onLinkUp = null, log = null, registry = defaultRegistry, maxQueueBytes = 8 << 20, maxFrameBytes = 1 << 20 }) {
        this.shard = shard;
        this.transport = transport;
        this.token = token;
        this.onMessage = onMessage;
        this.onLinkUp = onLinkUp;
        this.log = log;
        this.maxQueueBytes = maxQueueBytes;
        this.maxFrameBytes = maxFrameBytes;
        /** @type {Map<number, OutLink>} */
        this.out = new Map();
        /** @type {Set<InLink>} */
        this.in = new Set();
        this.closed = false;
        this._dirty = new Set();
        this._scheduled = false;
        this._flush = () => this._flushAll();
        const r = registry;
        this._framesOut = r.counter('scacelith_bus_frames_out_total', 'Frames sent on the shard bus');
        this._framesIn = r.counter('scacelith_bus_frames_in_total', 'Frames received on the shard bus');
        this._bytesOut = r.counter('scacelith_bus_bytes_out_total', 'Bytes written on the shard bus');
        this._writes = r.counter('scacelith_bus_writes_total', 'Write calls on the shard bus (batches)');
        this._dropped = r.counter('scacelith_bus_dropped_total', 'Bus frames dropped (peer unreachable or not reading)');
        this._reconnects = r.counter('scacelith_bus_reconnects_total', 'Bus link (re)connections');
    }

    /** Starts accepting links from the peers. */
    async start() {
        await this.transport.listen(this.shard, (socket) => this._accept(socket));
    }

    /**
     * Queues one frame for a peer shard. The payload is copied into the peer's batch buffer, so
     * the caller may reuse it.
     * @returns {boolean} false when dropped
     */
    send(peer, kind, connId, userId, gameId, payload) {
        if (this.closed) return false;
        if (peer === this.shard) {
            // Local delivery (should not happen: the router serves local games directly).
            const copy = Buffer.from(payload);
            setImmediate(() => this.onMessage(kind, this.shard, connId, userId, gameId, copy));
            return true;
        }
        const link = this._link(peer);
        const plen = payload ? payload.length : 0;
        const need = BUS_HEADER_BYTES + plen;
        const queued = link.len + (link.socket && link.connected ? link.socket.writableLength : 0);
        if (queued + need > this.maxQueueBytes) { this._dropped.inc(); return false; }
        this._reserve(link, need);
        writeHeader(link.buf, link.len, need - 4, kind, connId, userId, gameId);
        if (plen) payload.copy(link.buf, link.len + BUS_HEADER_BYTES);
        link.len += need;
        this._framesOut.inc();
        if (link.connected) {
            this._dirty.add(link);
            if (!this._scheduled) { this._scheduled = true; setImmediate(this._flush); }
        }
        return true;
    }

    /**
     * Queues a control frame.
     * @param {number} peer
     * @param {number} op BusOp
     * @param {number} connId
     * @param {number} userId
     * @param {number} gameId
     * @param {Buffer} [extra] op data
     */
    control(peer, op, connId, userId, gameId, extra = null) {
        const p = Buffer.allocUnsafe(1 + (extra ? extra.length : 0));
        p[0] = op;
        if (extra) extra.copy(p, 1);
        return this.send(peer, BusKind.Control, connId, userId, gameId, p);
    }

    _reserve(link, need) {
        if (link.buf === null) {
            link.buf = Buffer.allocUnsafe(Math.max(16384, need));
            link.len = 0;
            return;
        }
        if (link.len + need <= link.buf.length) return;
        const nb = Buffer.allocUnsafe(Math.max(link.buf.length * 2, link.len + need));
        link.buf.copy(nb, 0, 0, link.len);
        link.buf = nb;
    }

    _link(peer) {
        let link = this.out.get(peer);
        if (!link) {
            link = new OutLink(peer);
            this.out.set(peer, link);
            this._connect(link);
        } else if (!link.socket && !link.timer) {
            this._connect(link);
        }
        return link;
    }

    _connect(link) {
        if (this.closed) return;
        link.timer = null;
        let socket, readyEvent;
        try {
            ({ socket, readyEvent } = this.transport.connect(link.peer));
        } catch (e) {
            this.log?.warn?.('bus connect failed', { peer: link.peer, err: e });
            this._retry(link);
            return;
        }
        link.socket = socket;
        socket.setNoDelay?.(true);
        socket.once(readyEvent, () => {
            if (link.socket !== socket) return;
            const hello = Buffer.allocUnsafe(BUS_HEADER_BYTES + 2 + TOKEN_BYTES);
            writeHeader(hello, 0, hello.length - 4, BusKind.Control, 0, 0, 0);
            hello[BUS_HEADER_BYTES] = BusOp.Hello;
            hello[BUS_HEADER_BYTES + 1] = this.shard;
            this.token.copy(hello, BUS_HEADER_BYTES + 2, 0, TOKEN_BYTES);
            socket.write(hello);
            const reconnected = link.everConnected;
            link.connected = true;
            link.everConnected = true;
            link.backoff = 0;
            this._reconnects.inc();
            if (link.len) { this._dirty.add(link); if (!this._scheduled) { this._scheduled = true; setImmediate(this._flush); } }
            try { this.onLinkUp?.(link.peer, reconnected); } catch (e) { this.log?.error?.('bus onLinkUp failed', { err: e }); }
        });
        socket.on('data', () => { /* peers never answer on our outgoing link */ });
        socket.on('end', () => { if (link.socket === socket) link.connected = false; });   // peer going away: queue from now on
        socket.on('error', (e) => { this.log?.debug?.('bus link error', { peer: link.peer, code: e.code }); });
        socket.on('close', () => {
            if (link.socket !== socket) return;
            link.socket = null;
            link.connected = false;
            if (!this.closed && link.len > 0) this._retry(link);
        });
    }

    _retry(link) {
        if (this.closed || link.timer) return;
        link.backoff = link.backoff ? Math.min(2000, link.backoff * 2) : 50;
        link.timer = setTimeout(() => this._connect(link), link.backoff);
        link.timer.unref?.();
    }

    _flushAll() {
        this._scheduled = false;
        for (const link of this._dirty) {
            if (!link.connected || !link.len) continue;
            const chunk = link.buf.subarray(0, link.len);
            link.buf = null;
            link.len = 0;
            this._bytesOut.inc(chunk.length);
            this._writes.inc();
            link.socket.write(chunk);
        }
        this._dirty.clear();
    }

    _accept(socket) {
        const link = new InLink(socket);
        this.in.add(link);
        socket.setNoDelay?.(true);
        const t = setTimeout(() => { if (link.peer < 0) socket.destroy(); }, 5000);
        t.unref?.();
        socket.on('data', (chunk) => this._onData(link, chunk));
        socket.on('error', () => {});
        socket.on('close', () => { clearTimeout(t); this.in.delete(link); });
    }

    _onData(link, chunk) {
        while (link.rest !== null && chunk.length > 0) {
            const rest = link.rest;
            const take = Math.min(link.restNeed - rest.length, chunk.length);
            const joined = Buffer.allocUnsafe(rest.length + take);
            rest.copy(joined, 0);
            chunk.copy(joined, rest.length, 0, take);
            chunk = chunk.subarray(take);
            link.rest = null;
            if (!this._parse(link, joined)) return;
        }
        if (chunk.length > 0) this._parse(link, chunk);
    }

    _parse(link, buf) {
        const n = buf.length;
        let off = 0;
        while (off < n) {
            const avail = n - off;
            if (avail < 4) { link.rest = Buffer.from(buf.subarray(off)); link.restNeed = 4; return true; }
            const len = buf.readUInt32LE(off);
            if (len < BUS_HEADER_BYTES - 4 || len > this.maxFrameBytes) { this._badLink(link, 'bad frame length'); return false; }
            const total = 4 + len;
            if (avail < total) { link.rest = Buffer.from(buf.subarray(off)); link.restNeed = total; return true; }
            const kind = buf[off + 4];
            const connId = buf.readUInt32LE(off + 5);
            const userId = buf.readUInt32LE(off + 9);
            const gameId = buf.readUInt32LE(off + 17) * 0x100000000 + buf.readUInt32LE(off + 13);
            const payload = buf.subarray(off + BUS_HEADER_BYTES, off + total);
            off += total;
            if (link.peer < 0) {
                // First frame: Hello.
                if (kind !== BusKind.Control || payload.length !== 2 + TOKEN_BYTES || payload[0] !== BusOp.Hello
                    || !crypto.timingSafeEqual(payload.subarray(2), this.token)) {
                    this._badLink(link, 'bad hello');
                    return false;
                }
                link.peer = payload[1];
                continue;
            }
            this._framesIn.inc();
            try {
                this.onMessage(kind, link.peer, connId, userId, gameId, payload);
            } catch (e) {
                this.log?.error?.('bus handler failed', { kind, err: e });
            }
        }
        return true;
    }

    _badLink(link, why) {
        this.log?.warn?.('bus link refused', { why, peer: link.peer });
        link.rest = null;
        link.socket.destroy();
    }

    /** Link states (diagnostics). */
    stats() {
        const out = [];
        for (const l of this.out.values()) out.push({ peer: l.peer, connected: l.connected, queued: l.len });
        return { out, in: [...this.in].map((l) => l.peer) };
    }

    /** Flushes pending frames and closes every link. */
    async close() {
        if (this.closed) return;
        this._flushAll();
        this.closed = true;
        for (const l of this.out.values()) {
            clearTimeout(l.timer);
            if (l.socket) l.socket.end();
        }
        for (const l of this.in) l.socket.destroy();
        await this.transport.close();
    }
}
