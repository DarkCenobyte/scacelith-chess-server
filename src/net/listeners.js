// Network listeners of a shard: the HTTPS API and the WSS endpoint, in the three TLS modes.
//
//   native  API: https.createServer on API_PORT. WSS: tls.createServer on WS_PORT whose sockets go
//           straight to WsServer.handleSocket (strict, bounded upgrade parser; no http.Server per
//           connection). When WS_PORT == API_PORT, one https server serves the API and the
//           upgrade ('upgrade' event).
//   proxy   Same layout in plain text (http / net) on BIND_ADDRESS, behind a TLS-terminating
//           proxy; the client address comes from X-Forwarded-For only when the peer is one of
//           TRUSTED_PROXIES (addresses or CIDR subnets).
//   off     Plain text for local development (config refuses it without ALLOW_INSECURE_DEV);
//           X-Forwarded-For is never trusted.
//
// TLS: TLS_MIN_VERSION, Node's default (secure) cipher list with server preference, ALPN
// http/1.1, session tickets on. Every worker applies the same ticket keys, rotated daily (UTC), so
// that it can resume every other worker's sessions: a reconnecting client usually skips the full
// handshake whichever worker the kernel hands it to. They come from a random key the primary draws
// at its start and moves forward one way each day (net/ticket-keys.js), never from SERVER_SECRET:
// neither that secret nor a later memory dump recomputes a past day's keys. Certificates are
// reloaded without a restart on SIGHUP (the primary forwards it as the 'tls.reload' IPC message)
// and when the files change (stat polling, which also follows certbot's symlink swaps); a broken
// new certificate is refused and the old one stays in use.
//
// Cluster: by default workers listen through the cluster module (the primary accepts and hands
// connections out round-robin). LISTEN_REUSE_PORT=true on Linux makes each worker bind its own
// socket with SO_REUSEPORT (exclusive listen) and lets the kernel spread connections; see
// tools/ws-echo-bench.js --cluster for the comparison. Every listen uses LISTEN_BACKLOG (the
// kernel caps it at net.core.somaxconn); the round-robin handle of the primary honours it too.
//
// Admission before TLS (native mode; TlsGate): a TLS server wraps each accepted TCP socket in a
// TLSSocket from its own 'connection' listener. The gate runs in front of that listener, in three
// stages, and counts every socket it closes in scacelith_tls_refused_total{reason}:
//   0. The protection per address (net/ipguard.js IpGuard.connection, when the shard gives a
//      guard; ABUSE_EXEMPT addresses skip it): a blocked IPv4 address, IPv6 /64 or /48
//      ("blocked"), more than IP_CONN_RATE new connections per second ("conn_rate") or
//      IP_MAX_CONNECTIONS open ones ("conn_open", handshakes, API keep-alive and WebSockets
//      together) from one address are closed with an RST at once: two Map lookups and a token
//      bucket, no TLS byte, no slot. The other sockets are counted open until they close.
//   1. A new socket first waits, without a handshake slot, until its first TLS record has arrived
//      whole: a handshake record of at most 16 KB that starts the ClientHello (clients send the
//      whole ClientHello in that record, unless they fragment it on purpose). It has
//      HELLO_TIMEOUT_MS (3 s) for that, however it splits the bytes; a socket that stays silent,
//      is too slow, ends or sends anything else is closed. Waiting sockets cost no CPU and little
//      memory, and they are bounded as well: 16 * MAX_PENDING_HANDSHAKES of them per worker, and
//      4 * MAX_PENDING_HANDSHAKES_PER_IP per address group.
//   2. The socket then takes a handshake slot and goes to Node's listener with the record still
//      unread (a TLSSocket replays what its raw socket has buffered). Beyond
//      MAX_PENDING_HANDSHAKES slots per worker, or MAX_PENDING_HANDSHAKES_PER_IP for one address
//      group, it is closed with an RST instead, before any TLS work. A slot is held until
//      'secureConnection', 'tlsClientError' or the close of the socket. The handshake timeout
//      (10 s) bounds it: Node only reports that timeout and leaves the socket open, so the gate
//      destroys every socket whose handshake failed or timed out.
// An address group is an IPv4 address or an IPv6 /48 (one customer's allocation), and the
// per-group caps are small, so a few hosts cannot hold every slot. The CPU then serves a
// reconnection storm in turn (one handshake is 1-3.5 ms of CPU) instead of starting every
// handshake at once and finishing none before the clients' deadline; a refused client retries
// with its backoff. An attacker with enough address groups can still fill the caps: see
// docs/DESIGN.md 5.8 for what remains possible. The refusals that say something about one address
// (conn_rate, conn_open, per_ip, waiting_per_ip, bad_hello, hello_timeout, and a failed handshake)
// count toward a block of that address (IpGuard.noteRefusal); the server-wide ones (handshakes,
// waiting, server_full) do not.
// On the listener that carries the WebSocket upgrade, the gate also sheds load while the `full`
// predicate says so (Router.isFull: for up to 5 s after the primary refused an upgrade because the
// server is full, or while this worker holds 1.2 times its share of MAX_CONNECTIONS): new
// connections then pass at MAX_PENDING_HANDSHAKES / 2 per second, and the others are closed before
// the handshake. The primary has two checks. At the upgrade it refuses (HTTP 503, which starts the
// shedding) only beyond MAX_CONNECTIONS plus a reserve of max(16, 2 %), kept for the players whose
// game is in progress; at Hello it refuses newcomers at MAX_CONNECTIONS itself (ServerFull), which
// does not start the shedding. So a server at MAX_CONNECTIONS does not shed: each newcomer completes
// the handshake and the upgrade and gets ServerFull (scacelith_ws_hello_total{result="server_full"}),
// and a player coming back to a game in progress does not compete with newcomers for the shed rate.
// That 503 or ServerFull is the only way a client learns that the server is full (GET /info does
// not say it). On a shared API/WSS port the gate cannot tell an API request from an upgrade (both
// are inside TLS), so while it sheds the API is let through at that rate too, which keeps part of
// the API traffic working; with WS_PORT != API_PORT the API listener is never shed, the better
// layout for a server that expects to be full. Plain modes (proxy, off) have no gate: there is no
// TLS work to save.
//
// Requests (wrapApiHandler, every mode): the protection per address comes first, before the
// health endpoints and the API handler, for any path or method: a blocked address gets 429
// rate_limited with the time left (plus Connection: close, then the socket ends, except behind a
// proxy, whose connection is shared by many clients); then one token of HTTP_RATE_PER_IP and
// HTTP_RATE_PER_PREFIX, and a slot of IP_MAX_INFLIGHT requests in progress, given back when the
// response closes (finished or aborted); a pipelined response still queued when its socket closes
// gives it back then, or when its handler ends it (onCountedSocketClose). Beyond them: 429
// rate_limited with Retry-After. The WebSocket upgrade takes the same token in
// WsServer.handleUpgrade, before its IPC to the primary.
// In proxy mode the address is the X-Forwarded-For client and only these per-request checks exist:
// the connection limits are the proxy's job.
//
// Health: GET /api/v1/healthz and /api/v1/readyz (also /healthz, /readyz) are answered here,
// before the API handler, so they work whatever the API module does.
//
// Slow clients (hardenHttp; constants, shortened by the tests through the constructor):
// headersTimeout 10 s and requestTimeout 30 s, which Node checks every connectionsCheckingInterval
// (1 s here instead of Node's 30 s, so they hold to within a second: a slowloris client that sends
// a header line now and then is cut at 10-11 s); keepAliveTimeout 5 s; a socket with no byte in or
// out for IDLE_TIMEOUT_MS (30 s) is destroyed (a client that stops reading; upgraded sockets
// clear it), unless its request is still in the API handler, whose own timeout answers it (a GIF
// render or a data export may take longer); an answer that is not flushed to the kernel
// SEND_TIMEOUT_MS (60 s) after the handler ended it is destroyed with its socket (a client that
// reads a large answer, a GIF or a PGN, a few bytes at a time). Malformed HTTP ('clientError':
// 400, 408 for a header timeout, 431 for oversized headers) is counted in
// scacelith_http_client_errors_total{reason} and toward a block of the address, unless the peer
// is a trusted proxy, and its socket is closed once the answer is flushed (1 s at most); a client
// that ends or resets its connection in the middle of a request is closed without being counted.
//
// Privileged ports: API_PORT defaults to 443, and Linux lets only a process with the
// CAP_NET_BIND_SERVICE capability (root has it) bind a port below 1024
// (net.ipv4.ip_unprivileged_port_start, 1024 by default). A listen that fails with EACCES or
// EPERM on such a port is rethrown as a ListenError whose message names the fixes (listenHint):
// the systemd unit's AmbientCapabilities=CAP_NET_BIND_SERVICE, `setcap cap_net_bind_service=+ep`
// on the node binary, or the sysctl; the worker logs it and exits non-zero (worker-main.js), like
// any other listen failure (EADDRINUSE...), which keeps its own error.

import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import { performance } from 'node:perf_hooks';
import tls from 'node:tls';
import { defaultPendingPerGroup } from '../config.js';
import { metrics as defaultRegistry } from '../metrics.js';
import { ipGroupKey, ipMatcher, normalizeIp, resolveClientIp } from './ip.js';

const DEFAULT_BACKLOG = 2048;
/** TLS handshake timeout of the native listeners (Node's handshakeTimeout). */
export const HANDSHAKE_TIMEOUT_MS = 10000;
/** Time a new TLS connection has to send the first record of its ClientHello (TlsGate, before it takes a slot). */
export const HELLO_TIMEOUT_MS = 3000;
/** HTTP: time to receive the request headers (Node's headersTimeout). */
export const HEADERS_TIMEOUT_MS = 10000;
/** HTTP: time to receive the whole request (Node's requestTimeout). */
export const REQUEST_TIMEOUT_MS = 30000;
/** HTTP: an idle keep-alive connection is closed after this. */
export const KEEP_ALIVE_TIMEOUT_MS = 5000;
/** HTTP: a socket with no byte in or out for this long is destroyed (server.timeout), unless its request is in the handler. */
export const IDLE_TIMEOUT_MS = 30000;
/** HTTP: an answer still not flushed to the kernel this long after the handler ended it is destroyed. */
export const SEND_TIMEOUT_MS = 60000;
/** HTTP: how often Node checks headersTimeout and requestTimeout (its default is 30 s). */
export const CONNECTIONS_CHECK_MS = 1000;
/** Largest body of a TLS record (RFC 8446 section 5.1: 2^14 bytes). */
const MAX_RECORD_BODY = 16384;
const kSlot = Symbol('scacelith.tlsGateSlot');   // address group of a socket holding a handshake slot
const kWait = Symbol('scacelith.tlsGateWait');   // state of a socket waiting for its ClientHello
const kAdmitted = Symbol('scacelith.ipguardAdmitted');   // request already through admitRequest
const kInflight = Symbol('scacelith.ipguardInflight');   // keys of a response counted in progress
const kGuard = Symbol('scacelith.ipguard');
const kCounted = Symbol('scacelith.ipguardCounted');     // socket: its responses counted in progress (a Set)
const kCountedIn = Symbol('scacelith.ipguardCountedIn'); // response: that Set of its socket
const kSendTimer = Symbol('scacelith.sendDeadline');

// Default handshake slots per address group (MAX_PENDING_HANDSHAKES_PER_IP empty): config.js computes
// it, so that check-config prints the value the gate uses.
export { defaultPendingPerGroup };

function noop() {}

// Closes a refused socket with an RST: nothing is sent, and the server keeps no TIME_WAIT for it.
function refuseSocket(socket) {
    socket.on('error', noop);
    try { socket.resetAndDestroy(); } catch { socket.destroy(); }
}

/**
 * Admission of new TCP connections on the TLS listeners of one worker, before any TLS work (see
 * the header). O(1) per connection, plus a copy of the ClientHello.
 */
export class TlsGate {
    /**
     * @param {object} [o]
     * @param {number} [o.maxPending] handshakes in progress (MAX_PENDING_HANDSHAKES)
     * @param {number} [o.maxPendingPerIp] handshakes in progress per address group
     *   (MAX_PENDING_HANDSHAKES_PER_IP; default: defaultPendingPerGroup(maxPending))
     * @param {number} [o.maxWaiting] sockets waiting for their ClientHello (default: 16 * maxPending)
     * @param {number} [o.maxWaitingPerIp] of them per address group (default: 4 * maxPendingPerIp)
     * @param {number} [o.helloTimeoutMs] time a new socket has to send the first record of its ClientHello
     * @param {(() => boolean)|null} [o.full] true while the server is full (shedding on the upgrade listener)
     * @param {number} [o.fullRatePerSec] new connections let through per second while full
     * @param {() => number} [o.now] monotonic clock, in ms
     * @param {object} [o.registry] metrics registry
     * @param {import('./ipguard.js').IpGuard|null} [o.guard] protection per address (stage 0)
     */
    constructor({
        maxPending = 128, maxPendingPerIp = 0, maxWaiting = 0, maxWaitingPerIp = 0, helloTimeoutMs = HELLO_TIMEOUT_MS,
        full = null, fullRatePerSec = 0, now = () => performance.now(), registry = defaultRegistry, guard = null,
    } = {}) {
        this.maxPending = Math.max(1, Math.floor(maxPending) || 1);
        this.maxPendingPerIp = maxPendingPerIp >= 1 ? Math.floor(maxPendingPerIp) : defaultPendingPerGroup(this.maxPending);
        this.maxWaiting = maxWaiting >= 1 ? Math.floor(maxWaiting) : 16 * this.maxPending;
        this.maxWaitingPerIp = maxWaitingPerIp >= 1 ? Math.floor(maxWaitingPerIp) : 4 * this.maxPendingPerIp;
        this.helloTimeoutMs = helloTimeoutMs > 0 ? helloTimeoutMs : HELLO_TIMEOUT_MS;
        this.full = full;
        this.guard = guard;
        this.fullRate = fullRatePerSec > 0 ? fullRatePerSec : Math.max(1, Math.ceil(this.maxPending / 2));
        this.now = now;
        /** Handshakes in progress. */
        this.pending = 0;
        /** @type {Map<string, number>} address group -> handshakes in progress */
        this.perIp = new Map();
        /** Sockets waiting for their ClientHello. */
        this.waiting = 0;
        /** @type {Map<string, number>} address group -> sockets waiting for their ClientHello */
        this.waitingPerIp = new Map();
        this._tokens = this.fullRate;
        this._tokenAt = -Infinity;
        const r = registry;
        r.gaugeFn('scacelith_tls_handshakes_pending', 'TLS handshakes in progress', () => this.pending, { perShard: true });
        r.gaugeFn('scacelith_tls_hello_waiting', 'New TLS connections waiting for their ClientHello (no handshake slot yet)', () => this.waiting, { perShard: true });
        const refused = r.counter('scacelith_tls_refused_total', 'New connections closed before the TLS handshake, by reason', ['reason']);
        this._refusedBusy = refused.labels('handshakes');
        this._refusedIp = refused.labels('per_ip');
        this._refusedFull = refused.labels('server_full');
        this._refusedWaiting = refused.labels('waiting');
        this._refusedWaitingIp = refused.labels('waiting_per_ip');
        this._refusedTimeout = refused.labels('hello_timeout');
        this._refusedBadHello = refused.labels('bad_hello');
        this._refusedBy = { blocked: refused.labels('blocked'), conn_rate: refused.labels('conn_rate'), conn_open: refused.labels('conn_open') };
        const gate = this;
        this._onClose = function onGatedSocketClose() { gate.release(this); };
        this._onWaitData = function onWaitingData(chunk) { gate._waitData(this, chunk); };
        this._onWaitEnd = function onWaitingEnd() { gate._drop(this, null); };
        this._onWaitClose = function onWaitingClose() { gate._leave(this); };
        this._onWaitTimeout = (socket) => gate._drop(socket, gate._refusedTimeout, true);
    }

    /**
     * Puts the gate in front of a tls.Server or https.Server: its own 'connection' listener (Node's,
     * which starts the TLS work) only sees the sockets the gate admits, once their ClientHello
     * has arrived.
     * @param {tls.Server} server
     * @param {{ shed?: boolean }} [o] shed: this listener carries the WebSocket upgrade (server-full shedding)
     */
    attach(server, { shed = false } = {}) {
        const inner = server.listeners('connection');
        if (!inner.length) throw new Error('TlsGate: the TLS server has no connection listener');
        server.removeAllListeners('connection');
        const gate = this;
        const ready = (socket) => { for (const l of inner) l.call(server, socket); };
        server.on('connection', function gatedConnection(socket) { gate.accept(socket, shed, ready); });
        // A server-side TLSSocket keeps the raw socket it wraps in `_parent` (Node sets it for
        // every wrapped net.Socket); the unit tests check that completed and failed handshakes
        // give their slot back. 'tlsClientError' only comes before the handshake completes, and
        // Node does not close the socket after its handshake timeout: without the destroy, a
        // silent client would keep the socket (and a file descriptor) forever, and could still
        // complete the handshake later, outside the gate.
        server.on('secureConnection', (tlsSocket) => gate.release(tlsSocket?._parent));
        server.on('tlsClientError', (err, tlsSocket) => {
            const raw = tlsSocket?._parent;
            gate.release(raw);
            if (gate.guard !== null && raw) gate.guard.noteRefusal(gate.guard.socketKeys(raw), 1);
            tlsSocket?.destroy();
        });
    }

    /**
     * First stage for a new raw socket: it waits, without a handshake slot, until its first TLS
     * record (the start of its ClientHello, usually all of it) has arrived, then takes a slot
     * (admit) and is handed to `ready` with that record still unread. Returns false when the
     * socket was closed at once.
     * @param {import('node:net').Socket} socket
     * @param {boolean} shed apply the server-full shedding
     * @param {(socket: import('node:net').Socket) => void} ready
     */
    accept(socket, shed, ready) {
        let key;
        if (this.guard !== null) {
            const why = this.guard.connection(socket);
            if (why !== null) return this._refuse(socket, this._refusedBy[why]);
            const k = this.guard.socketKeys(socket);
            key = k.k48 ?? k.k64;               // what ipGroupKey(address, 48) gives
        } else {
            key = ipGroupKey(socket.remoteAddress, 48);
        }
        if (this.waiting >= this.maxWaiting) return this._refuse(socket, this._refusedWaiting);
        const n = this.waitingPerIp.get(key) || 0;
        if (n >= this.maxWaitingPerIp) return this._refuse(socket, this._refusedWaitingIp, true);
        this.waitingPerIp.set(key, n + 1);
        this.waiting++;
        socket[kWait] = {
            key, shed, ready, head: Buffer.alloc(5), record: null, need: 0, len: 0,
            timer: setTimeout(this._onWaitTimeout, this.helloTimeoutMs, socket),
        };
        socket.on('error', noop);
        socket.on('data', this._onWaitData);
        socket.on('end', this._onWaitEnd);
        socket.on('close', this._onWaitClose);
        return true;
    }

    // Collects the first record of a waiting socket: the 5-byte header, then the announced body.
    _waitData(socket, chunk) {
        const st = socket[kWait];
        if (!st) return;
        let off = 0;
        if (st.record === null) {
            off = Math.min(5 - st.len, chunk.length);
            chunk.copy(st.head, st.len, 0, off);
            st.len += off;
            if (st.len < 5) return;
            // TLSPlaintext: ContentType handshake (22), a 3.x record version, a body of 1 to 2^14
            // bytes (RFC 8446 section 5.1 forbids empty handshake fragments).
            const body = st.head.readUInt16BE(3);
            if (st.head[0] !== 22 || st.head[1] !== 3 || body < 1 || body > MAX_RECORD_BODY) return this._drop(socket, this._refusedBadHello, true);
            st.need = 5 + body;
            // A ClientHello is usually 0.3-2 KB: the room for the rest is only taken once the
            // client has sent it, so a header alone does not reserve 16 KB.
            st.record = Buffer.allocUnsafe(Math.min(st.need, 4096));
            st.head.copy(st.record, 0);
        }
        const n = Math.min(st.need - st.len, chunk.length - off);
        if (st.len + n > st.record.length) {
            const grown = Buffer.allocUnsafe(st.need);
            st.record.copy(grown, 0, 0, st.len);
            st.record = grown;
        }
        chunk.copy(st.record, st.len, off, off + n);
        st.len += n;
        off += n;
        if (st.len < st.need) return;
        // The record must start a ClientHello (HandshakeType 1). It need not hold all of it: a
        // client may fragment the ClientHello over several records (some tools that get around
        // censorship do). Requiring the whole message would protect nothing: an attacker can
        // replay a whole captured ClientHello and then stay silent, which holds the slot just as
        // long and costs the server more.
        if (st.record[5] !== 1) return this._drop(socket, this._refusedBadHello, true);
        this._leave(socket);
        if (!this.admit(socket, st.shed, st.key)) return;
        socket.pause();
        socket.unshift(off < chunk.length ? Buffer.concat([st.record, chunk.subarray(off)]) : st.record);
        st.ready(socket);
    }

    // Ends the waiting stage of a socket (idempotent). Returns its state, or null.
    _leave(socket) {
        const st = socket[kWait];
        if (!st) return null;
        socket[kWait] = null;
        clearTimeout(st.timer);
        socket.removeListener('data', this._onWaitData);
        socket.removeListener('end', this._onWaitEnd);
        socket.removeListener('close', this._onWaitClose);
        socket.removeListener('error', noop);
        this.waiting--;
        const n = this.waitingPerIp.get(st.key) || 0;
        if (n <= 1) this.waitingPerIp.delete(st.key); else this.waitingPerIp.set(st.key, n - 1);
        return st;
    }

    // Closes a waiting socket: with an RST, counted, when the gate gives up on it; quietly when
    // the client ended it first (counter null). perAddress: the refusal counts toward a block.
    _drop(socket, counter, perAddress = false) {
        if (!this._leave(socket)) return;
        if (counter === null) { socket.on('error', noop); socket.destroy(); return; }
        this._refuse(socket, counter, perAddress);
    }

    /**
     * Takes a handshake slot for a raw socket, or closes it (RST) and returns false.
     * @param {import('node:net').Socket} socket
     * @param {boolean} [shed] apply the server-full shedding
     * @param {string} [key] address group (IPv4 address, IPv6 /48)
     */
    admit(socket, shed = false, key = ipGroupKey(socket.remoteAddress, 48)) {
        if (this.pending >= this.maxPending) return this._refuse(socket, this._refusedBusy);
        const n = this.perIp.get(key) || 0;
        if (n >= this.maxPendingPerIp) return this._refuse(socket, this._refusedIp, true);
        if (shed && this.full !== null && this.full() && !this._takeFullToken()) return this._refuse(socket, this._refusedFull);
        this.perIp.set(key, n + 1);
        this.pending++;
        socket[kSlot] = key;
        socket.once('close', this._onClose);
        return true;
    }

    /** Gives back the slot of a socket (idempotent; unknown sockets are ignored). */
    release(socket) {
        const key = socket ? socket[kSlot] : undefined;
        if (key === undefined || key === null) return;
        socket[kSlot] = null;
        socket.removeListener('close', this._onClose);
        this.pending--;
        const n = this.perIp.get(key) || 0;
        if (n <= 1) this.perIp.delete(key); else this.perIp.set(key, n - 1);
    }

    // perAddress: a refusal that says something about the client's address (not a server-wide
    // cap), counted toward a block of that address.
    _refuse(socket, counter, perAddress = false) {
        counter.inc();
        if (perAddress && this.guard !== null) this.guard.noteRefusal(this.guard.socketKeys(socket), 1);
        refuseSocket(socket);
        return false;
    }

    // Token bucket of the connections let through while the server is full (burst: one second).
    // The clock is monotonic, and a clock that goes back anyway adds nothing rather than a debt.
    _takeFullToken() {
        const now = this.now();
        let t = this._tokens + Math.max(0, now - this._tokenAt) * this.fullRate / 1000;
        if (!(t <= this.fullRate)) t = this.fullRate;
        this._tokenAt = now;
        if (t < 1) { this._tokens = t; return false; }
        this._tokens = t - 1;
        return true;
    }
}

/** A listen failure with an actionable message (listenHint); `code`, `port` and `cause` kept. */
export class ListenError extends Error {
    constructor(message, { code, port, cause }) {
        super(message, { cause });
        this.name = 'ListenError';
        this.code = code;
        this.port = port;
    }
}

/**
 * The operator's way out of a listen failure, or null when the error has no specific advice: an
 * EACCES / EPERM on a port below 1024 (the process lacks CAP_NET_BIND_SERVICE).
 * @param {{ code?: string }} err the error of server.listen()
 * @param {number} port the port that was asked for
 * @param {string} [execPath] the node binary (for the setcap command)
 * @returns {string|null}
 */
export function listenHint(err, port, execPath = process.execPath) {
    const code = err && err.code;
    if ((code !== 'EACCES' && code !== 'EPERM') || !(port > 0 && port < 1024)) return null;
    return `Cannot listen on port ${port} (${code}): ports below 1024 need the CAP_NET_BIND_SERVICE capability. Either `
        + 'run the server with systemd and give the unit AmbientCapabilities=CAP_NET_BIND_SERVICE and '
        + 'CapabilityBoundingSet=CAP_NET_BIND_SERVICE (README, "Running as a service"); or give the node binary the '
        + `capability: sudo setcap cap_net_bind_service=+ep ${execPath} (again after each Node.js upgrade); or let `
        + `unprivileged processes bind it: sysctl -w net.ipv4.ip_unprivileged_port_start=${port} (and in /etc/sysctl.d/ to keep it); `
        + 'or choose a port of 1024 or above with API_PORT (and PUBLIC_API_PORT behind a port mapping).';
}

/**
 * The function giving the client address of a request, following the TLS mode.
 * @param {object} config
 * @returns {(req: {headers: object}, socket: {remoteAddress?: string}) => string}
 */
export function makeClientIp(config) {
    if (config.tlsMode !== 'proxy') return (req, socket) => normalizeIp(socket?.remoteAddress);
    const trusted = ipMatcher(config.trustedProxies || []);
    return (req, socket) => resolveClientIp(socket?.remoteAddress, req.headers['x-forwarded-for'], trusted);
}

/**
 * Reads the certificate and key files and builds the TLS options.
 * @param {object} config
 */
export function tlsOptions(config) {
    return {
        cert: fs.readFileSync(config.tlsCertFile),
        key: fs.readFileSync(config.tlsKeyFile),
        minVersion: config.tlsMinVersion || 'TLSv1.2',
        ciphers: tls.DEFAULT_CIPHERS,
        honorCipherOrder: true,
        ALPNProtocols: ['http/1.1'],
    };
}

/**
 * A small JSON answer with most of the API's security headers (no Cross-Origin-Resource-Policy;
 * the answers given before the API handler: health, refusals of the protection per address).
 * @param {import('node:http').ServerResponse} res
 * @param {number} status
 * @param {object} body
 * @param {boolean} native HTTPS served here (HSTS)
 * @param {Record<string, string>|null} [headers] extra headers
 */
export function sendJson(res, status, body, native, headers = null) {
    if (res.headersSent || res.writableEnded) return;
    const json = JSON.stringify(body);
    const h = {
        'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(json), 'Cache-Control': 'no-store',
        'X-Content-Type-Options': 'nosniff', 'Referrer-Policy': 'no-referrer', 'X-Frame-Options': 'DENY',
        'Content-Security-Policy': "default-src 'none'; frame-ancestors 'none'",
    };
    if (native) h['Strict-Transport-Security'] = 'max-age=31536000';
    if (headers) Object.assign(h, headers);
    res.writeHead(status, h);
    res.end(json);
}

// Gives back the slot of a response counted by admitRequest; once, whatever calls it first.
function releaseSlot(res) {
    const k = res[kInflight];
    if (k === undefined || k === null) return;
    res[kInflight] = null;
    const counted = res[kCountedIn];
    if (counted) { counted.delete(res); res[kCountedIn] = null; }
    res[kGuard].leave(k);
}

function onAdmittedClose() { releaseSlot(this); }

// Pipelined requests: Node gives the socket to one response at a time and queues the others
// (parsed and handed to the handler at once, answered in turn). A queued response whose socket
// closes before its turn never gets it and never emits 'close', so its slot would stay counted
// until the worker restarts (a few such connections, or one request without Host followed by
// pipelined ones, would lock the address out of the HTTP API). When the socket closes, each
// response still counted is released: at once when its handler has already ended it, else when
// the handler ends (or destroys) it, so that the slot keeps covering the work in progress (a
// password hash in the queue). Not on the request's 'close', which comes as soon as its body is
// read. The response that held the socket closes with it (onAdmittedClose); releaseSlot runs once.
function onCountedSocketClose() {
    const counted = this[kCounted];
    this[kCounted] = null;
    for (const res of counted) {
        res[kCountedIn] = null;
        if (res.writableEnded || res.destroyed) releaseSlot(res);
        else releaseOnEnd(res);
    }
    counted.clear();
}

function releaseOnEnd(res) {
    const { end, destroy } = res;
    res.end = function endAndRelease(...args) { releaseSlot(res); return end.apply(this, args); };
    if (typeof destroy === 'function') {
        res.destroy = function destroyAndRelease(...args) { releaseSlot(res); return destroy.apply(this, args); };
    }
}

function countOnSocket(socket, res) {
    if (!socket || typeof socket.once !== 'function') return;
    let counted = socket[kCounted];
    if (counted === null) { releaseOnEnd(res); return; }       // its socket already closed
    if (counted === undefined) {
        counted = new Set();
        socket[kCounted] = counted;
        socket.once('close', onCountedSocketClose);
    }
    counted.add(res);
    res[kCountedIn] = counted;
}

/**
 * The protection per address for one request (net/ipguard.js): block, budget, requests in
 * progress. Returns true when the request may go on; otherwise it has been answered 429
 * rate_limited with Retry-After (a blocked address also gets Connection: close when
 * `closeOnBlock`, so that its keep-alive connection ends after this answer). Runs once per request:
 * wrapApiHandler calls it, and createApiHandler again only for a request that did not come
 * through the wrapper (an API handler used alone).
 * @param {import('./ipguard.js').IpGuard} guard
 * @param {import('node:http').IncomingMessage} req `req.clientIp` when set (proxy aware), else the socket's address
 * @param {import('node:http').ServerResponse} res
 * @param {{ native?: boolean, closeOnBlock?: boolean }} [o]
 */
export function admitRequest(guard, req, res, { native = false, closeOnBlock = true } = {}) {
    if (req[kAdmitted] === true) return true;
    req[kAdmitted] = true;
    const ip = req.clientIp ?? normalizeIp(req.socket?.remoteAddress);
    const keys = guard.keysOf(ip, req.socket);
    if (keys.exempt) return true;
    const r = guard.request(keys);
    if (r === null) {
        if (guard.enter(keys)) {
            res[kGuard] = guard;
            res[kInflight] = keys;
            res.once('close', onAdmittedClose);
            countOnSocket(req.socket, res);
            return true;
        }
        refuseRequest(res, 1000, native, false);
        return false;
    }
    refuseRequest(res, r.retryAfterMs, native, r.reason === 'blocked' && closeOnBlock);
    return false;
}

function refuseRequest(res, retryAfterMs, native, close) {
    const s = Math.max(1, Math.ceil(retryAfterMs / 1000));
    const headers = { 'Retry-After': String(s) };
    // Node ends the socket once this answer is flushed (no further request is read on it).
    if (close) headers.Connection = 'close';
    sendJson(res, 429, { error: 'rate_limited', message: 'Too many requests; try again later.', retryAfter: s }, native, headers);
}

function destroyResponse(res) { res.destroy(); }
function destroySocket(socket) { socket.destroy(); }

// The inactivity timeout (server.timeout, IDLE_TIMEOUT_MS) is for a client that stops reading. A
// request whose handler is still at work (a GIF waiting for a render thread, a data export) keeps
// its socket: the handler's own timeout answers it. Node emits 'timeout' on the response of a
// socket that timed out and, once a listener is there, leaves the socket to it.
function onResponseIdle(socket) {
    if (!this.writableEnded) return;
    socket.destroy();
}

// 'prefinish' comes when the handler ends the answer; writableFinished is already true when
// every byte reached the kernel, so the deadline only costs a timer for the answers that wait.
function sendDeadline(ms) {
    function onClose() { clearTimeout(this[kSendTimer]); }
    return function armSendDeadline() {
        if (this.writableFinished || this.destroyed) return;
        const t = setTimeout(destroyResponse, ms, this);
        t.unref?.();
        this[kSendTimer] = t;
        this.once('close', onClose);
    };
}

/**
 * Wraps the API handler: sets `req.clientIp` (proxy-aware address), applies the protection per
 * address (admitRequest, when a guard is given) and the send deadline, and answers the health
 * endpoints.
 * @param {((req: object, res: object) => void) | null} apiHandler
 * @param {{ clientIp: Function, ready: () => boolean, native: boolean, guard?: import('./ipguard.js').IpGuard|null,
 *           closeOnBlock?: boolean, sendTimeoutMs?: number }} o closeOnBlock: false behind a proxy
 */
export function wrapApiHandler(apiHandler, { clientIp, ready, native, guard = null, closeOnBlock = true, sendTimeoutMs = SEND_TIMEOUT_MS }) {
    const armSendDeadline = sendDeadline(sendTimeoutMs);
    const admit = { native, closeOnBlock };
    return (req, res) => {
        req.clientIp = clientIp(req, req.socket);
        res.once('prefinish', armSendDeadline);
        res.on('timeout', onResponseIdle);
        if (guard !== null && !admitRequest(guard, req, res, admit)) return undefined;
        if (req.method === 'GET' || req.method === 'HEAD') {
            const q = req.url.indexOf('?');
            const p = q >= 0 ? req.url.slice(0, q) : req.url;
            if (p === '/api/v1/healthz' || p === '/healthz') return sendJson(res, 200, { status: 'ok' }, native);
            if (p === '/api/v1/readyz' || p === '/readyz') {
                const ok = ready();
                return sendJson(res, ok ? 200 : 503, { status: ok ? 'ready' : 'not_ready' }, native);
            }
        }
        if (!apiHandler) return sendJson(res, 404, { error: 'not_found', message: 'Not found' }, native);
        return apiHandler(req, res);
    };
}

const CLIENT_ERROR_STATUS = { timeout: '408 Request Timeout', too_large: '431 Request Header Fields Too Large', malformed: '400 Bad Request' };

// What a 'clientError' says about the client: null for a connection that simply went away (reset,
// or ended by the client in the middle of a request: HPE_INVALID_EOF_STATE).
function clientErrorReason(err) {
    const code = err && err.code;
    if (code === 'ECONNRESET' || code === 'EPIPE' || code === 'ECONNABORTED' || code === 'ETIMEDOUT') return null;
    if (code === 'HPE_INVALID_EOF_STATE') return null;
    if (code === 'ERR_HTTP_REQUEST_TIMEOUT') return 'timeout';
    if (code === 'HPE_HEADER_OVERFLOW' || code === 'HPE_CHUNK_EXTENSIONS_OVERFLOW') return 'too_large';
    return 'malformed';
}

function hardenHttp(server, { guard, isTrusted, clientErrors, headersTimeoutMs, requestTimeoutMs, idleTimeoutMs }) {
    server.headersTimeout = headersTimeoutMs;
    server.requestTimeout = requestTimeoutMs;
    server.keepAliveTimeout = KEEP_ALIVE_TIMEOUT_MS;
    server.timeout = idleTimeoutMs;
    server.maxHeadersCount = 64;
    server.maxRequestsPerSocket = 1000;
    server.on('clientError', (err, socket) => {
        const reason = clientErrorReason(err);
        if (reason !== null) {
            clientErrors[reason].inc();
            // Behind a proxy the peer is the proxy, not the client: nothing to attribute.
            if (guard !== null && !(isTrusted !== null && isTrusted(socket.remoteAddress))) {
                guard.noteRefusal(guard.keysOf(normalizeIp(socket.remoteAddress), socket), 1);
            }
        }
        if (reason !== null && socket.writable && !socket.destroyed) {
            socket.end(`HTTP/1.1 ${CLIENT_ERROR_STATUS[reason]}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`);
            // Ending is not enough: the parser stays attached to the socket (after a header timeout
            // Node no longer watches it, and a client that keeps sending would hold the socket and
            // could still complete its request). Closed once the answer is flushed, 1 s at most.
            socket.destroySoon();
            setTimeout(destroySocket, 1000, socket).unref();
        } else {
            socket.destroy();
        }
    });
}

/**
 * The listeners of one shard.
 */
export class Listeners {
    /**
     * @param {object} o
     * @param {object} o.config
     * @param {((req, res) => void)|null} o.apiHandler
     * @param {import('./ws.js').WsServer} o.wsServer
     * @param {object} [o.log]
     * @param {() => boolean} [o.ready] readiness for /readyz
     * @param {boolean} [o.reusePort] SO_REUSEPORT exclusive listen (defaults to LISTEN_REUSE_PORT on Linux)
     * @param {(() => boolean)|null} [o.full] true while the server is full (TlsGate shedding; Router.isFull)
     * @param {object} [o.registry] metrics registry
     * @param {number} [o.handshakeTimeoutMs] TLS handshake timeout (tests shorten it)
     * @param {number} [o.helloTimeoutMs] time a new TLS connection has to send its ClientHello (tests shorten it)
     * @param {import('./ipguard.js').IpGuard|null} [o.guard] protection per address (the shard's; also given
     *   to the WsServer and the API handler)
     * @param {number} [o.headersTimeoutMs] HTTP headers timeout (tests shorten it; likewise the next four)
     * @param {number} [o.requestTimeoutMs] whole-request timeout
     * @param {number} [o.idleTimeoutMs] socket inactivity timeout
     * @param {number} [o.sendTimeoutMs] send deadline of an answer
     * @param {number} [o.checkIntervalMs] Node's connectionsCheckingInterval
     * @param {import('./ticket-keys.js').TicketKeys|null} [o.ticketKeys] the session-ticket keys every
     *   worker shares (native mode; from the primary). Without them, each TLS server keeps Node's
     *   random keys and resumes only its own sessions.
     */
    constructor({
        config, apiHandler, wsServer, log = null, ready = () => true, reusePort, full = null, registry = defaultRegistry,
        handshakeTimeoutMs = HANDSHAKE_TIMEOUT_MS, helloTimeoutMs = HELLO_TIMEOUT_MS, guard = null,
        headersTimeoutMs = HEADERS_TIMEOUT_MS, requestTimeoutMs = REQUEST_TIMEOUT_MS, idleTimeoutMs = IDLE_TIMEOUT_MS,
        sendTimeoutMs = SEND_TIMEOUT_MS, checkIntervalMs = CONNECTIONS_CHECK_MS, ticketKeys = null,
    }) {
        this.config = config;
        this.log = log;
        this.wsServer = wsServer;
        this.native = config.tlsMode === 'native';
        this.clientIp = makeClientIp(config);
        this.reusePort = reusePort ?? (!!config.listenReusePort && process.platform === 'linux');
        this.backlog = Number.isInteger(config.listenBacklog) && config.listenBacklog > 0 ? config.listenBacklog : DEFAULT_BACKLOG;
        this.servers = [];            // [{ kind, server, port }]
        this._tlsServers = [];
        this._watchers = [];
        this.ticketKeys = ticketKeys;
        this._ticketDay = -1;
        this._ticketTimer = null;
        this._reloadTimer = null;
        /** @type {TlsGate|null} admission before TLS (native mode only) */
        this.gate = null;
        this.guard = guard;
        const proxy = config.tlsMode === 'proxy';
        const api = wrapApiHandler(apiHandler, { clientIp: this.clientIp, ready, native: this.native, guard, closeOnBlock: !proxy, sendTimeoutMs });
        const errs = registry.counter('scacelith_http_client_errors_total', 'Malformed HTTP requests closed before the API handler, by reason', ['reason']);
        const hardening = {
            guard, isTrusted: proxy ? ipMatcher(config.trustedProxies || []) : null,
            clientErrors: { timeout: errs.labels('timeout'), too_large: errs.labels('too_large'), malformed: errs.labels('malformed') },
            headersTimeoutMs, requestTimeoutMs, idleTimeoutMs,
        };
        // Node checks headersTimeout / requestTimeout only every connectionsCheckingInterval.
        const httpOpts = { maxHeaderSize: 8192, requestTimeout: requestTimeoutMs, connectionsCheckingInterval: checkIntervalMs };
        const shared = config.wsPort === config.apiPort;
        const onUpgrade = (req, socket, head) => wsServer.handleUpgrade(req, socket, head);

        if (this.native) {
            this.gate = new TlsGate({
                maxPending: config.maxPendingHandshakes ?? 128,
                maxPendingPerIp: config.maxPendingHandshakesPerIp ?? 0,      // loadConfig fills it; 0: the default share
                helloTimeoutMs,
                full,
                registry,
                guard,
            });
            const opts = { ...tlsOptions(config), handshakeTimeout: handshakeTimeoutMs, ...httpOpts };
            const apiServer = https.createServer(opts, api);
            hardenHttp(apiServer, hardening);
            if (shared) apiServer.on('upgrade', onUpgrade);
            this._addTls(apiServer, shared);
            this.servers.push({ kind: shared ? 'api+ws' : 'api', server: apiServer, port: config.apiPort });
            if (!shared) {
                const wss = tls.createServer({ ...tlsOptions(config), handshakeTimeout: handshakeTimeoutMs }, (s) => wsServer.handleSocket(s));
                this._addTls(wss, true);
                this.servers.push({ kind: 'ws', server: wss, port: config.wsPort });
            }
        } else {
            const apiServer = http.createServer(httpOpts, api);
            hardenHttp(apiServer, hardening);
            if (shared) apiServer.on('upgrade', onUpgrade);
            this.servers.push({ kind: shared ? 'api+ws' : 'api', server: apiServer, port: config.apiPort });
            if (!shared) {
                const wss = net.createServer({ noDelay: true }, (s) => wsServer.handleSocket(s));
                this.servers.push({ kind: 'ws', server: wss, port: config.wsPort });
            }
        }
        for (const { server } of this.servers) server.on('error', (e) => this.log?.error?.('listener error', { err: e }));
    }

    // shed: the listener carries the WebSocket upgrade.
    // Failed handshakes are not logged one by one; the gate gives their slot back and destroys them.
    _addTls(server, shed) {
        this._tlsServers.push(server);
        this.gate.attach(server, { shed });
    }

    /** Starts listening on every port. */
    async listen() {
        const host = this.config.bindAddress;
        await Promise.all(this.servers.map(({ server, port }) => new Promise((resolve, reject) => {
            const onErr = (e) => {
                const hint = listenHint(e, port);
                reject(hint ? new ListenError(hint, { code: e.code, port, cause: e }) : e);
            };
            server.once('error', onErr);
            const opts = { port, host, backlog: this.backlog };
            if (this.reusePort) { opts.exclusive = true; opts.reusePort = true; }
            server.listen(opts, () => { server.removeListener('error', onErr); resolve(); });
        })));
        if (this.native) {
            this._rotateTicketKeys();
            this._ticketTimer = setInterval(() => this._rotateTicketKeys(), 3600000);
            this._ticketTimer.unref();
            this._watchCertificates();
        }
    }

    /** The ports actually bound (useful with port 0 in tests). */
    addresses() {
        return this.servers.map(({ kind, server }) => ({ kind, port: server.address()?.port }));
    }

    _rotateTicketKeys(now = Date.now()) {
        if (!this.ticketKeys) return;
        const day = this.ticketKeys.advance(now);
        if (day === this._ticketDay) return;
        this._ticketDay = day;
        const keys = this.ticketKeys.ticketKeys(now);
        for (const s of this._tlsServers) s.setTicketKeys(keys);
        keys.fill(0);           // the TLS contexts keep their own copy
    }

    _watchCertificates() {
        const files = [this.config.tlsCertFile, this.config.tlsKeyFile].filter(Boolean);
        for (const f of files) {
            const listener = (curr, prev) => {
                if (curr.mtimeMs === prev.mtimeMs && curr.size === prev.size && curr.ino === prev.ino) return;
                clearTimeout(this._reloadTimer);
                this._reloadTimer = setTimeout(() => this.reloadCertificates(), 1000);
                this._reloadTimer.unref();
            };
            fs.watchFile(f, { interval: 10000, persistent: false }, listener);
            this._watchers.push([f, listener]);
        }
    }

    /**
     * Reloads the certificate and the key from disk. Returns false (and keeps the current ones)
     * when they cannot be read or do not match.
     */
    reloadCertificates() {
        if (!this.native) return false;
        let opts;
        try {
            opts = tlsOptions(this.config);
            tls.createSecureContext(opts);            // validates (throws on a key/cert mismatch)
        } catch (e) {
            this.log?.error?.('certificate reload failed, keeping the current certificate', { err: e });
            return false;
        }
        for (const s of this._tlsServers) s.setSecureContext(opts);
        // setSecureContext installs random ticket keys: put the day's shared ones back, or the
        // other workers could not resume this one's sessions until the next day.
        this._ticketDay = -1;
        this._rotateTicketKeys();
        this.log?.info?.('certificate reloaded');
        return true;
    }

    /**
     * Stops accepting new connections. Open connections (upgraded WebSockets included) are left
     * to their owners, so this does not wait for them; idle keep-alive API connections are
     * closed.
     */
    close() {
        clearInterval(this._ticketTimer);
        clearTimeout(this._reloadTimer);
        for (const [f, l] of this._watchers) fs.unwatchFile(f, l);
        this._watchers = [];
        for (const { server } of this.servers) {
            if (server.listening) server.close();
            if (typeof server.closeIdleConnections === 'function') server.closeIdleConnections();
        }
    }
}
