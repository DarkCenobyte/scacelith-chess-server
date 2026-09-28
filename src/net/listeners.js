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
// tools/ws-echo-bench.js --cluster for the comparison.
//
// Health: GET /api/v1/healthz and /api/v1/readyz (also /healthz, /readyz) are answered here,
// before the API handler, so they work whatever the API module does.

import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import tls from 'node:tls';
import { ipMatcher, normalizeIp, resolveClientIp } from './ip.js';

const DAY_MS = 86400000;

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
     */
    constructor({ config, apiHandler, wsServer, log = null, ready = () => true, reusePort }) {
        this.config = config;
        this.log = log;
        this.wsServer = wsServer;
        this.native = config.tlsMode === 'native';
        this.clientIp = makeClientIp(config);
        this.reusePort = reusePort ?? (!!config.listenReusePort && process.platform === 'linux');
        this.servers = [];            // [{ kind, server, port }]
        this._tlsServers = [];
        this._watchers = [];
        this._ticketDay = -1;
        this._ticketTimer = null;
        this._reloadTimer = null;
        const api = wrapApiHandler(apiHandler, { clientIp: this.clientIp, ready, native: this.native });
        const shared = config.wsPort === config.apiPort;
        const onUpgrade = (req, socket, head) => wsServer.handleUpgrade(req, socket, head);

        if (this.native) {
            const opts = { ...tlsOptions(config), handshakeTimeout: 10000, maxHeaderSize: 8192, requestTimeout: 30000 };
            const apiServer = https.createServer(opts, api);
            hardenHttp(apiServer);
            if (shared) apiServer.on('upgrade', onUpgrade);
            this._addTls(apiServer);
            this.servers.push({ kind: shared ? 'api+ws' : 'api', server: apiServer, port: config.apiPort });
            if (!shared) {
                const wss = tls.createServer({ ...tlsOptions(config), handshakeTimeout: 10000 }, (s) => wsServer.handleSocket(s));
                wss.on('tlsClientError', () => { /* failed handshakes: nothing to log per attempt */ });
                this._addTls(wss);
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

    _addTls(server) {
        this._tlsServers.push(server);
        server.on('tlsClientError', () => {});
    }

    /** Starts listening on every port. */
    async listen() {
        const host = this.config.bindAddress;
        await Promise.all(this.servers.map(({ server, port }) => new Promise((resolve, reject) => {
            const onErr = (e) => reject(e);
            server.once('error', onErr);
            const opts = { port, host };
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
