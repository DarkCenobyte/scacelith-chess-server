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
// an unknown user takes as long as one with a wrong password.
//
// Concurrency cap: one hash costs about 0.5-0.6 s of CPU and 64-128 MiB, and the thread pool
// (UV_THREADPOOL_SIZE, 4 threads per process by default) is shared with the journal's fdatasync,
// DNS lookups (SMTP, Google sign-in) and file reads. Without a cap, a burst of logins fills the
// pool of every worker and takes the cores from the game event loops. createHashLimiter() is a
// bounded FIFO semaphore (PASSWORD_HASH_CONCURRENCY running, PASSWORD_HASH_QUEUE_MAX waiting for
// at most PASSWORD_HASH_QUEUE_TIMEOUT_MS); limitHasher() sends every hash and verification of a
// hasher through it, the dummy verification included, so that an unknown account still costs
// the same wait and the same work. The auth service owns one limiter per worker process; a call
// the limiter refuses rejects with PasswordBusyError before any work is done.

import crypto from 'node:crypto';
import fs from 'node:fs';
import { promisify } from 'node:util';
import { metrics } from '../metrics.js';

const scryptAsync = promisify(crypto.scrypt);

const mInFlight = metrics.gauge('scacelith_password_hash_in_flight', 'Password hashes and verifications running (libuv thread pool)', [], { perShard: true });
const mQueued = metrics.gauge('scacelith_password_hash_queued', 'Password hashes and verifications waiting for a slot', [], { perShard: true });
const mWaitMs = metrics.histogram('scacelith_password_hash_wait_ms', 'Time a password hash waited for its slot (granted ones)',
    [1, 10, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 20000]);
const mRejected = metrics.counter('scacelith_password_hash_rejected_total', 'Password hashes refused (503 server_busy)', ['reason']);
const mRejectedFull = mRejected.labels('queue_full');
const mRejectedTimeout = mRejected.labels('timeout');

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
        else return { ok: false, needsRehash: false };     // argon2 hash on a runtime without argon2
        const ok = key.length === h.key.length && crypto.timingSafeEqual(key, h.key);
        return { ok, needsRehash: ok && outdated(h) };
    }

    let dummy = null;
    async function dummyWork(pw) {
        if (!dummy) dummy = hash(crypto.randomBytes(18).toString('base64'));
        const d = parse(await dummy);
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

    /** Computes the dummy hash in advance (so the first unknown login is not faster). */
    function warmUp() {
        if (!dummy) dummy = hash(crypto.randomBytes(18).toString('base64'));
        return dummy.then(() => undefined);
    }

    return { hash, verify, verifyDummy, warmUp, algorithm: preferred, parse };
}

/** A password hash the limiter refused: `reason` is 'queue_full' or 'timeout' (nothing was hashed). */
export class PasswordBusyError extends Error {
    /** @param {'queue_full'|'timeout'} reason */
    constructor(reason) {
        super(reason === 'timeout' ? 'password hashing: the wait for a slot expired' : 'password hashing: the queue is full');
        this.name = 'PasswordBusyError';
        this.code = 'password_busy';
        this.reason = reason;
    }
}

/**
 * Bounded FIFO semaphore for the password hashes of one process.
 *
 * At most `concurrency` tasks run at once; the others wait in arrival order. A task that finds
 * `queueMax` tasks already waiting is refused at once, and a waiting task gives up after
 * `queueTimeoutMs` (it leaves the queue); both reject with PasswordBusyError. A finished task
 * (resolved, rejected or thrown) hands its slot straight to the oldest waiter.
 * @param {{ concurrency?: number, queueMax?: number, queueTimeoutMs?: number }} [opts]
 */
export function createHashLimiter({ concurrency = 1, queueMax = 32, queueTimeoutMs = 10000 } = {}) {
    if (!Number.isInteger(concurrency) || concurrency < 1) throw new RangeError('concurrency must be an integer >= 1');
    if (!Number.isInteger(queueMax) || queueMax < 0) throw new RangeError('queueMax must be an integer >= 0');
    if (!Number.isFinite(queueTimeoutMs) || queueTimeoutMs < 0) throw new RangeError('queueTimeoutMs must be >= 0');
    let active = 0;
    const waiting = [];     // { resolve, reject, since, timer }, oldest first

    // Invariant: `waiting` is empty whenever active < concurrency (release() hands a slot over
    // instead of freeing it while someone waits), so a newcomer never overtakes a waiter.
    function acquire() {
        if (active < concurrency) {
            active++;
            mInFlight.inc();
            mWaitMs.observe(0);
            return Promise.resolve();
        }
        if (waiting.length >= queueMax) {
            mRejectedFull.inc();
            return Promise.reject(new PasswordBusyError('queue_full'));
        }
        return new Promise((resolve, reject) => {
            const w = { resolve, reject, since: performance.now(), timer: null };
            w.timer = setTimeout(() => {
                const i = waiting.indexOf(w);
                if (i < 0) return;
                waiting.splice(i, 1);
                mQueued.dec();
                mRejectedTimeout.inc();
                reject(new PasswordBusyError('timeout'));
            }, queueTimeoutMs);
            waiting.push(w);
            mQueued.inc();
        });
    }

    function release() {
        const w = waiting.shift();
        if (w) {
            clearTimeout(w.timer);
            mQueued.dec();
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
     * @returns {Promise<T>} rejects with PasswordBusyError when refused (fn is then not called)
     */
    async function run(fn) {
        await acquire();
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
    };
}

/**
 * The same hasher with every hash and verification (the dummy one and the warm-up included) run
 * through `limiter`.
 * @param {ReturnType<typeof createPasswordHasher>} hasher
 * @param {ReturnType<typeof createHashLimiter>} limiter
 * @param {{ onBusy?: (err: PasswordBusyError) => Error }} [opts] maps a refusal to the error
 *   thrown instead (the auth service answers 503 server_busy)
 */
export function limitHasher(hasher, limiter, { onBusy = (err) => err } = {}) {
    const run = (fn) => limiter.run(fn).catch((err) => { throw err instanceof PasswordBusyError ? onBusy(err) : err; });
    return {
        algorithm: hasher.algorithm,
        parse: hasher.parse,
        hash: (password) => run(() => hasher.hash(password)),
        verify: (stored, password) => run(() => hasher.verify(stored, password)),
        verifyDummy: (password) => run(() => hasher.verifyDummy(password)),
        warmUp: () => (typeof hasher.warmUp === 'function' ? run(() => hasher.warmUp()) : Promise.resolve()),
        limiter,
    };
}
