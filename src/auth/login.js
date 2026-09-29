// Password login, the MFA step, and the brute-force defences around them.
//
//  * Per account: failures are counted per login string (lower case, so the counter exists for
//    unknown logins too and the answers stay identical); from AUTH_FAILURES_PER_ACCOUNT failures
//    on, each attempt must wait exponentially longer (2 s, 4 s, ... up to 15 min): 429
//    too_many_attempts with retryAfter. Counters are per process, memory-bounded (LRU); the
//    per-IP limit of the auth endpoints (router, shared through the primary) and the login
//    proof of work bound what several shards add up to.
//  * Whole server: each failed login is counted locally and through the primary
//    (`ratelimit.take` on a server-wide key with limit POW_LOGIN_TRIGGER_PER_MIN / min). Above the
//    trigger, every login needs a proof of work (POW_LOGIN_BITS) for the next 5 minutes.
//  * Unknown account, wrong password and account without password (Google-only) all do the same
//    hashing work, in the same per-worker hash queue (a refusal there is 503 server_busy, or 429
//    rate_limited for a source with too many waiting, for all of them and counts no failure), and
//    give the same `invalid_credentials` answer after the same time: a failed check is padded to
//    the slowest recent check (security/password.js checkPassword), so a stored hash of another
//    algorithm than the dummy's does not tell that the account exists. `email_unverified` and
//    `banned` are only answered after the password matched.
//  * A matching outdated hash is upgraded only when a hash slot is free at once, and only while the
//    stored hash is unchanged; a login whose password a reset replaced during the check fails.
//  * The MFA step: an `mfa_` token (5 min, 5 wrong codes at most, single use) and a per-account
//    failure counter. The token of a password login holds a digest of the password hash it was
//    issued for (`pwh`); a password reset or change between the two steps makes the step fail
//    (invalid_mfa_token), so no session opens with a password the reset replaced.

import { AuthError, invalidCredentials, tooManyAttempts } from './errors.js';
import { MFA_TOKEN_RE, TOKEN_TTL_MS, dataOf, isLive } from './tokens.js';
import { normalizeEmail } from './identity.js';
import { PasswordBusyError } from '../security/password.js';
import { FailureCounter, SlidingWindowCounter } from '../security/ratelimit.js';
import { randomToken, sha256Hex } from '../security/keys.js';
import { metrics } from '../metrics.js';

export const POW_LOGIN_HOLD_MS = 5 * 60000;
export const MFA_TOKEN_ATTEMPTS = 5;

const loginsTotal = metrics.counter('scacelith_auth_logins_total', 'Login attempts', ['result']);
const loginOk = loginsTotal.labels('ok');
const loginFailed = loginsTotal.labels('failed');
const loginThrottled = loginsTotal.labels('throttled');
const loginMfa = loginsTotal.labels('mfa_required');

/**
 * @param {object} svc the auth service internals (see auth/index.js)
 */
export function createLogin(svc) {
    const { config, store, now, hasher, events, sessions, control, mfa } = svc;
    const failures = new FailureCounter({ threshold: config.authFailuresPerAccount, now });
    const mfaFailures = new FailureCounter({ threshold: 5, now });
    const recentFailures = new SlidingWindowCounter(60000, now);
    let powUntil = 0;

    function powActive() { return config.powLoginBits > 0 && now() < powUntil; }

    function activatePow(source) {
        const was = powActive();
        powUntil = now() + POW_LOGIN_HOLD_MS;
        if (!was && config.powLoginBits > 0) events.record('login_pow_on', { detail: { source } });
    }

    function noteFailure() {
        const trigger = config.powLoginTriggerPerMin;
        if (recentFailures.add(1) >= trigger) activatePow('local');
        control('ratelimit.take', { key: 'auth:login-failures', limit: trigger, windowMs: 60000, cost: 1 })
            .then((r) => { if (r && (r.allowed === false || r.count >= trigger)) activatePow('global'); }, () => {});
    }

    /** Refuses a banned account, or an unconfirmed one when confirmation is required. */
    function checkAccountAllowed(user) {
        const ban = store.sanctions.activeBan(user.id, now());
        if (ban) throw new AuthError(403, 'banned', 'This account is banned.', { until: ban.endsAt ?? null });
        if (config.requireEmailVerification && !user.emailVerified) {
            throw new AuthError(403, 'email_unverified', 'Confirm your e-mail address first: open the link we sent you.');
        }
    }

    /** Opens the session and builds the login answer. */
    function sessionAnswer(user, { clientLabel, ip, method }) {
        const s = sessions.create(user, { clientLabel, ip });
        try { store.users.update(user.id, { lastLoginAt: now() }); } catch { /* informative only */ }
        loginOk.inc();
        events.record('login', { userId: user.id, ip, detail: { method } });
        return { token: s.token, expiresAt: s.expiresAt, user: svc.accountView(store.users.byId(user.id) || user) };
    }

    /** Digest of a stored password hash, kept in an MFA step instead of the hash itself. */
    const passwordHashDigest = (passwordHash) => sha256Hex(passwordHash || '');

    /** After the first factor: the MFA challenge, or the session. */
    function finishLogin(user, { clientLabel = null, ip = null, method = 'password' }) {
        if (user.mfaEnabled) {
            const mfaToken = randomToken('mfa_');
            const data = { attempts: 0, clientLabel, method };
            // A password login's step is only good for the password hash it was issued for.
            if (method === 'password') data.pwh = passwordHashDigest(user.passwordHash);
            store.tokens.create({
                kind: 'mfa_login', tokenHash: sha256Hex(mfaToken), userId: user.id,
                data, expiresAt: now() + TOKEN_TTL_MS.mfa_login,
            });
            loginMfa.inc();
            return { mfaRequired: true, mfaToken, expiresIn: TOKEN_TTL_MS.mfa_login / 1000 };
        }
        return sessionAnswer(user, { clientLabel, ip, method });
    }

    function findLoginUser(login) {
        const l = login.trim();
        const u = store.users.byLogin(l.includes('@') ? normalizeEmail(l) : l);
        return u && u.status === 'active' ? u : null;
    }

    /**
     * POST /auth/login.
     * @param {{ login: string, password: string, clientLabel?: string, pow?: { challenge: string, nonce: string }, ip: string }} p
     */
    async function login({ login: loginName, password, clientLabel = null, pow, ip }) {
        const key = 'l:' + loginName.trim().toLowerCase();
        const wait = failures.retryAfter(key);
        if (wait > 0) {
            loginThrottled.inc();
            events.record('login_throttled', { ip, detail: { retryAfterMs: wait } });
            throw tooManyAttempts(wait);
        }
        if (powActive()) await svc.requirePow('login', config.powLoginBits, ip, pow);
        const budget = svc.hashBudget(ip);
        const user = findLoginUser(loginName);
        const stored = user && user.passwordHash ? user.passwordHash : null;
        // Unknown account or no password: the dummy check, in the same queue, padded alike.
        const { ok, needsRehash } = await hasher.checkPassword(stored, password, budget.next());
        let current = null;
        if (ok) {
            const checked = needsRehash ? await rehash(user.id, stored, password, budget) : stored;
            current = await svc.stillCurrent(user.id, checked, password, budget.next());
        }
        if (!current) {
            const f = failures.fail(key);
            noteFailure();
            loginFailed.inc();
            events.record('login_failed', { userId: user ? user.id : null, ip, detail: { failures: f.failures } });
            if (f.failures === config.authFailuresPerAccount) events.record('login_lockout', { userId: user ? user.id : null, ip, detail: { retryAfterMs: f.retryAfterMs } });
            throw invalidCredentials();
        }
        failures.reset(key);
        checkAccountAllowed(current);
        return finishLogin(current, { clientLabel, ip, method: 'password' });
    }

    /**
     * Upgrades an outdated hash with the password just checked. It runs only when a hash slot is
     * free at once (otherwise the next login does it, and this one does not wait a second time),
     * and it is written only while the stored hash is still the one checked, so that it never
     * overwrites a password reset or change that landed meanwhile.
     * @returns {Promise<string>} the hash the account should now have
     */
    async function rehash(userId, stored, password, budget) {
        try {
            const next = await hasher.hash(password, budget.noWait());
            return svc.setPasswordHashIf(userId, stored, next) ? next : stored;
        } catch (err) {
            if (!(err instanceof PasswordBusyError && err.reason === 'no_wait')) {
                svc.log.warn('password rehash failed', { userId, err: { message: err.message } });
            }
            return stored;
        }
    }

    function invalidMfaToken() {
        return new AuthError(401, 'invalid_mfa_token', 'The login step expired; log in again.');
    }

    /**
     * False when the step was issued for a password hash that is no longer the stored one (a
     * reset or a change landed between the two steps). Steps without `pwh` (a Google sign-in, or
     * one written by an earlier build, which expires within 5 minutes) are not bound.
     */
    function samePassword(data, user) {
        return typeof data.pwh !== 'string' || data.pwh === passwordHashDigest(user.passwordHash);
    }

    /**
     * POST /auth/login/mfa.
     * @param {{ mfaToken: string, code?: string, recoveryCode?: string, ip: string }} p
     */
    async function loginWithMfa({ mfaToken, code, recoveryCode, ip }) {
        if (!MFA_TOKEN_RE.test(mfaToken)) throw invalidMfaToken();
        const h = sha256Hex(mfaToken);
        const row = store.tokens.get('mfa_login', h);
        if (!isLive(row, now())) throw invalidMfaToken();
        const data = dataOf(row);
        const user = store.users.byId(row.userId);
        // Checked before the code, so that a recovery code is not spent on a dead step.
        if (!user || user.status !== 'active' || !user.mfaEnabled || !samePassword(data, user)) {
            store.tokens.consume('mfa_login', h, now());
            throw invalidMfaToken();
        }
        const fkey = 'm' + user.id;
        const wait = mfaFailures.retryAfter(fkey);
        if (wait > 0) throw tooManyAttempts(wait);
        if (!code && !recoveryCode) throw new AuthError(400, 'invalid_request', 'A code or a recovery code is required.');
        const ok = await mfa.checkSecondFactor(user, { code, recoveryCode, allowRecovery: true, ip });
        if (!ok) {
            mfaFailures.fail(fkey);
            const attempts = (data.attempts | 0) + 1;
            if (attempts >= MFA_TOKEN_ATTEMPTS) store.tokens.consume('mfa_login', h, now());
            else store.tokens.update('mfa_login', h, { ...data, attempts });
            events.record('mfa_failed', { userId: user.id, ip, detail: { attempts } });
            throw new AuthError(401, 'invalid_code', 'Wrong or already used code.');
        }
        if (!store.tokens.consume('mfa_login', h, now())) throw invalidMfaToken();
        mfaFailures.reset(fkey);
        const fresh = store.users.byId(user.id) || user;
        // Again after the code check (an await: a reset in this process or in another worker may
        // land meanwhile); the reset wins.
        if (fresh.status !== 'active' || !samePassword(data, fresh)) throw invalidMfaToken();
        checkAccountAllowed(fresh);
        return sessionAnswer(fresh, { clientLabel: data.clientLabel ?? null, ip, method: `${data.method || 'password'}+totp` });
    }

    return {
        login,
        loginWithMfa,
        finishLogin,
        checkAccountAllowed,
        sessionAnswer,
        powActive,
        activatePow,
        failures,
    };
}
