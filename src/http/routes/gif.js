// Animated GIF of a game (docs/API.md section 11, docs/DESIGN.md 5.9): the game played move by
// move on a 2D board seen from above, answered as the .gif file itself.
//
//   GET  /api/v1/games/:id/gif?size=small|medium|large&orientation=white|black&delay=<ms>&coords=0|1
//        a game of this server; the header shows the names and ratings stored in the game record
//        (an anonymized player as "deleted#<id>"), the result and how the game ended.
//        Defaults: medium, white, 500 ms per move, coordinates on.
//   POST /api/v1/gif { pgn, size?, orientation?, delayMs?, coords? (boolean) }
//        any game, e.g. one of the player's saved games: the FIRST game of the PGN text (at most
//        65536 bytes of UTF-8), read by src/chess/pgn.js readPgn; names and ratings from the White,
//        Black, WhiteElo and BlackElo tags (accents dropped, other characters outside printable
//        ASCII as '?'), the result from the Result tag (else the movetext's), how the game ended
//        from the Termination tag unless it is "normal" (the final position then says it:
//        checkmate, stalemate...).
//
// Answers: 200 `image/gif` with `Content-Disposition: attachment; filename="scacelith-<id>.gif"`
// (GET) or "scacelith-game.gif" (POST) and Content-Length. Errors: 401 (both need a session: the
// quotas count per account), 400 invalid_option {field} (size, orientation, delay 100..3000,
// coords 0|1 or a boolean), 400 invalid_game_id, 404 not_found, 400 invalid_request {field} (POST
// body: unknown field, missing pgn), 400 invalid_pgn {line, column} (with the reader's message),
// 422 game_too_long (more than GIF_MAX_PLIES plies), 429 rate_limited {retryAfter} (a quota),
// 503 server_busy {retryAfter} (the rendering queue is full or the wait ran out: the quota tokens
// of the request are given back), 500 render_failed, 404 gif_disabled (GIF_ENABLED=false).
//
// Cost and quotas (abuse design 3.6). Rendering never runs on the event loop: the job goes to a
// worker thread of src/gif/pool.js (GIF_THREADS per worker process, created on the first render,
// stopped after a minute without work, closed with the API handler at shutdown; GIF_QUEUE_MAX
// jobs may wait GIF_QUEUE_TIMEOUT_MS for a thread). The threads run at the lowest scheduling
// priority on Linux (routes/gif-thread.js), so a render only takes the CPU the games leave. The
// only work done on the event loop is the game record's read or the PGN's (GIF_MAX_PLIES moves at
// most), a hash of the job and the cache.
//  * Request level, every call: the account's budget (USER_RATE_PER_MIN, server.js) and the
//    route's `gif` limit, 30 per minute per account (both routes together).
//  * Render level, only when the GIF is not in the cache, through ctx.takeRates: per account
//    GIF_USER_RENDERS_PER_MIN (4) and GIF_USER_RENDERS_PER_HOUR (30), per address (IPv4 or IPv6
//    /64) GIF_IP_RENDERS_PER_MIN (12) and GIF_IP_RENDERS_PER_HOUR (120), 3 times that per IPv6 /48;
//    all shared through the primary, so exact whatever worker a request reaches.
//  * Cache: an LRU of rendered GIFs per worker process (GIF_CACHE_MB), keyed by a hash of the job
//    (moves, start position, names, ratings, result, ending, options: never the PGN text itself),
//    so that a new name (an anonymized account) is a new picture. A request for a GIF being
//    rendered waits for that render instead of starting another (a render counts as started
//    before its quotas are checked, so two identical requests at once make one GIF). Neither
//    costs a render quota.
//
// Metrics: scacelith_gif_renders_total{result: ok|busy|failed}, scacelith_gif_render_duration_ms
// (queue wait and render), scacelith_gif_cache_total{result: hit|miss}, scacelith_gif_queue
// (renders waiting for a thread), scacelith_gif_renders_running, scacelith_gif_cache_bytes; the
// quota refusals are in scacelith_http_rate_limited_total{limit="gif"|"gif_user_min"|...}.

import crypto from 'node:crypto';
import { createGifPool } from '../../gif/pool.js';
import { DELAY, MAX_PLIES, SIZES } from '../../gif/render.js';
import { endReasonText, readPgn, PgnError } from '../../chess/index.js';
import { normalizeResult } from '../../chess/pgn.js';
import { enums } from '../../protocol/schema.js';
import { metrics } from '../../metrics.js';

/** The thread module of the pool: src/gif/worker.js at the lowest priority (routes/gif-thread.js). */
export const GIF_THREAD_URL = new URL('./gif-thread.js', import.meta.url);

/** The largest PGN text POST /gif takes, in bytes of UTF-8. */
export const GIF_PGN_MAX_BYTES = 65536;
export const GIF_CONTENT_TYPE = 'image/gif';

const ID_RE = /^[1-9][0-9]{0,15}$/;
const SIZE_NAMES = Object.keys(SIZES);
const ORIENTATIONS = ['white', 'black'];
const BODY_FIELDS = new Set(['pgn', 'size', 'orientation', 'delayMs', 'coords']);
const RESULT = { [enums.GameStatus.WhiteWins]: '1-0', [enums.GameStatus.BlackWins]: '0-1', [enums.GameStatus.Draw]: '1/2-1/2' };
/** Retry-After of a 503 server_busy, seconds (drawn at random, so that refused clients spread out). */
export const GIF_BUSY_RETRY_SEC = Object.freeze({ min: 3, max: 10 });

const live = new Set();
const sum = (f) => { let n = 0; for (const s of live) n += f(s); return n; };
const rendersTotal = metrics.counter('scacelith_gif_renders_total', 'GIF renders by result (ok, busy: refused by a full queue or a wait timeout, failed)', ['result']);
const rendersOk = rendersTotal.labels('ok'), rendersBusy = rendersTotal.labels('busy'), rendersFailed = rendersTotal.labels('failed');
const renderMs = metrics.histogram('scacelith_gif_render_duration_ms', 'Time to get a GIF rendered (queue wait and render)',
    [25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000]);
const cacheTotal = metrics.counter('scacelith_gif_cache_total', 'GIF requests served without a render (hit: cached or being rendered) or rendered (miss)', ['result']);
const cacheHit = cacheTotal.labels('hit'), cacheMiss = cacheTotal.labels('miss');
metrics.gaugeFn('scacelith_gif_queue', 'GIF renders waiting for a free rendering thread', () => sum((s) => s.stats().queued));
metrics.gaugeFn('scacelith_gif_renders_running', 'GIF renders in progress', () => sum((s) => s.stats().running));
metrics.gaugeFn('scacelith_gif_cache_bytes', 'Bytes of rendered GIFs in the cache', () => sum((s) => s.cache.bytes));

/** Byte-bounded LRU of rendered GIFs (the least recently used goes first). */
export class GifCache {
    /** @param {number} maxBytes 0 disables the cache */
    constructor(maxBytes) {
        this.maxBytes = Math.max(0, Math.floor(maxBytes) || 0);
        this.map = new Map();
        this.bytes = 0;
    }
    get size() { return this.map.size; }
    /** @returns {Buffer|null} */
    get(key) {
        const v = this.map.get(key);
        if (!v) return null;
        this.map.delete(key);
        this.map.set(key, v);
        return v;
    }
    /** Keeps `buf` unless it would take more than a quarter of the cache. @returns {boolean} */
    set(key, buf) {
        if (this.maxBytes <= 0 || buf.length > this.maxBytes / 4) return false;
        const old = this.map.get(key);
        if (old) { this.bytes -= old.length; this.map.delete(key); }
        this.map.set(key, buf);
        this.bytes += buf.length;
        while (this.bytes > this.maxBytes) {
            const [k, v] = this.map.entries().next().value;
            this.map.delete(k);
            this.bytes -= v.length;
        }
        return true;
    }
    clear() { this.map.clear(); this.bytes = 0; }
}

/**
 * The GIF renderer of one worker process: the thread pool (created on the first render), the
 * cache, and the renders in progress (a second request for the same GIF waits for the first).
 * @param {{ config: object, createPool?: Function }} o createPool: createGifPool or a stand-in (tests)
 */
export function createGifService({ config, createPool = createGifPool }) {
    let pool = null;
    let closed = false;
    const cache = new GifCache(config.gifCacheMb * 1024 * 1024);
    const inflight = new Map();
    const getPool = () => {
        if (!pool) {
            pool = createPool({
                threads: config.gifThreads, queueMax: config.gifQueueMax, timeoutMs: config.gifQueueTimeoutMs,
                renderTimeoutMs: config.gifRenderTimeoutMs, workerUrl: GIF_THREAD_URL,
            });
        }
        return pool;
    };
    const service = {
        cache,
        /** @returns {boolean} whether the rendering threads exist (they start with the first render) */
        get started() { return pool !== null; },
        /**
         * The GIF of `key` when it is cached, or the render in progress of the same GIF: a promise
         * of the GIF, or of null when the request that was to make it was refused its quotas
         * (claim().cancel(); the caller then looks again).
         */
        lookup(key) { return cache.get(key) || inflight.get(key) || null; },
        /**
         * Registers the GIF of `key` as being made BEFORE the request that makes it takes its render
         * quotas: those are shared through the primary (IPC round trips), and an identical request
         * arriving meanwhile must find this render (lookup) and wait for it, not pay for a second.
         * @returns {{ render(job: object): Promise<Buffer>, cancel(): void }} render: renders `job`
         *   on the pool and caches the result under `key` (rejects with the pool's errors, code
         *   'busy' | 'render_failed', and so do the requests waiting for it); cancel: nothing will
         *   be made (the quotas refused), the requests waiting get null and look again
         */
        claim(key) {
            let settle;
            const pending = new Promise((resolve, reject) => { settle = { resolve, reject }; });
            pending.catch(() => { /* nobody waiting for it */ });
            inflight.set(key, pending);
            const done = () => { if (inflight.get(key) === pending) inflight.delete(key); };
            let used = false;
            return {
                render(job) {
                    if (used) throw new Error('GIF claim already used');
                    used = true;
                    const t0 = performance.now();
                    let started;
                    try {
                        started = closed ? Promise.reject(Object.assign(new Error('GIF renderer closed'), { code: 'busy' })) : getPool().render(job);
                    } catch (err) {
                        started = Promise.reject(err);      // never leave the claim pending
                    }
                    const p = started.then((gif) => {
                        rendersOk.inc();
                        renderMs.observe(performance.now() - t0);
                        cache.set(key, gif);
                        return gif;
                    }, (err) => {
                        if (err && err.code === 'busy') rendersBusy.inc(); else rendersFailed.inc();
                        throw err;
                    });
                    p.then(settle.resolve, settle.reject);
                    p.then(done, done);
                    return p;
                },
                cancel() {
                    if (used) return;
                    used = true;
                    done();
                    settle.resolve(null);
                },
            };
        },
        stats() {
            const s = pool ? pool.stats() : null;
            return { queued: s ? s.queued : 0, running: s ? s.running : 0, live: s ? s.live : 0, cached: cache.size, cacheBytes: cache.bytes };
        },
        async close() {
            if (closed) return;
            closed = true;
            live.delete(service);
            cache.clear();
            if (pool) await pool.close();
        },
    };
    live.add(service);
    return service;
}

function error(status, code, message, extra) { return { status, body: { error: code, message, ...(extra || {}) } }; }

const invalidOption = (field, message) => error(400, 'invalid_option', message, { field });

/**
 * The picture options of a request, with the defaults filled in. A query string only carries text,
 * so the GET form takes the delay as decimal digits and coords as '0' / '1'; a JSON body has types,
 * and the POST form takes only a number (field delayMs) and a boolean: "500" or 1 there is a
 * mistake of the client, refused rather than guessed.
 * @param {{ size?: unknown, orientation?: unknown, delay?: unknown, coords?: unknown }} o
 * @param {'query' | 'body'} [from]  where the options come from
 * @returns {{ options: { size: string, orientation: string, delayMs: number, coords: boolean } } | { error: object }}
 */
export function gifOptions({ size, orientation, delay, coords }, from = 'query') {
    const query = from === 'query';
    const delayField = query ? 'delay' : 'delayMs';
    const out = { size: 'medium', orientation: 'white', delayMs: DELAY.default, coords: true };
    if (size !== undefined) {
        if (typeof size !== 'string' || !SIZE_NAMES.includes(size)) return { error: invalidOption('size', `size must be one of ${SIZE_NAMES.join(', ')}.`) };
        out.size = size;
    }
    if (orientation !== undefined) {
        if (typeof orientation !== 'string' || !ORIENTATIONS.includes(orientation)) return { error: invalidOption('orientation', 'orientation must be white or black.') };
        out.orientation = orientation;
    }
    if (delay !== undefined) {
        const n = query ? (typeof delay === 'string' && /^\d{1,6}$/.test(delay) ? Number(delay) : NaN) : delay;
        if (!Number.isInteger(n) || n < DELAY.min || n > DELAY.max) {
            return { error: invalidOption(delayField, `${delayField} must be an integer from ${DELAY.min} to ${DELAY.max} (milliseconds per move).`) };
        }
        out.delayMs = n;
    }
    if (coords !== undefined) {
        if (coords === (query ? '1' : true)) out.coords = true;
        else if (coords === (query ? '0' : false)) out.coords = false;
        else return { error: invalidOption('coords', query ? 'coords must be 0 or 1.' : 'coords must be true or false.') };
    }
    return { options: out };
}

/** A player's name from a PGN tag: printable ASCII (accents dropped, anything else '?'), at most 48 characters. */
export function tagText(v, max = 48) {
    if (typeof v !== 'string') return '';
    const s = v.normalize('NFKD').replace(/[\u0300-\u036f]/g, '').replace(/[^\x20-\x7e]/g, '?').replace(/\s+/g, ' ').trim();
    return s.length > max ? s.slice(0, max) : s;
}

/** A rating from a WhiteElo / BlackElo tag (null when absent, "-", "?" or not a number). */
function tagRating(v) {
    return typeof v === 'string' && /^\s*\d{1,4}\s*$/.test(v) ? Number(v) : null;
}

/** The ending shown from a Termination tag: null for "normal" or none (the final position says it). */
function tagEnding(v) {
    const s = tagText(v, 60);
    if (!s || s.toLowerCase() === 'normal') return null;
    return s.charAt(0).toUpperCase() + s.slice(1);
}

/** The cache key of a job: a hash of everything that changes the picture. */
export function gifCacheKey(job) {
    const parts = [
        'v1', job.startFen ?? null, Array.from(job.moves ?? []),
        job.white?.name ?? '', job.white?.rating ?? null, job.black?.name ?? '', job.black?.rating ?? null,
        job.result ?? '*', job.footer ?? null, job.options,
    ];
    return crypto.createHash('sha256').update(JSON.stringify(parts)).digest('base64url');
}

/**
 * Registers the GIF routes.
 * @param {import('../router.js').Router} router
 * @param {{ config: object, store: object, log: object, onClose?: Function, gifService?: object, createGifPool?: Function }} deps
 *   gifService / createGifPool: stand-ins for the tests
 */
export function register(router, deps) {
    const { config, store, log } = deps;
    const service = deps.gifService || createGifService({ config, createPool: deps.createGifPool });
    if (typeof deps.onClose === 'function') deps.onClose(() => service.close());
    const maxPlies = Math.min(config.gifMaxPlies, MAX_PLIES);
    const routeRate = { key: 'gif', limit: 30, windowMs: 60000, by: 'user' };
    const renderRates = [
        { key: 'gif_user_min', limit: config.gifUserRendersPerMin, windowMs: 60000, by: 'user', shared: true },
        { key: 'gif_user_hour', limit: config.gifUserRendersPerHour, windowMs: 3600000, by: 'user', shared: true },
        { key: 'gif_ip_min', limit: config.gifIpRendersPerMin, windowMs: 60000, shared: true, prefixLimit: 3 * config.gifIpRendersPerMin },
        { key: 'gif_ip_hour', limit: config.gifIpRendersPerHour, windowMs: 3600000, shared: true, prefixLimit: 3 * config.gifIpRendersPerHour },
    ];
    // A render waits GIF_QUEUE_TIMEOUT_MS at most for a thread, then runs GIF_RENDER_TIMEOUT_MS at most.
    const timeoutMs = config.gifQueueTimeoutMs + config.gifRenderTimeoutMs + 5000;

    const disabled = () => ({ ...error(404, 'gif_disabled', 'Animated GIFs are turned off on this server.'), refundRate: true });
    const tooLong = (plies) => error(422, 'game_too_long', `The game is too long for a GIF (${plies === null ? 'more than ' + maxPlies : plies} plies, at most ${maxPlies}).`);

    /** The cached GIF, or a new render (render quotas taken first), as the answer. */
    async function deliver(ctx, job, filename) {
        const key = gifCacheKey(job);
        let gif = null;
        try {
            // A GIF in the cache or being made for another request costs no render quota. The
            // render is registered before its quotas are taken (claim), so that an identical
            // request arriving during that check waits for it; when they refuse it (null), the
            // requests waiting look again and the next one pays for its own render.
            let found = service.lookup(key);
            while (found) {
                gif = await found;
                found = gif ? null : service.lookup(key);
            }
            if (gif) {
                cacheHit.inc();
            } else {
                cacheMiss.inc();
                const claim = service.claim(key);
                try {
                    await ctx.takeRates(renderRates);
                } catch (err) {
                    claim.cancel();
                    throw err;
                }
                gif = await claim.render(job);
            }
        } catch (err) {
            if (err && err.code === 'busy' && !err.expose) {
                const s = GIF_BUSY_RETRY_SEC.min + Math.floor(Math.random() * (GIF_BUSY_RETRY_SEC.max - GIF_BUSY_RETRY_SEC.min + 1));
                return { ...error(503, 'server_busy', 'The server is busy making other GIFs; try again in a few seconds.', { retryAfter: s }),
                    headers: { 'Retry-After': String(s) }, refundRate: true };
            }
            if (err && err.code === 'render_failed') {
                log.warn('GIF render failed', { err: { message: err.message }, plies: job.moves.length, route: ctx.route });
                return error(500, 'render_failed', 'The GIF could not be made.');
            }
            throw err;
        }
        return {
            status: 200, bytes: gif, contentType: GIF_CONTENT_TYPE,
            headers: { 'Content-Disposition': `attachment; filename="${filename}"` },
        };
    }

    async function gameGif(ctx) {
        if (!config.gifEnabled) return disabled();
        const q = ctx.query || {};
        const o = gifOptions({ size: q.size, orientation: q.orientation, delay: q.delay, coords: q.coords });
        if (o.error) return o.error;
        const raw = ctx.params && ctx.params.id;
        if (!ID_RE.test(String(raw)) || !Number.isSafeInteger(Number(raw))) return error(400, 'invalid_game_id', 'Invalid game id.');
        let g;
        try {
            g = store.games.byId(Number(raw));
        } catch (e) {
            if (e && e.code === 'busy') return error(503, 'busy', 'Try again shortly.', { retryAfter: 1 });
            throw e;
        }
        if (!g) return error(404, 'not_found', 'No such game.');
        if (g.moves.length > maxPlies) return tooLong(g.moves.length);
        const job = {
            startFen: null,
            moves: Array.from(g.moves),
            white: { name: String(g.whiteName ?? ''), rating: g.whiteRating ?? null },
            black: { name: String(g.blackName ?? ''), rating: g.blackRating ?? null },
            result: RESULT[g.status] ?? '*',
            footer: endReasonText(g.reason) || null,
            options: o.options,
        };
        return deliver(ctx, job, `scacelith-${g.id}.gif`);
    }

    async function pgnGif(ctx) {
        if (!config.gifEnabled) return disabled();
        const b = ctx.body;
        if (b === null || typeof b !== 'object' || Array.isArray(b)) return error(400, 'invalid_request', 'The body must be a JSON object.');
        for (const k of Object.keys(b)) if (!BODY_FIELDS.has(k)) return error(400, 'invalid_request', `unknown field "${k}"`, { field: k });
        if (typeof b.pgn !== 'string') return error(400, 'invalid_request', b.pgn === undefined ? '"pgn" is required' : '"pgn" must be a string', { field: 'pgn' });
        const o = gifOptions({ size: b.size, orientation: b.orientation, delay: b.delayMs, coords: b.coords }, 'body');
        if (o.error) return o.error;
        let game;
        try {
            game = readPgn(b.pgn, { maxBytes: GIF_PGN_MAX_BYTES, maxPlies });
        } catch (e) {
            if (!(e instanceof PgnError)) throw e;
            // readPgn stops at the first move beyond maxPlies.
            if (/^too many moves/.test(e.message)) return tooLong(null);
            return error(400, 'invalid_pgn', e.message, { line: e.line, column: e.column });
        }
        const tag = (name) => { let v; for (const [n, x] of game.tags) if (n === name) v = x; return v; };
        const job = {
            startFen: game.startFen,
            moves: game.moves,
            white: { name: tagText(tag('White')), rating: tagRating(tag('WhiteElo')) },
            black: { name: tagText(tag('Black')), rating: tagRating(tag('BlackElo')) },
            result: normalizeResult(tag('Result') ?? '') || game.result,
            footer: tagEnding(tag('Termination')),
            options: o.options,
        };
        return deliver(ctx, job, 'scacelith-game.gif');
    }

    router.get('/games/:id/gif', gameGif, { auth: 'required', rate: routeRate, timeoutMs });
    router.post('/gif', pgnGif, {
        auth: 'required', rate: routeRate, timeoutMs, ownBodyValidation: true,
        // The PGN as a JSON string: escaping may double its bytes.
        bodyLimit: 2 * GIF_PGN_MAX_BYTES + 4096,
    });
}
