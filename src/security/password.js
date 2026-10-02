// Password hashing and password policy (docs/DESIGN.md section 8).
//
// Hashes are self-describing strings:
//   scrypt$<log2 N>$<r>$<p>$<salt base64url>$<key base64url>        (N=2^17, r=8, p=1, 64-byte key)
//   $argon2id$v=19$m=<KiB>,t=<passes>,p=<lanes>$<salt b64>$<tag b64> (PHC string format)
// argon2id is used for new hashes when the runtime has crypto.argon2 (Node >= 24.7, feature
// detected); a stored hash of another algorithm or with weaker parameters is reported by
// verify() as `needsRehash`, and the login upgrades it with the password it just checked.
// Both functions run in the libuv thread pool (never on the event loop).
//
// Unknown accounts: verifyDummy() does the same work on a fixed dummy hash, so that a login for
// an unknown user takes as long as one with a wrong password. A stored argon2id hash on a runtime
// without crypto.argon2 does the dummy work too. The dummy uses the preferred algorithm, so after
// a runtime upgrade (scrypt hashes stored, argon2id preferred) the two still take different times;
// the limited hasher's checkPassword() therefore pads failed checks (below).
//
// Concurrency cap: one hash costs about 0.5-0.6 s of CPU and 64-128 MiB, and the thread pool
// (UV_THREADPOOL_SIZE, 4 threads per process by default) is shared with the journal's fdatasync,
// DNS lookups (SMTP, Google sign-in) and file reads. Without a cap, a burst of logins fills the
// pool of every worker and takes the cores from the game event loops. createHashLimiter() is a
// bounded FIFO semaphore (PASSWORD_HASH_CONCURRENCY running, PASSWORD_HASH_QUEUE_MAX waiting for
// at most PASSWORD_HASH_QUEUE_TIMEOUT_MS, and, once the queue is half full, no more than
// `perSourceMax` of the waiting ones from one client source, PASSWORD_HASH_WAITERS_PER_SOURCE);
// limitHasher() sends every hash and verification of a hasher through it, the
// dummy verification included, so that an unknown account still costs the same wait and the same
// work. A call may bring its own, shorter wait budget (`maxWaitMs`): a request that hashes twice
// (a password change) spends one queue timeout in all, and `maxWaitMs: 0` runs an optional hash
// (the rehash of an outdated hash at login) only when a slot is free at once. The auth service
// owns one limiter per worker process; a call the limiter refuses rejects with PasswordBusyError
// before any work is done.
//
// Padding of failed checks: checkPassword() records how long each check worked in its slot and,
// when the password is wrong or the account unknown, waits after releasing the slot until the
// check took as long as the slowest check of the last 10 to 20 minutes, and never less than the
// baseline the warm-up measured (at most 2 s in all). The warm-up times a verification of the
// slowest kind of hash the database can hold: the preferred algorithm (computing the dummy hash
// is that work) and, when that is argon2id, the scrypt hashes stored before the runtime had
// argon2. The baseline does not decay, so the first failed check after a start or after a quiet
// period is padded too. The wait uses no hash capacity, and a failure then takes the same time
// whatever algorithm, parameters or dummy was behind it, so that the time of a failed login does
// not tell whether the account exists.

import crypto from 'node:crypto';
import fs from 'node:fs';
import { promisify } from 'node:util';
import { metrics } from '../metrics.js';

const scryptAsync = promisify(crypto.scrypt);

const mInFlight = metrics.gauge('scacelith_password_hash_in_flight', 'Password hashes and verifications running (libuv thread pool)', [], { perShard: true });
const mQueued = metrics.gauge('scacelith_password_hash_queued', 'Password hashes and verifications waiting for a slot', [], { perShard: true });
const mWaitMs = metrics.histogram('scacelith_password_hash_wait_ms', 'Time a password hash waited for its slot (granted ones)',
    [1, 10, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 20000]);
const mRejected = metrics.counter('scacelith_password_hash_rejected_total',
    'Password hashes refused (queue_full, timeout: 503 server_busy; source_limit: 429 rate_limited)', ['reason']);
const mRejectedFull = mRejected.labels('queue_full');
const mRejectedTimeout = mRejected.labels('timeout');
const mRejectedSource = mRejected.labels('source_limit');

export const PASSWORD_MAX_BYTES = 256;
export const SCRYPT_DEFAULTS = Object.freeze({ logN: 17, r: 8, p: 1, keyLen: 64, saltLen: 16 });
// RFC 9106 second recommended option (64 MiB, 3 passes, 4 lanes).
export const ARGON2_DEFAULTS = Object.freeze({ memory: 65536, passes: 3, parallelism: 4, tagLength: 32, saltLen: 16 });

let commonSet = null;

/**
 * The embedded list of common passwords (lower case), loaded on first use.
 * @returns {Set<string>}
 */
export function commonPasswords() {
    if (!commonSet) {
        const text = fs.readFileSync(new URL('./common-passwords.txt', import.meta.url), 'utf8');
        commonSet = new Set(text.split(/\r?\n/).map((l) => l.trim().toLowerCase()).filter((l) => l && !l.startsWith('#')));
    }
    return commonSet;
}

/**
 * True when the password (lower-cased) is in the embedded list.
 * @param {string} password
 * @returns {boolean}
 */
export function isCommonPassword(password) {
    return commonPasswords().has(String(password).toLowerCase());
}

/**
 * Unicode normalisation applied to every password before hashing or checking (NFC), so that the
 * same password typed on different systems gives the same bytes.
 * @param {string} password
 * @returns {string}
 */
export function normalizePassword(password) {
    return String(password).normalize('NFC');
}

/**
 * Checks the password policy. Returns null when acceptable, else { reason, message }.
 * reason: 'too_short' | 'too_long' | 'contains_username' | 'contains_email' | 'too_common'.
 * @param {string} password
 * @param {{ minLength: number, username?: string, email?: string }} opts
 * @returns {null | { reason: string, message: string }}
 */
export function checkPasswordPolicy(password, { minLength, username = '', email = '' }) {
    const pw = normalizePassword(password);
    if ([...pw].length < minLength) return { reason: 'too_short', message: `The password must have at least ${minLength} characters.` };
    if (Buffer.byteLength(pw, 'utf8') > PASSWORD_MAX_BYTES) return { reason: 'too_long', message: `The password must not exceed ${PASSWORD_MAX_BYTES} bytes.` };
    const lower = pw.toLowerCase();
    const u = String(username || '').toLowerCase();
    if (u.length >= 3 && lower.includes(u)) return { reason: 'contains_username', message: 'The password must not contain the username.' };
    const local = String(email || '').toLowerCase().split('@')[0];
    if (local.length >= 3 && lower.includes(local)) return { reason: 'contains_email', message: 'The password must not contain the e-mail address.' };
    if (isCommonPassword(pw)) return { reason: 'too_common', message: 'This password is too common; choose another one.' };
    return null;
}

function b64(buf) { return buf.toString('base64').replace(/=+$/, ''); }

/**
 * Password hasher.
 * @param {{ scrypt?: Partial<typeof SCRYPT_DEFAULTS>, argon2?: Function|null|false,
 *           argon2Params?: Partial<typeof ARGON2_DEFAULTS> }} [opts]
 *   `argon2`: the crypto.argon2-compatible function to use (default: crypto.argon2 when it
 *   exists; false forces scrypt).
 */
export function createPasswordHasher(opts = {}) {
    const sc = { ...SCRYPT_DEFAULTS, ...(opts.scrypt || {}) };
    const argon2Fn = opts.argon2 === undefined ? (typeof crypto.argon2 === 'function' ? crypto.argon2 : null) : (opts.argon2 || null);
    const ap = { ...ARGON2_DEFAULTS, ...(opts.argon2Params || {}) };
    const argon2Async = argon2Fn ? (params) => new Promise((resolve, reject) => {
        argon2Fn('argon2id', params, (err, key) => (err ? reject(err) : resolve(Buffer.from(key))));
    }) : null;
    const preferred = argon2Async ? 'argon2id' : 'scrypt';
    // scrypt needs 128 * N * r bytes (+ 128 * r * p); give it twice that as the ceiling.
    const scryptMaxmem = (n, r, p) => 2 * 128 * r * (n + p + 2) + 1024 * 1024;

    async function scryptKey(pw, salt, logN, r, p, keyLen) {
        const N = 2 ** logN;
        return scryptAsync(pw, salt, keyLen, { N, r, p, maxmem: scryptMaxmem(N, r, p) });
    }

    /**
     * @param {string} password
     * @returns {Promise<string>} self-describing hash
     */
    async function hash(password) {
        const pw = Buffer.from(normalizePassword(password), 'utf8');
        if (preferred === 'argon2id') {
            const salt = crypto.randomBytes(ap.saltLen);
            const tag = await argon2Async({ message: pw, nonce: salt, parallelism: ap.parallelism, tagLength: ap.tagLength, memory: ap.memory, passes: ap.passes });
            return `$argon2id$v=19$m=${ap.memory},t=${ap.passes},p=${ap.parallelism}$${b64(salt)}$${b64(tag)}`;
        }
        const salt = crypto.randomBytes(sc.saltLen);
        const key = await scryptKey(pw, salt, sc.logN, sc.r, sc.p, sc.keyLen);
        return `scrypt$${sc.logN}$${sc.r}$${sc.p}$${salt.toString('base64url')}$${key.toString('base64url')}`;
    }

    function parse(stored) {
        if (typeof stored !== 'string') return null;
        let m = /^scrypt\$(\d{1,2})\$(\d{1,3})\$(\d{1,3})\$([A-Za-z0-9_-]{16,})\$([A-Za-z0-9_-]{22,})$/.exec(stored);
        if (m) {
            const logN = +m[1], r = +m[2], p = +m[3];
            if (logN < 1 || logN > 22 || r < 1 || r > 64 || p < 1 || p > 16) return null;
            return { alg: 'scrypt', logN, r, p, salt: Buffer.from(m[4], 'base64url'), key: Buffer.from(m[5], 'base64url') };
        }
        m = /^\$argon2id\$v=19\$m=(\d{1,8}),t=(\d{1,3}),p=(\d{1,3})\$([A-Za-z0-9+/]{11,})\$([A-Za-z0-9+/]{11,})$/.exec(stored);
        if (m) return { alg: 'argon2id', memory: +m[1], passes: +m[2], parallelism: +m[3], salt: Buffer.from(m[4], 'base64'), key: Buffer.from(m[5], 'base64') };
        return null;
    }

    function outdated(h) {
        if (h.alg !== preferred) return true;
        if (h.alg === 'scrypt') return h.logN < sc.logN || h.r !== sc.r || h.p !== sc.p || h.key.length !== sc.keyLen;
        return h.memory < ap.memory || h.passes < ap.passes || h.parallelism !== ap.parallelism || h.key.length !== ap.tagLength;
    }

    /**
     * @param {string} stored
     * @param {string} password
     * @returns {Promise<{ ok: boolean, needsRehash: boolean }>}
     */
    async function verify(stored, password) {
        const h = parse(stored);
        const pw = Buffer.from(normalizePassword(password), 'utf8');
        if (!h) {
            await dummyWork(pw);
            return { ok: false, needsRehash: false };
        }
        let key;
        if (h.alg === 'scrypt') key = await scryptKey(pw, h.salt, h.logN, h.r, h.p, h.key.length);
        else if (argon2Async) key = await argon2Async({ message: pw, nonce: h.salt, parallelism: h.parallelism, tagLength: h.key.length, memory: h.memory, passes: h.passes });
        else {
            // An argon2 hash on a runtime without argon2 cannot be checked: fail after the same
            // work as an unknown account, so that the answer does not come back at once.
            await dummyWork(pw);
            return { ok: false, needsRehash: false };
        }
        const ok = key.length === h.key.length && crypto.timingSafeEqual(key, h.key);
        return { ok, needsRehash: ok && outdated(h) };
    }

    let dummy = null;
    // The dummy hash, computed once. A failure (memory, thread pool) is not kept: the next call
    // computes it again, instead of every unknown-account login failing until a restart.
    function dummyHash() {
        if (!dummy) {
            const p = hash(crypto.randomBytes(18).toString('base64'));
            dummy = p;
            p.catch(() => { if (dummy === p) dummy = null; });
        }
        return dummy;
    }
    async function dummyWork(pw) {
        const d = parse(await dummyHash());
        if (d.alg === 'scrypt') await scryptKey(pw, d.salt, d.logN, d.r, d.p, d.key.length);
        else await argon2Async({ message: pw, nonce: d.salt, parallelism: d.parallelism, tagLength: d.key.length, memory: d.memory, passes: d.passes });
    }

    /**
     * The work of a verification, for a login whose account does not exist (always false).
     * @param {string} password
     * @returns {Promise<false>}
     */
    async function verifyDummy(password) {
        await dummyWork(Buffer.from(normalizePassword(password), 'utf8'));
        return false;
    }

    /**
     * Computes the dummy hash in advance (so the first unknown login is not faster), and measures
     * the slowest verification a failed login can cost: one of the preferred algorithm (computing
     * the dummy hash is that work) and, when argon2id is preferred, one with the scrypt parameters
     * of the hashes stored before (they stay in the database until their owner logs in again).
     * @returns {Promise<number>} the slowest measured verification, in ms
     */
    async function warmUp() {
        let slowest = 0;
        const timed = async (work) => {
            const t0 = performance.now();
            await work();
            slowest = Math.max(slowest, performance.now() - t0);
        };
        const pw = Buffer.from(crypto.randomBytes(18).toString('base64'), 'utf8');
        if (!dummy) {
            await timed(() => dummyHash());
        } else {
            await dummy;
            await timed(() => dummyWork(pw));
        }
        if (preferred === 'argon2id') {
            await timed(() => scryptKey(pw, crypto.randomBytes(sc.saltLen), sc.logN, sc.r, sc.p, sc.keyLen));
        }
        return slowest;
    }

    return { hash, verify, verifyDummy, warmUp, algorithm: preferred, parse };
}

const BUSY_MESSAGES = Object.freeze({
    queue_full: 'password hashing: the queue is full',
    timeout: 'password hashing: the wait for a slot expired',
    source_limit: 'password hashing: this client already has as many hashes waiting as it may',
    no_wait: 'password hashing: no slot is free and the call would not wait',
});

/**
 * A password hash the limiter refused (nothing was hashed). `reason`:
 *   'queue_full'    `queueMax` tasks already wait;
 *   'timeout'       the wait for a slot expired (queueTimeoutMs, or the call's shorter maxWaitMs);
 *   'source_limit'  `perSourceMax` tasks of the same client source already wait;
 *   'no_wait'       no slot was free and the call would not wait (maxWaitMs <= 0). This is no
 *                   refusal of a request: the caller skips optional work, and no metric counts it.
 */
export class PasswordBusyError extends Error {
    /** @param {'queue_full'|'timeout'|'source_limit'|'no_wait'} reason */
    constructor(reason) {
        super(BUSY_MESSAGES[reason] || 'password hashing: refused');
        this.name = 'PasswordBusyError';
        this.code = 'password_busy';
        this.reason = reason;
    }
}

/**
 * Bounded FIFO semaphore for the password hashes of one process.
 *
 * At most `concurrency` tasks run at once; the others wait in arrival order. A task that finds
 * `queueMax` tasks already waiting is refused at once. So is a task whose `source` (a client
 * address or network prefix) already has `perSourceMax` tasks waiting, but only under contention,
 * once at least half of `queueMax` tasks wait: one source (a classroom behind one IPv4 address)
 * may use an idle queue, and, as long as `perSourceMax` is at most half of `queueMax`, it cannot
 * take more than half of it, so the other half stays open to the other sources (a larger
 * `perSourceMax` lets it hold that many). A waiting task gives up after `queueTimeoutMs`, or after its own shorter
 * `maxWaitMs` (it leaves the queue). All of these reject with PasswordBusyError. A finished task
 * (resolved, rejected or thrown) hands its slot straight to the oldest waiter.
 * @param {{ concurrency?: number, queueMax?: number, queueTimeoutMs?: number, perSourceMax?: number }} [opts]
 */
export function createHashLimiter({ concurrency = 1, queueMax = 32, queueTimeoutMs = 10000, perSourceMax = Infinity } = {}) {
    if (!Number.isInteger(concurrency) || concurrency < 1) throw new RangeError('concurrency must be an integer >= 1');
    if (!Number.isInteger(queueMax) || queueMax < 0) throw new RangeError('queueMax must be an integer >= 0');
    if (!Number.isFinite(queueTimeoutMs) || queueTimeoutMs < 0) throw new RangeError('queueTimeoutMs must be >= 0');
    if (perSourceMax !== Infinity && !(Number.isInteger(perSourceMax) && perSourceMax >= 1)) throw new RangeError('perSourceMax must be an integer >= 1');
    let active = 0;
    const waiting = [];             // { resolve, reject, since, timer, source }, oldest first
    const bySource = new Map();     // source -> number of its tasks waiting (sources with at least one)
    const contended = Math.floor(queueMax / 2);     // from this many waiting on, perSourceMax applies

    function leave(w) {
        mQueued.dec();
        if (w.source === null) return;
        const n = bySource.get(w.source) - 1;
        if (n > 0) bySource.set(w.source, n);
        else bySource.delete(w.source);
    }

    // Invariant: `waiting` is empty whenever active < concurrency (release() hands a slot over
    // instead of freeing it while someone waits), so a newcomer never overtakes a waiter.
    function acquire(waitMs, noWait, source) {
        if (active < concurrency) {
            active++;
            mInFlight.inc();
            mWaitMs.observe(0);
            return Promise.resolve();
        }
        if (noWait) return Promise.reject(new PasswordBusyError('no_wait'));
        if (source !== null && waiting.length >= contended && (bySource.get(source) || 0) >= perSourceMax) {
            mRejectedSource.inc();
            return Promise.reject(new PasswordBusyError('source_limit'));
        }
        if (waiting.length >= queueMax) {
            mRejectedFull.inc();
            return Promise.reject(new PasswordBusyError('queue_full'));
        }
        return new Promise((resolve, reject) => {
            const w = { resolve, reject, since: performance.now(), timer: null, source };
            w.timer = setTimeout(() => {
                const i = waiting.indexOf(w);
                if (i < 0) return;
                waiting.splice(i, 1);
                leave(w);
                mRejectedTimeout.inc();
                reject(new PasswordBusyError('timeout'));
            }, waitMs);
            waiting.push(w);
            mQueued.inc();
            if (source !== null) bySource.set(source, (bySource.get(source) || 0) + 1);
        });
    }

    function release() {
        const w = waiting.shift();
        if (w) {
            clearTimeout(w.timer);
            leave(w);
            mWaitMs.observe(performance.now() - w.since);
            w.resolve();            // the slot passes to the oldest waiter: `active` is unchanged
            return;
        }
        active--;
        mInFlight.dec();
    }

    /**
     * Runs `fn` when a slot is free and returns its result.
     * @template T
     * @param {() => T|Promise<T>} fn
     * @param {{ maxWaitMs?: number, source?: string|null }} [opts]
     *   `maxWaitMs`: the longest wait of this call (queueTimeoutMs when absent, never more). At 0 or
     *   less the call runs only when a slot is free at once and is otherwise refused with reason
     *   'no_wait'. `source`: the client source that `perSourceMax` counts (null: not counted).
     * @returns {Promise<T>} rejects with PasswordBusyError when refused (fn is then not called)
     */
    async function run(fn, { maxWaitMs, source = null } = {}) {
        const own = maxWaitMs !== undefined && maxWaitMs !== null;
        await acquire(own ? Math.min(queueTimeoutMs, maxWaitMs) : queueTimeoutMs, own && !(maxWaitMs > 0), source ?? null);
        try {
            return await fn();
        } finally {
            release();
        }
    }

    return {
        run,
        /** Current state (tests, logs). */
        stats: () => ({ active, waiting: waiting.length, concurrency, queueMax, queueTimeoutMs }),
        /** The per-source cap (Infinity: none). */
        perSourceMax,
        /** Number of tasks of `source` waiting now (tests). */
        waitingFrom: (source) => bySource.get(source) || 0,
    };
}

/**
 * The slowest recent password check: the longest duration recorded in the current period of
 * `periodMs` or in the one before (a value is remembered for one to two periods), never less than
 * the baseline (setBaseline(), which does not decay), at most `capMs`.
 * @param {{ capMs?: number, periodMs?: number, clock?: () => number }} [opts] `clock` in ms
 *   (performance.now by default: the periods do not follow the server's configured clock)
 */
export function createCheckFloor({ capMs = 2000, periodMs = 10 * 60000, clock = () => performance.now() } = {}) {
    const origin = clock();
    let period = 0, cur = 0, prev = 0, baseline = 0;
    function roll(t) {
        const p = Math.floor((t - origin) / periodMs);
        if (p === period) return;
        prev = p === period + 1 ? cur : 0;
        cur = 0;
        period = p;
    }
    return {
        /** Records the duration of one check, in ms. */
        record(ms) {
            roll(clock());
            if (ms > cur) cur = ms;
        },
        /**
         * Sets the part of the floor that does not decay (the warm-up's measure of the slowest
         * kind of verification), in ms; a value below the current baseline is ignored.
         */
        setBaseline(ms) {
            if (Number.isFinite(ms) && ms > baseline) baseline = ms;
        },
        /** The baseline, in ms. */
        baselineMs: () => baseline,
        /** The duration a failed check is padded to, in ms. */
        floorMs() {
            roll(clock());
            return Math.min(capMs, Math.max(cur, prev, baseline));
        },
    };
}

/**
 * The same hasher with every hash and verification (the dummy one and the warm-up included) run
 * through `limiter`. hash, verify and verifyDummy take the options of the limiter's run() as their
 * last argument ({ maxWaitMs, source }). checkPassword() is the check of a login (file header);
 * warmUp() also sets the floor's baseline to the slowest verification the hasher's warm-up timed.
 * @param {ReturnType<typeof createPasswordHasher>} hasher
 * @param {ReturnType<typeof createHashLimiter>} limiter
 * @param {{ onBusy?: (err: PasswordBusyError) => Error, floor?: ReturnType<typeof createCheckFloor> }} [opts]
 *   `onBusy` maps a refusal to the error thrown instead (the auth service answers 503 server_busy
 *   or 429 rate_limited); `floor` replaces the default padding floor (tests).
 */
export function limitHasher(hasher, limiter, { onBusy = (err) => err, floor = createCheckFloor() } = {}) {
    const run = (fn, opts) => limiter.run(fn, opts).catch((err) => { throw err instanceof PasswordBusyError ? onBusy(err) : err; });

    /**
     * Checks `password` against `stored`, or does the dummy check when `stored` is null (unknown
     * account, account without password). A failure resolves only once the check took the padding
     * floor, waited after the slot is released.
     * @param {string|null} stored
     * @param {string} password
     * @param {{ maxWaitMs?: number, source?: string|null }} [opts]
     * @returns {Promise<{ ok: boolean, needsRehash: boolean }>}
     */
    async function checkPassword(stored, password, opts) {
        let workMs = 0;
        const r = await run(async () => {
            const t0 = performance.now();
            try {
                return stored ? await hasher.verify(stored, password) : { ok: await hasher.verifyDummy(password), needsRehash: false };
            } finally {
                workMs = performance.now() - t0;
                floor.record(workMs);
            }
        }, opts);
        if (!r.ok) {
            const pad = floor.floorMs() - workMs;
            if (pad >= 1) await new Promise((resolve) => { setTimeout(resolve, pad); });
        }
        return r;
    }

    return {
        algorithm: hasher.algorithm,
        parse: hasher.parse,
        hash: (password, opts) => run(() => hasher.hash(password), opts),
        verify: (stored, password, opts) => run(() => hasher.verify(stored, password), opts),
        verifyDummy: (password, opts) => run(() => hasher.verifyDummy(password), opts),
        checkPassword,
        warmUp: async () => {
            if (typeof hasher.warmUp !== 'function') return;
            floor.setBaseline(await run(() => hasher.warmUp()));
        },
        limiter,
        floor,
    };
}
