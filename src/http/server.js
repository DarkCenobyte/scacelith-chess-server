// The HTTPS API request handler (docs/DESIGN.md section 5.9), mounted by the net module on the
// API listener (TLS, ports and upgrades are the listener's business).
//
//   const handle = createApiHandler({ config, store, auth, primary, anticheat, log, routes });
//   server.on('request', handle);
//
// Pipeline of a request: protection per address (net/ipguard.js, applied by the listener's
// wrapApiHandler before anything else, health checks included, or here first when the handler is
// used alone with a `guard`: block, HTTP_RATE_PER_IP / HTTP_RATE_PER_PREFIX budget, IP_MAX_INFLIGHT
// requests in progress; 429 rate_limited) -> security headers -> URL checks -> /healthz, /readyz
// -> route match (404, 405 + Allow, OPTIONS -> 204 + Allow, HEAD = GET without body) ->
// authentication (Authorization: Bearer sct_...) -> the account's budget when a valid session
// came with the request (USER_RATE_PER_MIN across every endpoint, each worker its share,
// createUserBudget; never counted against the address) -> route rate limits (local token bucket,
// then the primary's `ratelimit.take` for `shared` limits; `by: 'user'` limits count per account,
// or per client without a session; the password endpoints also limit each IPv6 /48 as a whole, see
// checkRates; all or none: a refusal gives back what the earlier rates took; a request refused
// because its client already has too many password hashes waiting gets these tokens back; a
// refusal by a limit keyed by the client's address, not by an account, also counts toward a block
// of that address, `abuseWeight` times or once: routeRefusal, `guard`) -> body (JSON only for the
// API, form-urlencoded for HTML pages, HTTP_BODY_LIMIT, or the route's `bodyLimit`, enforced while
// streaming: 413; Content-Type checked: 415; body read timeout: 408) -> strict schema validation
// (400; a route declared with { ownBodyValidation: true } gets the parsed JSON as it is and
// validates it itself) -> handler (timeout: 503; it may take more rates with ctx.takeRates(rates),
// e.g. the render quotas of a GIF on a cache miss, which join the request's tokens and count
// toward a block like the route's own when keyed by the address) -> JSON, HTML, text or binary
// answer.
//
// Answers: a handler returns { status, body, headers } (JSON), { status, html, headers } (an HTML
// page, PAGE_CSP), { status, text, contentType, headers } (a text file such as a PGN download:
// UTF-8, contentType defaults to text/plain; charset=utf-8) or { status, bytes, contentType,
// headers } (a binary file such as a GIF: a Buffer or Uint8Array, contentType defaults to
// application/octet-stream); null/undefined answers 204. `refundRate: true` on the result (or on
// a thrown error) gives back every rate token the request took (a 503 of the GIF pool, a 429 of
// the password hash queue). Every answer carries the same security headers; the JSON, text and
// binary answers the API's CSP. The send deadline is the listener's (net/listeners.js
// wrapApiHandler): an answer not flushed to the kernel SEND_TIMEOUT_MS (60 s) after the handler
// ended it is destroyed with its socket (a client that stops reading a GIF, a PGN or an export
// keeps neither the socket nor the buffer); the time the handler itself takes (a GIF render, an
// export, bounded by the route's `timeoutMs`) does not count toward it. handle.close() runs the
// close hooks of the route modules (deps.onClose: the GIF rendering threads) when the worker
// stops.
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
import { admitRequest } from '../net/listeners.js';
import * as infoRoutes from './routes/info.js';
import * as authRoutes from './routes/auth.js';
import * as accountRoutes from './routes/account.js';
import * as ssoRoutes from './routes/sso.js';
import * as playerRoutes from './routes/players.js';
import * as reportRoutes from './routes/reports.js';
import * as accountGameRoutes from './routes/account-games.js';
import * as accountExportRoutes from './routes/account-export.js';
import * as gifRoutes from './routes/gif.js';

export { HttpError } from './router.js';

/** Route modules of the auth owner (the bootstrap passes the full list, other owners' included). */
export const DEFAULT_ROUTES = Object.freeze([infoRoutes, authRoutes, accountRoutes, ssoRoutes, playerRoutes, reportRoutes, accountGameRoutes,
    accountExportRoutes, gifRoutes]);

const API_CSP = "default-src 'none'; frame-ancestors 'none'";
const BODY_METHODS = new Set(['POST', 'PUT', 'PATCH', 'DELETE']);
const MAX_URL = 4096;

const requestsTotal = metrics.counter('scacelith_http_requests_total', 'API requests', ['route', 'status']);
const requestMs = metrics.histogram('scacelith_http_request_duration_ms', 'API request duration', [2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000]);
const rateLimitedTotal = metrics.counter('scacelith_http_rate_limited_total', 'API requests refused by a rate limit', ['limit']);

function jsonError(status, code, message, extra) { return new HttpError(status, code, message, extra); }

function retryAfterSec(ms) { return Math.max(1, Math.ceil(ms / 1000)); }

/**
 * The 429 of a route limit (take()). One keyed by the client's address, not by an account,
 * carries `abuseWeight`: sendError counts it toward a block of that address (net/ipguard.js
 * noteRefusal), with the rate's own `abuseWeight` (the login, registration, reset and MFA family:
 * 5, AUTH_REFUSAL_WEIGHT) or 1. A per-account limit is the account's problem, not its network's:
 * weight 0. checkRates names the bucket of an account `<rate key>:u<userId>` and those of an
 * address `<rate key>:<ipKey>` and `<rate key>/48:<prefixKey>` (a `by: 'user'` rate without a
 * session falls back to the address).
 */
function routeRefusal(rate, key, ms, label) {
    const err = rateLimited(ms, label);
    const byUser = rate.by === 'user' && key.startsWith(`${rate.key}:u`);
    err.abuseWeight = byUser ? 0 : (rate.abuseWeight > 0 ? rate.abuseWeight : 1);
    return err;
}

function rateLimited(ms, label) {
    rateLimitedTotal.labels(label).inc();
    const s = retryAfterSec(ms);
    return new HttpError(429, 'rate_limited', 'Too many requests; try again later.', { retryAfter: s });
}

/**
 * The budget of one signed-in account across every call that carries a valid session
 * (USER_RATE_PER_MIN, abuse design 3.6): each worker allows its share of the whole-server rate,
 * max(1, min(L, ceil(2 L / WORKERS))) (all of it with 1 or 2 workers, half with 4), as a token
 * bucket holding half a minute of that share. Local to the worker: no IPC per request; a client
 * spread over every worker gets at most twice the rate. A refusal is the account's problem, not
 * its network's: it never counts toward blocking an address.
 * @param {object} config
 * @param {() => number} now
 */
function createUserBudget(config, now) {
    const perMin = Number.isInteger(config.userRatePerMin) && config.userRatePerMin > 0 ? config.userRatePerMin : 120;
    const workers = Math.max(1, Number(config.workers) || 1);
    const share = Math.max(1, Math.min(perMin, Math.ceil(2 * perMin / workers)));
    const burst = Math.max(1, Math.ceil(share / 2));
    const windowMs = Math.max(1, Math.round(burst * 60000 / share));
    const buckets = new TokenBucketLimiter({ now });
    return {
        share, burst,
        /** @returns {{ allowed: boolean, retryAfterMs: number }} */
        take(userId) { return buckets.take(`u${userId}`, burst, windowMs, 1); },
    };
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
 *           deps?: object, guard?: import('../net/ipguard.js').IpGuard|null }} opts `deps` adds fields to what route
 *           modules receive (they also get `onClose(fn)`, a hook run by handle.close()); `guard`: the shard's
 *           protection per address (the same one the listeners use), which also counts the refusals of the route
 *           limits keyed by the client's address (routeRefusal).
 * @returns {((req: import('node:http').IncomingMessage, res: import('node:http').ServerResponse) => Promise<void>)
 *   & { router: Router, close: () => Promise<void> }}
 */
export function createApiHandler({ config, store, auth, primary = null, anticheat = null, log, routes = DEFAULT_ROUTES,
    now = Date.now, ready = () => true, bodyTimeoutMs = 10000, handlerTimeoutMs = 30000, deps: extraDeps = {}, guard = null }) {
    const router = new Router();
    const closers = [];
    const deps = { config, store, auth, primary, anticheat, log, now, onClose: (fn) => { closers.push(fn); }, ...extraDeps };
    for (const r of routes) {
        const reg = typeof r === 'function' ? r : r && r.register;
        if (typeof reg !== 'function') throw new Error('createApiHandler: a route module has no register(router, deps)');
        reg(router, deps);
    }
    const limiter = new TokenBucketLimiter({ now });
    const userBudget = createUserBudget(config, now);
    const labelCache = new Map();
    const hsts = config.tlsMode === 'native';
    // Requests that did not come through the listener's wrapApiHandler (a handler used alone) meet
    // the protection per address here first; admitRequest runs once per request either way.
    const admission = { native: hsts, closeOnBlock: config.tlsMode !== 'proxy' };
    let primaryWarnAt = 0;

    function setBaseHeaders(res) {
        res.setHeader('Cache-Control', 'no-store');
        res.setHeader('X-Content-Type-Options', 'nosniff');
        res.setHeader('Referrer-Policy', 'no-referrer');
        res.setHeader('X-Frame-Options', 'DENY');
        res.setHeader('Cross-Origin-Resource-Policy', 'same-origin');
        if (hsts) res.setHeader('Strict-Transport-Security', 'max-age=31536000');
    }

    function send(req, res, status, { body, html, text, bytes, contentType, headers } = {}) {
        if (res.headersSent || res.writableEnded) return;
        let payload = '';
        if (html !== undefined) {
            res.setHeader('Content-Type', 'text/html; charset=utf-8');
            res.setHeader('Content-Security-Policy', PAGE_CSP);
            payload = html;
        } else if (text !== undefined) {
            res.setHeader('Content-Security-Policy', API_CSP);
            if (status !== 204) {
                res.setHeader('Content-Type', contentType || 'text/plain; charset=utf-8');
                payload = String(text);
            }
        } else if (bytes !== undefined) {
            res.setHeader('Content-Security-Policy', API_CSP);
            if (status !== 204) {
                res.setHeader('Content-Type', contentType || 'application/octet-stream');
                payload = Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
            }
        } else {
            res.setHeader('Content-Security-Policy', API_CSP);
            if (body !== undefined && status !== 204) {
                res.setHeader('Content-Type', 'application/json; charset=utf-8');
                payload = JSON.stringify(body);
            }
        }
        if (headers) for (const [k, v] of Object.entries(headers)) res.setHeader(k, v);
        const buf = Buffer.isBuffer(payload) ? payload : Buffer.from(payload, 'utf8');
        if (status !== 204) res.setHeader('Content-Length', buf.length);
        res.writeHead(status);
        // The listener's wrapApiHandler arms the send deadline when this ends the answer.
        res.end(req.method === 'HEAD' || status === 204 ? undefined : buf);
    }

    function sendError(req, res, err, page) {
        // A route limit of the client's address refused it (routeRefusal): toward a block.
        if (guard !== null && err && err.abuseWeight > 0) {
            guard.noteRefusal(guard.keysOf(req.clientIp ?? normalizeIp(req.socket?.remoteAddress), req.socket), err.abuseWeight);
        }
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

    /**
     * Takes one token of `key` (local bucket, then the primary's shared window for `shared` rates).
     * @returns {{ key: string, limit: number, windowMs: number, sharedAt: number|null }} what was
     *   taken (sharedAt: when the primary was asked, on the handler's clock, Date.now like the
     *   primary's; null when the primary did not count it)
     */
    async function take(rate, key, limit, label) {
        const local = limiter.take(key, limit, rate.windowMs, 1);
        if (!local.allowed) throw routeRefusal(rate, key, local.retryAfterMs, label);
        const taken = { key, limit, windowMs: rate.windowMs, sharedAt: null };
        if (rate.shared && primary) {
            let r = null;
            const askedAt = now();
            try {
                r = await primary.request('ratelimit.take', { key, limit, windowMs: rate.windowMs, cost: 1 });
            } catch (err) {
                // Fail open on the shared stage: the local bucket above still applies.
                const t = now();
                if (t - primaryWarnAt > 60000) { primaryWarnAt = t; log.warn('shared rate limit unavailable', { err: { message: err.message } }); }
            }
            if (r && r.allowed === false) throw routeRefusal(rate, key, r.retryAfterMs || 1000, label);
            if (r && r.allowed) taken.sharedAt = askedAt;
        }
        return taken;
    }

    /**
     * Gives back the tokens a request took (checkRates), when it ended with an answer that did
     * none of the work the limits protect: `refundRate` on the error or the handler's result (a
     * 429 of the password hash queue for a client with too many hashes waiting, auth/errors.js).
     * A shared window that cannot be reached keeps its count (it fails open anyway).
     */
    function giveBack(taken) {
        for (const t of taken) {
            limiter.give(t.key, t.limit, t.windowMs, 1);
            if (t.sharedAt === null || !primary) continue;
            const ageMs = Math.max(0, now() - t.sharedAt);
            Promise.resolve()
                .then(() => primary.request('ratelimit.refund', { key: t.key, windowMs: t.windowMs, cost: 1, ageMs }))
                .catch(() => { /* the shared window keeps this token */ });
        }
    }

    // A rate { key, limit, windowMs, shared?, by?: 'user', prefixLimit? } takes from the bucket of
    // the user or of the client's ipKey() (an IPv4 address or an IPv6 /64); `by: 'user'` without
    // a session (an anonymous request on an 'optional' or 'none' route) falls back to the client.
    // With `prefixLimit`, a client on IPv6 also takes from the bucket of its /48 (prefixKey()),
    // which bounds the 65536 /64 networks of one site together: without it, one /48 or /56 would
    // multiply the limit by the number of its /64s. The rates are taken in order, and all or none:
    // when one refuses, the tokens the earlier ones took are given back, so that a refused request
    // does not spend them (a route with an hourly and a daily limit, the render quotas of a GIF).
    // Returns what was taken (giveBack).
    async function checkRates(rates, ctx) {
        const taken = [];
        try {
            for (const rate of rates) {
                const byUser = rate.by === 'user' && ctx.user;
                taken.push(await take(rate, `${rate.key}:${byUser ? `u${ctx.user.userId}` : ipKey(ctx.ip)}`, rate.limit, rate.key));
                if (rate.prefixLimit && !byUser && ctx.ip.includes(':')) {
                    taken.push(await take(rate, `${rate.key}/48:${prefixKey(ctx.ip)}`, rate.prefixLimit, `${rate.key}/48`));
                }
            }
        } catch (err) {
            giveBack(taken);
            throw err;
        }
        return taken;
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
        if (guard !== null && !admitRequest(guard, req, res, admission)) return;
        const started = performance.now();
        let label = 'unmatched';
        let page = false;
        setBaseHeaders(res);
        const ip = normalizeIp(req.clientIp ?? req.socket?.remoteAddress ?? '');
        let taken = null;
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
            // The account's budget across every endpoint, before the endpoint's own limits.
            if (user) {
                const b = userBudget.take(user.userId);
                if (!b.allowed) throw rateLimited(b.retryAfterMs, 'user');
            }
            taken = [];
            const ctx = {
                req, res, ip, params, query: parseQuery(search), body: {},
                user: user ? { id: user.userId, userId: user.userId, username: user.username, emailVerified: user.emailVerified } : null,
                session: user ? { id: user.sessionId, tokenHash: user.tokenHash ?? null } : null,
                config, store, log, primary, auth, anticheat, now: now(), route: route.path,
                // More rates taken by the handler itself (the render quotas of a GIF, only on a
                // cache miss); they join the request's tokens, so that `refundRate` gives them back.
                takeRates: async (more) => {
                    const t = await checkRates(Array.isArray(more) ? more : [more], ctx);
                    taken.push(...t);
                    return t;
                },
            };
            const rates = route.opts.rate ? (Array.isArray(route.opts.rate) ? route.opts.rate : [route.opts.rate]) : [];
            if (rates.length) taken.push(...await checkRates(rates, ctx));

            if (route.opts.query) {
                const v = validate(route.opts.query, ctx.query);
                if (!v.ok) throw jsonError(400, 'invalid_request', v.message, { field: v.field });
                ctx.query = v.value;
            }

            if (BODY_METHODS.has(method)) {
                const buf = await readBody(req, route.opts.bodyLimit || config.httpBodyLimit, bodyTimeoutMs);
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
                if (route.opts.ownBodyValidation) {
                    ctx.body = parsed;      // any JSON value: the handler validates it
                } else {
                    const v = validate(route.opts.body || {}, parsed);
                    if (!v.ok) throw jsonError(400, 'invalid_request', v.message, v.field ? { field: v.field } : undefined);
                    ctx.body = v.value;
                }
            }

            const out = await runHandler(route, ctx);
            if (taken && out && out.refundRate) giveBack(taken);
            if (out === undefined || out === null) { send(req, res, 204); return; }
            const status = out.status || 200;
            if (out.html !== undefined) send(req, res, status, { html: out.html, headers: out.headers });
            else if (out.text !== undefined) send(req, res, status, { text: out.text, contentType: out.contentType, headers: out.headers });
            else if (out.bytes !== undefined) send(req, res, status, { bytes: out.bytes, contentType: out.contentType, headers: out.headers });
            else send(req, res, status, { body: out.body, headers: out.headers });
        } catch (err) {
            if (!(err && err.expose)) log.error('request failed', { route: label, err });
            if (taken && err && err.refundRate) giveBack(taken);
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
    /** Releases what the route modules hold (the GIF rendering threads): the worker's shutdown. */
    handle.close = async () => {
        for (const fn of closers.splice(0)) {
            try { await fn(); } catch (err) { log.error('API handler close failed', { err }); }
        }
    };
    return handle;
}
