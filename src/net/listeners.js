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
// http/1.1, session tickets on. Ticket keys are derived from SERVER_SECRET (HKDF, rotated daily)
// so that every worker can resume every other worker's sessions: a reconnecting client usually
// skips the full handshake whichever worker the kernel hands it to. Certificates are reloaded
// without a restart on SIGHUP (the primary forwards it as the 'tls.reload' IPC message) and when
// the files change (stat polling, which also follows certbot's symlink swaps); a broken new
// certificate is refused and the old one stays in use.
//
// Cluster: by default workers listen through the cluster module (the primary accepts and hands
// connections out round-robin). LISTEN_REUSE_PORT=true on Linux makes each worker bind its own
// socket with SO_REUSEPORT (exclusive listen) and lets the kernel spread connections; see
// tools/ws-echo-bench.js --cluster for the comparison. Every listen uses LISTEN_BACKLOG (the
// kernel caps it at net.core.somaxconn); the round-robin handle of the primary honours it too.
//
// Admission before TLS (native mode; TlsGate): a TLS server wraps each accepted TCP socket in a
// TLSSocket from its own 'connection' listener. The gate runs in front of that listener, in two
// stages, and counts every socket it closes in scacelith_tls_refused_total{reason}:
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
// docs/DESIGN.md 5.8 for what remains possible.
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
// Health: GET /api/v1/healthz and /api/v1/readyz (also /healthz, /readyz) are answered here,
// before the API handler, so they work whatever the API module does.
//
// Privileged ports: API_PORT defaults to 443, and Linux lets only a process with the
// CAP_NET_BIND_SERVICE capability (root has it) bind a port below 1024
// (net.ipv4.ip_unprivileged_port_start, 1024 by default). A listen that fails with EACCES or
// EPERM on such a port is rethrown as a ListenError whose message names the fixes (listenHint):
// the systemd unit's AmbientCapabilities=CAP_NET_BIND_SERVICE, `setcap cap_net_bind_service=+ep`
// on the node binary, or the sysctl; the worker logs it and exits non-zero (worker-main.js), like
// any other listen failure (EADDRINUSE...), which keeps its own error.

import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import { performance } from 'node:perf_hooks';
import tls from 'node:tls';
import { defaultPendingPerGroup } from '../config.js';
import { metrics as defaultRegistry } from '../metrics.js';
import { ipGroupKey, ipMatcher, normalizeIp, resolveClientIp } from './ip.js';

const DAY_MS = 86400000;
const DEFAULT_BACKLOG = 2048;
/** TLS handshake timeout of the native listeners (Node's handshakeTimeout). */
export const HANDSHAKE_TIMEOUT_MS = 10000;
/** Time a new TLS connection has to send the first record of its ClientHello (TlsGate, before it takes a slot). */
export const HELLO_TIMEOUT_MS = 3000;
/** Largest body of a TLS record (RFC 8446 section 5.1: 2^14 bytes). */
const MAX_RECORD_BODY = 16384;
const kSlot = Symbol('scacelith.tlsGateSlot');   // address group of a socket holding a handshake slot
const kWait = Symbol('scacelith.tlsGateWait');   // state of a socket waiting for its ClientHello

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
     */
    constructor({
        maxPending = 128, maxPendingPerIp = 0, maxWaiting = 0, maxWaitingPerIp = 0, helloTimeoutMs = HELLO_TIMEOUT_MS,
        full = null, fullRatePerSec = 0, now = () => performance.now(), registry = defaultRegistry,
    } = {}) {
        this.maxPending = Math.max(1, Math.floor(maxPending) || 1);
        this.maxPendingPerIp = maxPendingPerIp >= 1 ? Math.floor(maxPendingPerIp) : defaultPendingPerGroup(this.maxPending);
        this.maxWaiting = maxWaiting >= 1 ? Math.floor(maxWaiting) : 16 * this.maxPending;
        this.maxWaitingPerIp = maxWaitingPerIp >= 1 ? Math.floor(maxWaitingPerIp) : 4 * this.maxPendingPerIp;
        this.helloTimeoutMs = helloTimeoutMs > 0 ? helloTimeoutMs : HELLO_TIMEOUT_MS;
        this.full = full;
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
        const gate = this;
        this._onClose = function onGatedSocketClose() { gate.release(this); };
        this._onWaitData = function onWaitingData(chunk) { gate._waitData(this, chunk); };
        this._onWaitEnd = function onWaitingEnd() { gate._drop(this, null); };
        this._onWaitClose = function onWaitingClose() { gate._leave(this); };
        this._onWaitTimeout = (socket) => gate._drop(socket, gate._refusedTimeout);
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
            gate.release(tlsSocket?._parent);
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
        if (this.waiting >= this.maxWaiting) return this._refuse(socket, this._refusedWaiting);
        const key = ipGroupKey(socket.remoteAddress, 48);
        const n = this.waitingPerIp.get(key) || 0;
        if (n >= this.maxWaitingPerIp) return this._refuse(socket, this._refusedWaitingIp);
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
            if (st.head[0] !== 22 || st.head[1] !== 3 || body < 1 || body > MAX_RECORD_BODY) return this._drop(socket, this._refusedBadHello);
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
        if (st.record[5] !== 1) return this._drop(socket, this._refusedBadHello);
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
    // the client ended it first (counter null).
    _drop(socket, counter) {
        if (!this._leave(socket)) return;
        if (counter === null) { socket.on('error', noop); socket.destroy(); return; }
        this._refuse(socket, counter);
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
        if (n >= this.maxPendingPerIp) return this._refuse(socket, this._refusedIp);
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

    _refuse(socket, counter) {
        counter.inc();
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
 * TLS session-ticket keys (48 bytes) for a given day, derived from the server secret.
 * @param {Buffer} secret
 * @param {number} day days since the epoch
 */
export function deriveTicketKeys(secret, day) {
    return Buffer.from(crypto.hkdfSync('sha256', secret, 'scacelith-tls-tickets', `day:${day}`, 48));
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

function sendJson(res, status, body, native) {
    const json = JSON.stringify(body);
    const headers = {
        'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(json), 'Cache-Control': 'no-store',
        'X-Content-Type-Options': 'nosniff', 'Referrer-Policy': 'no-referrer',
    };
    if (native) headers['Strict-Transport-Security'] = 'max-age=31536000';
    res.writeHead(status, headers);
    res.end(json);
}

/**
 * Wraps the API handler: sets `req.clientIp` (proxy-aware address) and answers the health
 * endpoints.
 * @param {((req: object, res: object) => void) | null} apiHandler
 * @param {{ clientIp: Function, ready: () => boolean, native: boolean }} o
 */
export function wrapApiHandler(apiHandler, { clientIp, ready, native }) {
    return (req, res) => {
        req.clientIp = clientIp(req, req.socket);
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

function hardenHttp(server) {
    server.headersTimeout = 10000;
    server.requestTimeout = 30000;
    server.keepAliveTimeout = 5000;
    server.maxHeadersCount = 64;
    server.maxRequestsPerSocket = 1000;
    server.on('clientError', (err, socket) => {
        if (socket.writable && !socket.destroyed) socket.end('HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n');
        else socket.destroy();
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
     */
    constructor({
        config, apiHandler, wsServer, log = null, ready = () => true, reusePort, full = null, registry = defaultRegistry,
        handshakeTimeoutMs = HANDSHAKE_TIMEOUT_MS, helloTimeoutMs = HELLO_TIMEOUT_MS,
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
        this._ticketDay = -1;
        this._ticketTimer = null;
        this._reloadTimer = null;
        /** @type {TlsGate|null} admission before TLS (native mode only) */
        this.gate = null;
        const api = wrapApiHandler(apiHandler, { clientIp: this.clientIp, ready, native: this.native });
        const shared = config.wsPort === config.apiPort;
        const onUpgrade = (req, socket, head) => wsServer.handleUpgrade(req, socket, head);

        if (this.native) {
            this.gate = new TlsGate({
                maxPending: config.maxPendingHandshakes ?? 128,
                maxPendingPerIp: config.maxPendingHandshakesPerIp ?? 0,      // loadConfig fills it; 0: the default share
                helloTimeoutMs,
                full,
                registry,
            });
            const opts = { ...tlsOptions(config), handshakeTimeout: handshakeTimeoutMs, maxHeaderSize: 8192, requestTimeout: 30000 };
            const apiServer = https.createServer(opts, api);
            hardenHttp(apiServer);
            if (shared) apiServer.on('upgrade', onUpgrade);
            this._addTls(apiServer, shared);
            this.servers.push({ kind: shared ? 'api+ws' : 'api', server: apiServer, port: config.apiPort });
            if (!shared) {
                const wss = tls.createServer({ ...tlsOptions(config), handshakeTimeout: handshakeTimeoutMs }, (s) => wsServer.handleSocket(s));
                this._addTls(wss, true);
                this.servers.push({ kind: 'ws', server: wss, port: config.wsPort });
            }
        } else {
            const apiServer = http.createServer({ maxHeaderSize: 8192, requestTimeout: 30000 }, api);
            hardenHttp(apiServer);
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
        const day = Math.floor(now / DAY_MS);
        if (day === this._ticketDay || !this.config.serverSecret) return;
        this._ticketDay = day;
        const keys = deriveTicketKeys(this.config.serverSecret, day);
        for (const s of this._tlsServers) s.setTicketKeys(keys);
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
