// The auth service of a shard (docs/DESIGN.md sections 3, 5.9 and 8): sessions, passwords,
// registration, e-mail confirmation, password reset, MFA (TOTP + recovery codes), Google
// sign-in, brute-force defences, proof of work and security events.
//
//   const auth = createAuth({ config, store, primary, log });
//   auth.validateToken(token)  -> { userId, username, sessionId, emailVerified, tokenHash } | null   (sync, cached 30 s)
//   auth.invalidate({ userId, tokenHashes })  // on the primary's `auth.invalidate` broadcast
//   ... the functions used by the HTTP routes (src/http/routes/*.js)
//   auth.close()                                // flushes the security events
//
// `primary` is the IPC client ({ request(type, payload) -> Promise }); it may be null (single
// process, tests): the rate limits and single-use keys then live in this process. When the
// primary does not answer, the same local implementation is used as a fallback.
//
// Password hashing: `svc.hasher` runs every hash and verification (the dummy one of unknown
// accounts included) through this service's hash limiter, one per worker process
// (PASSWORD_HASH_CONCURRENCY at once, PASSWORD_HASH_QUEUE_MAX waiting, security/password.js).
// Each request gets one budget (svc.hashBudget): all its hashes together wait at most
// PASSWORD_HASH_QUEUE_TIMEOUT_MS, and one client source (an IPv4 address or an IPv6 /48) may have
// at most HASH_WAITERS_PER_SOURCE hashes waiting. A refused hash fails the request before the
// account changed, with 503 server_busy (queue full or wait expired) or 429 rate_limited (too
// many waiting from that source), each with a random Retry-After of 5 to 15 s: no failed login is
// counted and a reset link stays valid (a proof of work already given is spent, as for any
// answer). A new password hash is only written while the stored one is still the hash the
// request checked (svc.setPasswordHashIf, svc.stillCurrent), so a reset always wins a race.

import { createAccounts } from './accounts.js';
import { AuthError, hashRateLimited, serverBusy } from './errors.js';
import { createLogin } from './login.js';
import { createMfa } from './mfa.js';
import { createOidcClient, GOOGLE_OIDC } from './oidc.js';
import { createSessionManager } from './sessions.js';
import { createSso } from './sso.js';
import { createMailer, secretText } from '../mail/index.js';
import { deriveAuthKeys } from '../security/keys.js';
import { createHashLimiter, createPasswordHasher, limitHasher } from '../security/password.js';
import { createPow } from '../security/pow.js';
import { createLocalControl, prefixKey } from '../security/ratelimit.js';
import { createSecurityEvents } from '../security/events.js';
import { createSecretBox } from '../security/totp.js';
import { metrics } from '../metrics.js';

const powTotal = metrics.counter('scacelith_auth_pow_total', 'Proof-of-work answers', ['endpoint', 'result']);

/** Password hashes one client source (an IPv4 address or an IPv6 /48) may have waiting in a worker's queue. */
export const HASH_WAITERS_PER_SOURCE = 2;

/**
 * The public base URL of the server's pages (e-mail links).
 * @param {object} config
 * @returns {string}
 */
export function publicBaseUrl(config) {
    const proto = config.tlsMode === 'off' ? 'http' : 'https';
    const port = config.publicApiPort;
    const def = proto === 'https' ? 443 : 80;
    let host = config.serverPublicHost;
    if (host.includes(':') && !host.startsWith('[')) host = `[${host}]`;
    return `${proto}://${host}${port === def ? '' : ':' + port}`;
}

/**
 * Creates the auth service.
 * @param {{ config: object, store: object, primary?: { request(type: string, payload: object): Promise<object> }|null,
 *           log: object, now?: () => number, mailer?: object, passwordHasher?: object, oidc?: object|null,
 *           oidcEndpoints?: object, oidcAllowHttp?: boolean, events?: object }} opts
 *   `mailer`, `passwordHasher`, `oidc` / `oidcEndpoints` / `oidcAllowHttp` and `events` replace the defaults (tests).
 */
export function createAuth({ config, store, primary = null, log, now = Date.now, mailer, passwordHasher, oidc,
    oidcEndpoints = GOOGLE_OIDC, oidcAllowHttp = false, events }) {
    const keys = deriveAuthKeys(config);
    const local = createLocalControl({ now });
    let controlWarnAt = 0;

    async function control(type, payload) {
        if (!primary) return local.request(type, payload);
        try {
            return await primary.request(type, payload);
        } catch (err) {
            const t = now();
            if (t - controlWarnAt > 60000) { controlWarnAt = t; log.warn('primary unavailable, local fallback', { type, err: { message: err.message } }); }
            return local.request(type, payload);
        }
    }
    async function once(key, ttlMs) {
        const r = await control('once.consume', { key, ttlMs });
        return !!(r && r.fresh);
    }

    const hashLimiter = createHashLimiter({
        concurrency: config.passwordHashConcurrency, queueMax: config.passwordHashQueueMax, queueTimeoutMs: config.passwordHashQueueTimeoutMs,
        perSourceMax: HASH_WAITERS_PER_SOURCE,
    });
    let busyWarnAt = 0;
    function hashBusy(err) {
        // An optional hash that found no free slot (the login's rehash): the caller skips it.
        if (err.reason === 'no_wait') return err;
        // One client with too many hashes waiting: the answer of a rate limit, not a server problem.
        if (err.reason === 'source_limit') return hashRateLimited();
        const t = now();
        if (t - busyWarnAt > 60000) {
            busyWarnAt = t;
            log.warn('password hashing saturated: requests refused with 503 server_busy', { reason: err.reason, ...hashLimiter.stats() });
        }
        return serverBusy();
    }

    const svc = {
        config, store, log, now, primary, keys, control, once,
        hasher: limitHasher(passwordHasher || createPasswordHasher(), hashLimiter, { onBusy: hashBusy }),
        box: createSecretBox(keys.mfa),
        events: events || createSecurityEvents({ store, log, now }),
        mailer: mailer || createMailer({ config, log }),
    };

    /**
     * The hash budget of one request: every hash and verification it makes shares one
     * PASSWORD_HASH_QUEUE_TIMEOUT_MS of waiting, and they count for the client's source (an IPv4
     * address or an IPv6 /48) in the per-source limit of the queue.
     * @param {string|null} ip
     */
    svc.hashBudget = (ip) => {
        const deadline = performance.now() + config.passwordHashQueueTimeoutMs;
        const source = ip ? prefixKey(ip) : null;
        return {
            /** Options of the next hash: what is left of the budget (at least 1 ms, so that a spent budget ends as an ordinary timeout). */
            next: () => ({ maxWaitMs: Math.max(1, Math.ceil(deadline - performance.now())), source }),
            /** Options of an optional hash, run only when a slot is free at once. */
            noWait: () => ({ maxWaitMs: 0, source }),
        };
    };

    // Runs fn in one store transaction when the store has them (the SQLite store: atomic across the
    // processes that share the database; the in-memory test store runs it as it is).
    const atomically = (fn) => (typeof store.transaction === 'function' ? store.transaction(fn) : fn());

    /**
     * Stores `passwordHash` for `userId` only while the stored hash is still `expected` (compare and
     * set): a hash computed from a password checked against `expected` never overwrites a password
     * reset or change that landed while it was computed.
     * @returns {boolean} true when written
     */
    svc.setPasswordHashIf = (userId, expected, passwordHash) => atomically(() => {
        const u = store.users.byId(userId);
        if (!u || u.status !== 'active' || u.passwordHash !== expected) return false;
        store.users.update(userId, { passwordHash });
        return true;
    });

    /**
     * After `password` matched the hash `checked` of `userId`: the account row when the password
     * still matches what is stored now, else null. A check waits in the hash queue and runs for
     * half a second, and a password reset or change (or another login's rehash) may land meanwhile;
     * only then is the password checked again, against the new hash (`hashOpts`: the request's
     * budget). So a login or an account change never succeeds with a password that a reset has
     * already replaced.
     * @returns {Promise<object|null>}
     */
    svc.stillCurrent = async (userId, checked, password, hashOpts) => {
        const u = store.users.byId(userId);
        if (!u || u.status !== 'active' || !u.passwordHash) return null;
        if (u.passwordHash === checked) return u;
        const { ok } = await svc.hasher.verify(u.passwordHash, password, hashOpts);
        if (!ok) return null;
        const again = store.users.byId(userId);
        return again && again.status === 'active' && again.passwordHash === u.passwordHash ? again : null;
    };
    svc.pow = createPow({ key: keys.pow, ipKeyHash: keys.ipHash, now, once });
    svc.links = {
        verify: (token) => `${publicBaseUrl(config)}/verify-email?token=${token}`,
        reset: (token) => `${publicBaseUrl(config)}/reset-password?token=${token}`,
    };
    // Fire-and-forget: a request never waits for an e-mail.
    svc.mail = (template, to, vars) => {
        Promise.resolve()
            .then(() => svc.mailer.sendTemplate(template, to, vars))
            .catch((err) => log.error('e-mail not queued', { template, err: { message: err.message } }));
    };
    svc.accountView = (user) => {
        let googleLinked = !!user.googleLinked;
        if (typeof store.sso.forUser === 'function') {
            try { googleLinked = (store.sso.forUser(user.id) || []).some((l) => l.provider === 'google'); } catch { /* keep default */ }
        }
        return {
            id: user.id, username: user.username, email: user.email, emailVerified: !!user.emailVerified,
            mfaEnabled: !!user.mfaEnabled, googleLinked, hasPassword: !!user.passwordHash,
            acceptChallenges: user.acceptChallenges || 'all', createdAt: user.createdAt ?? null,
        };
    };
    /** Throws 428 pow_required unless `given` is a valid, fresh answer for this endpoint and client. */
    svc.requirePow = async (endpoint, bits, ip, given) => {
        const again = (reason) => {
            powTotal.labels(endpoint, reason).inc();
            return new AuthError(428, 'pow_required', 'Proof of work required.', { reason, pow: svc.pow.issue({ ip, endpoint, bits }) });
        };
        if (!given) {
            svc.events.record('pow_required', { ip, detail: { endpoint, bits } });
            throw again('required');
        }
        const r = await svc.pow.verify({ ip, endpoint, bits, challenge: given.challenge, nonce: given.nonce });
        if (!r.ok) {
            svc.events.record('pow_failed', { ip, detail: { endpoint, reason: r.reason } });
            throw again(r.reason);
        }
        powTotal.labels(endpoint, 'ok').inc();
    };

    svc.sessions = createSessionManager(svc);
    svc.mfa = createMfa(svc);
    svc.login = createLogin(svc);
    svc.accounts = createAccounts(svc);
    if (oidc !== undefined) svc.oidc = oidc;
    else if (config.ssoGoogleEnabled && config.googleClientId) {
        svc.oidc = createOidcClient({
            clientId: config.googleClientId, clientSecret: secretText(config.googleClientSecret),
            redirectUri: config.googleRedirectUri, endpoints: oidcEndpoints, now, allowHttp: oidcAllowHttp,
        });
    } else svc.oidc = null;
    svc.sso = createSso(svc);

    // Prepare the dummy hash now, so that the first login of an unknown user is not faster (it
    // takes a slot of the hash limiter like any other hash).
    Promise.resolve().then(() => svc.hasher.warmUp?.()).catch(() => {});

    const a = svc.accounts, l = svc.login, s = svc.sso;
    return {
        /** Validates a session token (sync). */
        validateToken: (token) => svc.sessions.validate(token),
        /** Drops cached sessions ({ userId, tokenHashes }: empty tokenHashes = every session of the user). */
        invalidate: (payload) => svc.sessions.invalidate(payload || {}),

        register: a.register,
        login: l.login,
        loginMfa: l.loginWithMfa,
        logout: a.logout,
        logoutAll: a.logoutAll,
        listSessions: (ctxUser, currentId) => ({ sessions: svc.sessions.list(ctxUser.userId, currentId) }),
        revokeSession: a.revokeSession,
        resendVerification: a.resendVerification,
        forgotPassword: a.forgotPassword,
        resetPassword: a.resetPassword,
        peekToken: a.peekToken,
        verifyEmail: a.verifyEmail,
        me: a.me,
        changePassword: a.changePassword,
        mfaSetup: a.mfaSetup,
        mfaEnable: a.mfaEnable,
        mfaDisable: a.mfaDisable,
        regenerateRecoveryCodes: a.regenerateRecoveryCodes,
        deleteAccount: a.deleteAccount,
        setPreferences: a.setPreferences,
        sso: { enabled: s.enabled, start: s.start, callback: s.callback, poll: s.poll, complete: s.complete },

        /** True while the login proof of work is on (credential-stuffing wave). */
        loginPowActive: () => l.powActive(),
        mailer: svc.mailer,
        events: svc.events,
        pow: svc.pow,
        /** Flushes pending security events (shutdown). */
        close() { svc.events.close(); },
        _svc: svc,
    };
}
