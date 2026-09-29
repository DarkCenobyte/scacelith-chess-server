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
// (PASSWORD_HASH_CONCURRENCY at once, PASSWORD_HASH_QUEUE_MAX waiting, PASSWORD_HASH_QUEUE_TIMEOUT_MS,
// security/password.js). A refused hash fails the request with 503 server_busy and a random
// Retry-After of 5 to 15 s before the account changed: no failed login is counted and a reset
// link stays valid (a proof of work already given is spent, as for any answer).

import { createAccounts } from './accounts.js';
import { AuthError, serverBusy } from './errors.js';
import { createLogin } from './login.js';
import { createMfa } from './mfa.js';
import { createOidcClient, GOOGLE_OIDC } from './oidc.js';
import { createSessionManager } from './sessions.js';
import { createSso } from './sso.js';
import { createMailer, secretText } from '../mail/index.js';
import { deriveAuthKeys } from '../security/keys.js';
import { createHashLimiter, createPasswordHasher, limitHasher } from '../security/password.js';
import { createPow } from '../security/pow.js';
import { createLocalControl } from '../security/ratelimit.js';
import { createSecurityEvents } from '../security/events.js';
import { createSecretBox } from '../security/totp.js';
import { metrics } from '../metrics.js';

const powTotal = metrics.counter('scacelith_auth_pow_total', 'Proof-of-work answers', ['endpoint', 'result']);

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
    });
    let busyWarnAt = 0;
    function hashBusy(err) {
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
