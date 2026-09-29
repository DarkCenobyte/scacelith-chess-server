// The HTTPS API request handler (docs/DESIGN.md section 5.9), mounted by the net module on the
// API listener (TLS, ports and upgrades are the listener's business).
//
//   const handle = createApiHandler({ config, store, auth, primary, anticheat, log, routes });
//   server.on('request', handle);
//
// Pipeline of a request: security headers -> URL checks -> /healthz, /readyz -> global per-IP
// token bucket (HTTP_RATE_PER_IP / min) -> route match (404, 405 + Allow, OPTIONS -> 204 + Allow,
// HEAD = GET without body) -> authentication (Authorization: Bearer sct_...) -> route rate limits
// (local token bucket, then the primary's `ratelimit.take` for `shared` limits; the password
// endpoints also limit each IPv6 /48 as a whole, see checkRates) -> body (JSON only
// for the API, form-urlencoded for HTML pages, HTTP_BODY_LIMIT enforced while streaming: 413;
// Content-Type checked: 415; body read timeout: 408) -> strict schema validation (400) -> handler
// (timeout: 503) -> JSON or HTML answer.
//
// No CORS: the API serves the game, not browsers; no Access-Control-* header is ever sent, so a
// web page cannot read an answer, and the JSON-only rule makes every cross-site write need a
// preflight that fails.
//
// Client address: `req.clientIp` when the listener set it (proxy mode, X-Forwarded-For from a
// trusted proxy), else the socket's remote address.
//
// Errors are `{ "error": "<snake_case>", "message": "...", "retryAfter"?: seconds, ...extra }`.

import { API_PREFIX, HttpError, Router, validate } from './router.js';
import { PAGE_CSP, renderMessage } from './pages/layout.js';
import { TokenBucketLimiter, ipKey, normalizeIp, prefixKey } from '../security/ratelimit.js';
import { ipForLog } from '../log.js';
import { metrics } from '../metrics.js';
import * as infoRoutes from './routes/info.js';
import * as authRoutes from './routes/auth.js';
import * as accountRoutes from './routes/account.js';
import * as ssoRoutes from './routes/sso.js';
import * as playerRoutes from './routes/players.js';
import * as reportRoutes from './routes/reports.js';

export { HttpError } from './router.js';

/** Route modules of the auth owner (the bootstrap passes the full list, other owners' included). */
export const DEFAULT_ROUTES = Object.freeze([infoRoutes, authRoutes, accountRoutes, ssoRoutes, playerRoutes, reportRoutes]);

const API_CSP = "default-src 'none'; frame-ancestors 'none'";
const BODY_METHODS = new Set(['POST', 'PUT', 'PATCH', 'DELETE']);
const MAX_URL = 4096;

const requestsTotal = metrics.counter('scacelith_http_requests_total', 'API requests', ['route', 'status']);
const requestMs = metrics.histogram('scacelith_http_request_duration_ms', 'API request duration', [2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000]);
const rateLimitedTotal = metrics.counter('scacelith_http_rate_limited_total', 'API requests refused by a rate limit', ['limit']);

function jsonError(status, code, message, extra) { return new HttpError(status, code, message, extra); }

function retryAfterSec(ms) { return Math.max(1, Math.ceil(ms / 1000)); }

function rateLimited(ms, label) {
    rateLimitedTotal.labels(label).inc();
    const s = retryAfterSec(ms);
    return new HttpError(429, 'rate_limited', 'Too many requests; try again later.', { retryAfter: s });
}

/**
 * Reads a request body, enforcing `limit` bytes while streaming and a read timeout.
 * @returns {Promise<Buffer>}
 */
function readBody(req, limit, timeoutMs) {
    const cl = req.headers['content-length'];
    if (cl !== undefined) {
        if (!/^\d{1,15}$/.test(cl)) return Promise.reject(Object.assign(jsonError(400, 'invalid_request', 'Invalid Content-Length.'), { closeConnection: true }));
        if (+cl > limit) return Promise.reject(Object.assign(jsonError(413, 'payload_too_large', `The body must not exceed ${limit} bytes.`), { closeConnection: true }));
    }
    return new Promise((resolve, reject) => {
        const chunks = [];
        let size = 0, done = false;
        const finish = (err, buf) => {
            if (done) return;
            done = true;
            clearTimeout(timer);
            if (err) reject(err); else resolve(buf);
        };
        const timer = setTimeout(() => finish(Object.assign(jsonError(408, 'request_timeout', 'The request body took too long.'), { closeConnection: true })), timeoutMs);
        timer.unref?.();
        req.on('data', (c) => {
            if (done) return;          // draining after an error
            size += c.length;
            if (size > limit) { finish(Object.assign(jsonError(413, 'payload_too_large', `The body must not exceed ${limit} bytes.`), { closeConnection: true })); return; }
            chunks.push(c);
        });
        req.on('end', () => finish(null, Buffer.concat(chunks, size)));
        req.on('error', () => finish(Object.assign(jsonError(400, 'invalid_request', 'The request was aborted.'), { closeConnection: true })));
    });
}

function mediaType(req) {
    const ct = req.headers['content-type'];
    if (!ct) return { type: '', charset: '' };
    const [type, ...params] = ct.split(';');
    let charset = '';
    for (const p of params) {
        const m = /^\s*charset\s*=\s*"?([^";\s]+)"?\s*$/i.exec(p);
        if (m) charset = m[1].toLowerCase();
    }
    return { type: type.trim().toLowerCase(), charset };
}

function parseForm(buf) {
    const out = {};
    const sp = new URLSearchParams(buf.toString('utf8'));
    for (const [k, v] of sp) {
        if (Object.prototype.hasOwnProperty.call(out, k)) throw jsonError(400, 'invalid_request', `field "${k}" given twice`);
        out[k] = v;
    }
    return out;
}

function parseQuery(search) {
    const out = {};
    for (const [k, v] of new URLSearchParams(search)) if (!Object.prototype.hasOwnProperty.call(out, k)) out[k] = v;
    return out;
}

/**
 * Creates the API request handler.
 * @param {{ config: object, store: object, auth: object, primary?: { request(type: string, payload: object): Promise<object> }|null,
 *           anticheat?: object|null, log: object, routes?: Array<{ register: Function }|Function>,
 *           now?: () => number, ready?: () => boolean, bodyTimeoutMs?: number, handlerTimeoutMs?: number,
 *           deps?: object }} opts `deps` adds fields to what route modules receive.
 * @returns {((req: import('node:http').IncomingMessage, res: import('node:http').ServerResponse) => Promise<void>) & { router: Router }}
 */
export function createApiHandler({ config, store, auth, primary = null, anticheat = null, log, routes = DEFAULT_ROUTES,
    now = Date.now, ready = () => true, bodyTimeoutMs = 10000, handlerTimeoutMs = 30000, deps: extraDeps = {} }) {
    const router = new Router();
    const deps = { config, store, auth, primary, anticheat, log, now, ...extraDeps };
    for (const r of routes) {
        const reg = typeof r === 'function' ? r : r && r.register;
        if (typeof reg !== 'function') throw new Error('createApiHandler: a route module has no register(router, deps)');
        reg(router, deps);
    }
    const limiter = new TokenBucketLimiter({ now });
    const labelCache = new Map();
    const hsts = config.tlsMode === 'native';
    let primaryWarnAt = 0;

    function setBaseHeaders(res) {
        res.setHeader('Cache-Control', 'no-store');
        res.setHeader('X-Content-Type-Options', 'nosniff');
        res.setHeader('Referrer-Policy', 'no-referrer');
        res.setHeader('X-Frame-Options', 'DENY');
        res.setHeader('Cross-Origin-Resource-Policy', 'same-origin');
        if (hsts) res.setHeader('Strict-Transport-Security', 'max-age=31536000');
    }

    function send(req, res, status, { body, html, headers } = {}) {
        if (res.headersSent || res.writableEnded) return;
        let payload = '';
        if (html !== undefined) {
            res.setHeader('Content-Type', 'text/html; charset=utf-8');
            res.setHeader('Content-Security-Policy', PAGE_CSP);
            payload = html;
        } else {
            res.setHeader('Content-Security-Policy', API_CSP);
            if (body !== undefined && status !== 204) {
                res.setHeader('Content-Type', 'application/json; charset=utf-8');
                payload = JSON.stringify(body);
            }
        }
        if (headers) for (const [k, v] of Object.entries(headers)) res.setHeader(k, v);
        const buf = Buffer.from(payload, 'utf8');
        if (status !== 204) res.setHeader('Content-Length', buf.length);
        res.writeHead(status);
        res.end(req.method === 'HEAD' || status === 204 ? undefined : buf);
    }

    function sendError(req, res, err, page) {
        let status = 500, code = 'internal_error', message = 'Internal server error.', extra = null, headers = null;
        if (err && err.expose && Number.isInteger(err.status)) {
            status = err.status; code = err.code; message = err.message; extra = err.extra; headers = err.headers;
        }
        const h = { ...(headers || {}) };
        if (extra && extra.retryAfter) h['Retry-After'] = String(extra.retryAfter);
        if (err && err.closeConnection) {
            h.Connection = 'close';
            // Keep draining what the client still sends for a moment so that it can read the
            // answer, then drop the connection.
            res.once('finish', () => { const t = setTimeout(() => req.destroy(), 2000); t.unref?.(); });
        }
        if (page) {
            send(req, res, status, { html: renderMessage({ serverName: config.serverName, title: status >= 500 ? 'Server error' : 'Request refused', message, tone: 'error' }), headers: h });
        } else {
            send(req, res, status, { body: { error: code, message, ...(extra || {}) }, headers: h });
        }
    }

    async function authenticate(req, mode) {
        if (mode === 'none') return null;
        const h = req.headers.authorization;
        if (!h) {
            if (mode === 'required') throw new HttpError(401, 'unauthorized', 'Log in first.', null, { 'WWW-Authenticate': 'Bearer realm="scacelith"' });
            return null;
        }
        const m = /^Bearer ([\x21-\x7e]{1,512})$/.exec(h);
        const v = m ? await auth.validateToken(m[1]) : null;
        if (!v) throw new HttpError(401, 'invalid_token', 'The session is invalid or has expired; log in again.', null, { 'WWW-Authenticate': 'Bearer realm="scacelith", error="invalid_token"' });
        return v;
    }

    /** Takes one token of `key` (local bucket, then the primary's shared window for `shared` rates). */
    async function take(rate, key, limit, label) {
        const local = limiter.take(key, limit, rate.windowMs, 1);
        if (!local.allowed) throw rateLimited(local.retryAfterMs, label);
        if (rate.shared && primary) {
            let r = null;
            try {
                r = await primary.request('ratelimit.take', { key, limit, windowMs: rate.windowMs, cost: 1 });
            } catch (err) {
                // Fail open on the shared stage: the local bucket above still applies.
                const t = now();
                if (t - primaryWarnAt > 60000) { primaryWarnAt = t; log.warn('shared rate limit unavailable', { err: { message: err.message } }); }
            }
            if (r && r.allowed === false) throw rateLimited(r.retryAfterMs || 1000, label);
        }
    }

    // A rate { key, limit, windowMs, shared?, by?: 'user', prefixLimit? } takes from the bucket of
    // the user or of the client's ipKey() (an IPv4 address or an IPv6 /64). With `prefixLimit`, a
    // client on IPv6 also takes from the bucket of its /48 (prefixKey()), which bounds the 65536
    // /64 networks of one site together: without it, one /48 or /56 would multiply the limit by
    // the number of its /64s.
    async function checkRates(rates, ctx) {
        for (const rate of rates) {
            const byUser = rate.by === 'user' && ctx.user;
            await take(rate, `${rate.key}:${byUser ? `u${ctx.user.userId}` : ipKey(ctx.ip)}`, rate.limit, rate.key);
            if (rate.prefixLimit && !byUser && ctx.ip.includes(':')) {
                await take(rate, `${rate.key}/48:${prefixKey(ctx.ip)}`, rate.prefixLimit, `${rate.key}/48`);
            }
        }
    }

    function routeLabel(route) {
        let l = labelCache.get(route);
        if (!l) { l = `${route.method} ${route.path}`; labelCache.set(route, l); }
        return l;
    }

    async function runHandler(route, ctx) {
        const timeoutMs = route.opts.timeoutMs || handlerTimeoutMs;
        let timer;
        const timeout = new Promise((_, reject) => {
            timer = setTimeout(() => reject(jsonError(503, 'timeout', 'The server took too long to answer; try again.')), timeoutMs);
            timer.unref?.();
        });
        try {
            return await Promise.race([Promise.resolve().then(() => route.handler(ctx)), timeout]);
        } finally {
            clearTimeout(timer);
        }
    }

    async function handle(req, res) {
        const started = performance.now();
        let label = 'unmatched';
        let page = false;
        setBaseHeaders(res);
        const ip = normalizeIp(req.clientIp ?? req.socket?.remoteAddress ?? '');
        try {
            const method = String(req.method || '').toUpperCase();
            const rawUrl = String(req.url || '');
            if (rawUrl.length > MAX_URL) throw jsonError(414, 'uri_too_long', 'The URL is too long.');
            if (!rawUrl.startsWith('/') || rawUrl.startsWith('//')) throw jsonError(400, 'invalid_request', 'Invalid request target.');
            const q = rawUrl.indexOf('?');
            const pathname = q < 0 ? rawUrl : rawUrl.slice(0, q);
            const search = q < 0 ? '' : rawUrl.slice(q + 1);

            if (pathname === '/healthz' || pathname === API_PREFIX + '/healthz') {
                label = 'healthz';
                if (method !== 'GET' && method !== 'HEAD') throw new HttpError(405, 'method_not_allowed', 'Method not allowed.', null, { Allow: 'GET, HEAD' });
                send(req, res, 200, { body: { status: 'ok' } });
                return;
            }
            if (pathname === '/readyz' || pathname === API_PREFIX + '/readyz') {
                label = 'readyz';
                if (method !== 'GET' && method !== 'HEAD') throw new HttpError(405, 'method_not_allowed', 'Method not allowed.', null, { Allow: 'GET, HEAD' });
                if (ready()) send(req, res, 200, { body: { status: 'ready' } });
                else send(req, res, 503, { body: { error: 'not_ready', message: 'The server is starting or stopping.' } });
                return;
            }

            const global = limiter.take(`api:${ipKey(ip)}`, config.httpRatePerIp, 60000, 1);
            if (!global.allowed) throw rateLimited(global.retryAfterMs, 'api');

            const found = router.match(method, pathname);
            if (!found) throw jsonError(404, 'not_found', 'No such endpoint.');
            if (!found.route) {
                const allow = [...new Set([...found.methods, ...(found.methods.includes('GET') ? ['HEAD'] : []), 'OPTIONS'])].join(', ');
                if (method === 'OPTIONS') { send(req, res, 204, { headers: { Allow: allow } }); return; }
                throw new HttpError(405, 'method_not_allowed', 'Method not allowed.', null, { Allow: allow });
            }
            const { route, params } = found;
            label = routeLabel(route);
            page = route.opts.page;

            const user = await authenticate(req, route.opts.auth);
            const ctx = {
                req, res, ip, params, query: parseQuery(search), body: {},
                user: user ? { id: user.userId, userId: user.userId, username: user.username, emailVerified: user.emailVerified } : null,
                session: user ? { id: user.sessionId, tokenHash: user.tokenHash ?? null } : null,
                config, store, log, primary, auth, anticheat, now: now(), route: route.path,
            };
            const rates = route.opts.rate ? (Array.isArray(route.opts.rate) ? route.opts.rate : [route.opts.rate]) : [];
            if (rates.length) await checkRates(rates, ctx);

            if (route.opts.query) {
                const v = validate(route.opts.query, ctx.query);
                if (!v.ok) throw jsonError(400, 'invalid_request', v.message, { field: v.field });
                ctx.query = v.value;
            }

            if (BODY_METHODS.has(method)) {
                const buf = await readBody(req, config.httpBodyLimit, bodyTimeoutMs);
                let parsed = {};
                if (buf.length) {
                    const mt = mediaType(req);
                    if (mt.charset && mt.charset !== 'utf-8' && mt.charset !== 'utf8') throw jsonError(415, 'unsupported_media_type', 'Only UTF-8 is accepted.');
                    if (page && mt.type === 'application/x-www-form-urlencoded') {
                        parsed = parseForm(buf);
                    } else if (mt.type === 'application/json') {
                        try { parsed = JSON.parse(buf.toString('utf8')); } catch { throw jsonError(400, 'invalid_json', 'The body is not valid JSON.'); }
                    } else {
                        throw jsonError(415, 'unsupported_media_type', page ? 'Form data expected.' : 'Content-Type must be application/json.');
                    }
                }
                const v = validate(route.opts.body || {}, parsed);
                if (!v.ok) throw jsonError(400, 'invalid_request', v.message, v.field ? { field: v.field } : undefined);
                ctx.body = v.value;
            }

            const out = await runHandler(route, ctx);
            if (out === undefined || out === null) { send(req, res, 204); return; }
            const status = out.status || 200;
            if (out.html !== undefined) send(req, res, status, { html: out.html, headers: out.headers });
            else send(req, res, status, { body: out.body, headers: out.headers });
        } catch (err) {
            if (!(err && err.expose)) log.error('request failed', { route: label, err });
            sendError(req, res, err, page);
        } finally {
            const code = res.statusCode;
            requestsTotal.labels(label, String(code)).inc();
            const ms = performance.now() - started;
            requestMs.observe(ms);
            if (log.debugEnabled) log.debug('request', { route: label, status: code, ms: Math.round(ms), ip: ipForLog(ip) });
        }
    }

    handle.router = router;
    return handle;
}
