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
// TLSSocket from its own 'connection' listener. The gate runs in front of that listener and closes
// a new socket (RST, nothing sent) before any TLS work when this worker already has
// MAX_PENDING_HANDSHAKES handshakes in progress, or MAX_CONNECTIONS_PER_IP of them from the
// client's address group (so a few idle sockets from one host cannot hold every slot). A slot is
// held from the accept until 'secureConnection', 'tlsClientError' or the close of the socket
// (the handshake timeout, 10 s, bounds it). The CPU then serves a reconnection storm in turn
// (one handshake is 1-3.5 ms of CPU) instead of starting every handshake at once and finishing
// none before the clients' deadline; a refused client retries with its backoff. On the listener
// that carries the WebSocket upgrade, the gate also sheds load while the server is full (the
// `full` predicate, Router.isFull: the primary refused an upgrade for MAX_CONNECTIONS, or this
// worker holds 1.2 times its share): new connections then pass at MAX_PENDING_HANDSHAKES / 2 per
// second, enough for the API (GET /info tells a client the server is full) and for the exact
// check (HTTP 503 at the upgrade), and the others are closed before the handshake. On a shared
// API/WSS port the gate cannot tell an API request from an upgrade (both are inside TLS), so they
// share that rate; with WS_PORT != API_PORT the API listener is never shed. Plain modes (proxy,
// off) have no gate: there is no TLS work to save.
//
// Health: GET /api/v1/healthz and /api/v1/readyz (also /healthz, /readyz) are answered here,
// before the API handler, so they work whatever the API module does.

import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import tls from 'node:tls';
import { metrics as defaultRegistry } from '../metrics.js';
import { ipGroupKey, ipMatcher, normalizeIp, resolveClientIp } from './ip.js';

const DAY_MS = 86400000;
const DEFAULT_BACKLOG = 2048;
const kSlot = Symbol('scacelith.tlsGateSlot');   // address group of a socket holding a handshake slot

function noop() {}

// Closes a refused socket with an RST: nothing is sent, and the server keeps no TIME_WAIT for it.
function refuseSocket(socket) {
    socket.on('error', noop);
    try { socket.resetAndDestroy(); } catch { socket.destroy(); }
}

/**
 * Admission of new TCP connections on the TLS listeners of one worker, before any TLS work (see
 * the header). O(1) per connection.
 */
export class TlsGate {
    /**
     * @param {object} [o]
     * @param {number} [o.maxPending] handshakes in progress (MAX_PENDING_HANDSHAKES)
     * @param {number} [o.maxPendingPerIp] handshakes in progress per address group (IPv6: /64)
     * @param {(() => boolean)|null} [o.full] true while the server is full (shedding on the upgrade listener)
     * @param {number} [o.fullRatePerSec] new connections let through per second while full
     * @param {() => number} [o.now]
     * @param {object} [o.registry] metrics registry
     */
    constructor({ maxPending = 128, maxPendingPerIp = Infinity, full = null, fullRatePerSec = 0, now = Date.now, registry = defaultRegistry } = {}) {
        this.maxPending = Math.max(1, Math.floor(maxPending) || 1);
        this.maxPendingPerIp = Math.max(1, maxPendingPerIp || 1);
        this.full = full;
        this.fullRate = fullRatePerSec > 0 ? fullRatePerSec : Math.max(1, Math.ceil(this.maxPending / 2));
        this.now = now;
        /** Handshakes in progress. */
        this.pending = 0;
        /** @type {Map<string, number>} address group -> handshakes in progress */
        this.perIp = new Map();
        this._tokens = this.fullRate;
        this._tokenAt = -Infinity;
        const r = registry;
        r.gaugeFn('scacelith_tls_handshakes_pending', 'TLS handshakes in progress', () => this.pending, { perShard: true });
        const refused = r.counter('scacelith_tls_refused_total', 'New connections closed before the TLS handshake, by reason', ['reason']);
        this._refusedBusy = refused.labels('handshakes');
        this._refusedIp = refused.labels('per_ip');
        this._refusedFull = refused.labels('server_full');
        const gate = this;
        this._onClose = function onGatedSocketClose() { gate.release(this); };
    }

    /**
     * Puts the gate in front of a tls.Server or https.Server: its own 'connection' listener (Node's,
     * which starts the TLS work) only sees the sockets the gate admits.
     * @param {tls.Server} server
     * @param {{ shed?: boolean }} [o] shed: this listener carries the WebSocket upgrade (server-full shedding)
     */
    attach(server, { shed = false } = {}) {
        const inner = server.listeners('connection');
        if (!inner.length) throw new Error('TlsGate: the TLS server has no connection listener');
        server.removeAllListeners('connection');
        const gate = this;
        server.on('connection', function gatedConnection(socket) {
            if (!gate.admit(socket, shed)) return;
            for (const l of inner) l.call(this, socket);
        });
        // A server-side TLSSocket keeps the raw socket it wraps in `_parent` (Node sets it for
        // every wrapped net.Socket); the unit tests check that completed and failed handshakes
        // give their slot back.
        server.on('secureConnection', (tlsSocket) => gate.release(tlsSocket?._parent));
        server.on('tlsClientError', (err, tlsSocket) => gate.release(tlsSocket?._parent));
    }

    /**
     * Takes a handshake slot for a new raw socket, or closes it (RST) and returns false.
     * @param {import('node:net').Socket} socket
     * @param {boolean} [shed] apply the server-full shedding
     */
    admit(socket, shed = false) {
        if (this.pending >= this.maxPending) return this._refuse(socket, this._refusedBusy);
        const key = ipGroupKey(socket.remoteAddress);
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
    _takeFullToken() {
        const now = this.now();
        let t = this._tokens + (now - this._tokenAt) * this.fullRate / 1000;
        if (!(t <= this.fullRate)) t = this.fullRate;
        this._tokenAt = now;
        if (t < 1) { this._tokens = t; return false; }
        this._tokens = t - 1;
        return true;
    }
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
     */
    constructor({ config, apiHandler, wsServer, log = null, ready = () => true, reusePort, full = null, registry = defaultRegistry }) {
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
                maxPendingPerIp: config.maxConnectionsPerIp ?? 16,
                full,
                registry,
            });
            const opts = { ...tlsOptions(config), handshakeTimeout: 10000, maxHeaderSize: 8192, requestTimeout: 30000 };
            const apiServer = https.createServer(opts, api);
            hardenHttp(apiServer);
            if (shared) apiServer.on('upgrade', onUpgrade);
            this._addTls(apiServer, shared);
            this.servers.push({ kind: shared ? 'api+ws' : 'api', server: apiServer, port: config.apiPort });
            if (!shared) {
                const wss = tls.createServer({ ...tlsOptions(config), handshakeTimeout: 10000 }, (s) => wsServer.handleSocket(s));
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
    _addTls(server, shed) {
        this._tlsServers.push(server);
        server.on('tlsClientError', noop);        // failed handshakes: nothing to log per attempt
        this.gate.attach(server, { shed });
    }

    /** Starts listening on every port. */
    async listen() {
        const host = this.config.bindAddress;
        await Promise.all(this.servers.map(({ server, port }) => new Promise((resolve, reject) => {
            const onErr = (e) => reject(e);
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
