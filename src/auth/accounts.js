// Account flows: registration, e-mail confirmation, password reset and change, account view,
// re-authenticated account changes (MFA, e-mail address, data export, deletion), preferences,
// sessions, logout.
//
// Enumeration resistance: register / resend / forgot give the same answer whatever the e-mail;
// the "e-mail exists" branch of register hashes the password too (same work) and mails the
// owner of the address instead. E-mails are throttled per address (primary `once.consume`, the
// same call made whether the address exists or not).
//
// Pending signups (with REQUIRE_EMAIL_VERIFICATION; store.signups, migration 007): register
// creates no account. The signup (username, address, password hash, the SHA-256 of its link's
// token) waits in pending_signups for the life of the link (24 h) and holds its username, in both
// branches: a new address gets the link (at most one link mail per address every 5 minutes,
// `signup` throttle, consumed once the signup is stored: within that time the new signup's link is
// not mailed), an address that already has an account gets no link (token NULL) and its owner the
// notice. A username held by a live pending signup of another address is refused as a taken one
// (409 username_taken, here and in POST /auth/sso/complete); a new signup with the same address
// replaces the pending one (its username is freed, its link stops working). Nothing else sees a
// pending signup: no account row, so sign-in answers invalid_credentials, the public profile 404,
// a password reset nothing. POST /auth/verify-email/resend gives the address's pending signup its
// 24 h again whether or not the address has an account (the same transaction in both cases, best
// effort: a busy store still answers 202), and a new link (a new token) only when it has none.
// Using the link (POST /verify-email, confirmSignup) creates the account, its address confirmed,
// and deletes the pending signup in one transaction; when another account took the username or the
// address meanwhile (Google sign-in), the signup is dropped and the page says so. Expired pending
// signups free their username at once and are deleted by the retention purge. Accounts created
// unconfirmed before pending signups existed keep the `email_verify` links and the 403
// email_unverified sign-in answer.
//
// E-mail change (POST /account/email { newEmail, password, code?, recoveryCode? }; the
// re-authentication of deletion: password, plus a code or a recovery code when MFA is on):
//  * 400 invalid_email / same_email before the password is checked (the session already shows
//    the address).
//  * With REQUIRE_EMAIL_VERIFICATION: 202 { status: 'verification_sent' } whether or not another
//    account uses the address. An `email_change` token (24 h, single use, data { email, from })
//    replaces the user's pending change either way, so that GET /account/me shows the same
//    pendingEmail; the new address gets the confirmation link (GET /confirm-email-change shows a
//    button, POST applies; at most one link mail per address every 5 minutes: within that time a
//    request keeps its pending change to the same address, whose link was mailed, and mails none),
//    or, when it belongs to another account, its owner gets the registrationAttempt notice
//    instead (at most one per hour per address; both throttles' `once.consume` run in both
//    cases). The current address is told that a change to the masked new address was requested
//    (emailChangeRequested). The requester's security event is the same in both cases
//    (email_change_requested); the owner's (email_change_existing_email) has no IP.
//  * Confirmation (confirmEmailChange): the token is consumed; refused ('invalid') when the
//    account's address is no longer the one of the request, refused ('taken') when another account
//    took the new address meanwhile (the UNIQUE index decides, atomically); otherwise the address
//    changes, emailVerified becomes true, the cached sessions are dropped on every shard (the
//    other sessions stay signed in: the password did not change), the links sent to the former
//    address (email_verify, password_reset, other email_change tokens) stop working, an
//    email_changed security event is recorded and the former address gets emailChanged. The use
//    of the link, the new address and the end of the former address's links are one transaction
//    (a busy store: 503 server_busy, nothing changed, the link still works); a password reset
//    link also has to match the account's current address when it is used, so one that another
//    shard stored for the former address while the change committed does not work either.
//  * Without REQUIRE_EMAIL_VERIFICATION: the change happens at once, 200 { status:
//    'email_changed', email }, or 409 email_taken (the deviation of register, below; the owner
//    still gets the notice), and the former address is still told (the same transaction).
//  * A password change or reset cancels a pending e-mail change (whoever requested it knew the
//    password the owner just replaced). A request racing it gets 403 invalid_password: its token
//    (or its immediate change) is written only while the password it checked is still the stored
//    one, in the same transaction (compare and set, as POST /account/password does). It also ends
//    the account's other password reset links (one of them would replace the new password).
//
// Data export (POST /account/export, same re-authentication): exportAccount() checks the
// credentials, lets the caller build the document (http/routes/account-export.js) and records
// an account_exported security event.
//
// Deviations (also in docs/API.md sections 4 and 6, and docs/DESIGN.md section 8):
//  * REQUIRE_EMAIL_VERIFICATION=false: an account is ready at once (201 { status: 'ready' }), with
//    no pending signup, and an existing e-mail is answered 409 email_taken (without confirmation
//    e-mails the "same answer" would only hide the failure from honest users; the owner still gets
//    the notice e-mail).
//  * POST /account/password requires the current password only (the client contract
//    changePassword(current, next) sends no code); PUT /account/preferences requires only the
//    session. Deletion, MFA disable and recovery-code regeneration require password + second factor.
//  * A password reset also marks the e-mail address as confirmed (the link proved it).

import crypto from 'node:crypto';
import { AuthError, serverBusy, tooManyAttempts } from './errors.js';
import { LINK_TOKEN_RE, TOKEN_TTL_MS, dataOf, isLive } from './tokens.js';
import { checkUsername, isValidEmail, maskEmail, normalizeEmail } from './identity.js';
import { checkPasswordPolicy } from '../security/password.js';
import { FailureCounter } from '../security/ratelimit.js';
import { randomToken, sha256Hex } from '../security/keys.js';

const MAIL_THROTTLE_MS = 5 * 60000;
const NOTICE_THROTTLE_MS = 60 * 60000;

/**
 * @param {object} svc the auth service internals (see auth/index.js)
 */
export function createAccounts(svc) {
    const { config, store, now, hasher, events, sessions, mfa, keys, once } = svc;
    const reauthFailures = new FailureCounter({ threshold: config.authFailuresPerAccount, now });

    const mailKey = (kind, email) => `mail:${kind}:` + crypto.createHmac('sha256', keys.mailThrottle).update(email).digest('base64url').slice(0, 24);

    function weak(policy) {
        return new AuthError(400, 'weak_password', policy.message, { reason: policy.reason });
    }

    function activeUser(id) {
        const u = store.users.byId(id);
        if (!u || u.status !== 'active') throw new AuthError(401, 'invalid_token', 'The session is invalid; log in again.');
        return u;
    }

    function sendVerification(user) {
        const token = randomToken('', 32);
        store.tokens.create({ kind: 'email_verify', tokenHash: sha256Hex(token), userId: user.id, data: { email: user.email }, expiresAt: now() + TOKEN_TTL_MS.email_verify });
        svc.mail('verification', user.email, { username: user.username, link: svc.links.verify(token), hours: TOKEN_TTL_MS.email_verify / 3600000 });
    }

    // Whether a live pending signup of another address holds `username` (it is then refused as a
    // taken one; the signup's own address may sign up again with it, which replaces the signup).
    function usernameHeld(username, email) {
        const p = store.signups.byUsername(username);
        return !!p && p.expiresAt > now() && normalizeEmail(p.email) !== normalizeEmail(email);
    }

    // Stores the pending signup of `em` (replacing the address's previous one), in one transaction
    // with the checks of its username. false: an account or a live pending signup of another
    // address took the username since the first check.
    function holdSignup({ username, em, passwordHash, token }) {
        return atomicallyOrBusy(() => {
            const t = now();
            if (store.users.byUsername(username) || usernameHeld(username, em)) return false;
            for (const p of [store.signups.byUsername(username), store.signups.byEmail(em)]) if (p) store.signups.delete(p.id);
            store.signups.create({ username, email: em, passwordHash, tokenHash: token ? sha256Hex(token) : null, createdAt: t,
                expiresAt: t + TOKEN_TTL_MS.email_verify });
            return true;
        });
    }

    function mailSignupLink(username, email, token) {
        svc.mail('verification', email, { username, link: svc.links.verify(token), hours: TOKEN_TTL_MS.email_verify / 3600000 });
    }

    async function notifyExistingAddress(user, ip) {
        events.record('register_existing_email', { userId: user.id, ip });
        if (await once(mailKey('regattempt', user.email), NOTICE_THROTTLE_MS)) {
            svc.mail('registrationAttempt', user.email, { username: user.username });
        }
    }

    // The owner of an address another player asked to move their account to. `fresh`: the hourly
    // throttle of this notice (the once.consume the caller made for the address, taken or not).
    // No IP in the owner's event: it would be the other player's.
    function notifyAddressClaimed(owner, fresh) {
        events.record('email_change_existing_email', { userId: owner.id });
        if (fresh) svc.mail('registrationAttempt', owner.email, { username: owner.username, emailChange: true });
    }

    /**
     * POST /auth/register.
     * @returns {Promise<{ status: number, body: object }>}
     */
    async function register({ username, email, password, pow, ip }) {
        if (config.registration !== 'open') throw new AuthError(403, 'registration_closed', 'Registration is closed on this server.');
        const uErr = checkUsername(username, config);
        if (uErr) throw new AuthError(400, 'invalid_username', uErr);
        const em = normalizeEmail(email);
        if (!isValidEmail(em)) throw new AuthError(400, 'invalid_email', 'This e-mail address is not valid.');
        const policy = checkPasswordPolicy(password, { minLength: config.passwordMinLength, username, email: em });
        if (policy) throw weak(policy);
        const taken = () => new AuthError(409, 'username_taken', 'This username is already taken.');
        if (store.users.byUsername(username) || usernameHeld(username, em)) throw taken();
        if (config.powRegisterBits > 0) await svc.requirePow('register', config.powRegisterBits, ip, pow);

        const existing = store.users.byEmail(em);
        const passwordHash = await hasher.hash(password, svc.hashBudget(ip).next());     // also for an existing address: same work
        if (config.requireEmailVerification) {
            // No account before the link is used: a pending signup holds the username in both
            // branches, so that the answer, a second signup with that username, a sign-in and the
            // public profile are the same whether or not the address has an account (header). The
            // mail throttle is consumed only once the signup is stored (a busy store or a lost race
            // leaves it for the retry).
            const token = existing ? null : randomToken('', 32);
            if (!holdSignup({ username, em, passwordHash, token })) throw taken();
            if (existing) await notifyExistingAddress(existing, ip);
            else if (await once(mailKey('signup', em), MAIL_THROTTLE_MS)) mailSignupLink(username, em, token);
            return { status: 202, body: { status: 'verification_sent' } };
        }
        if (existing) {
            await notifyExistingAddress(existing, ip);
            throw new AuthError(409, 'email_taken', 'An account already uses this e-mail address.');
        }
        let id;
        try {
            id = store.users.create({ username, email: em, passwordHash, emailVerified: true });
        } catch (err) {
            if (err && err.code === 'username_taken') throw taken();
            if (err && err.code === 'email_taken') {
                const owner = store.users.byEmail(em);
                if (owner) await notifyExistingAddress(owner, ip);
                throw new AuthError(409, 'email_taken', 'An account already uses this e-mail address.');
            }
            throw err;
        }
        events.record('register', { userId: id, ip });
        return { status: 201, body: { status: 'ready' } };
    }

    /**
     * A live token row of `kind` for a link token, or null (does not consume it). A password reset
     * link also needs the account to have the address it was mailed to (sentToCurrentAddress).
     */
    function peekToken(kind, token) {
        if (typeof token !== 'string' || !LINK_TOKEN_RE.test(token)) return null;
        const row = store.tokens.get(kind, sha256Hex(token));
        if (!isLive(row, now())) return null;
        return kind !== 'password_reset' || sentToCurrentAddress(row) ? row : null;
    }

    // Whether the account of a link token (data { email }) is active and still has the address the
    // link was mailed to. A link of a former address is refused even when the purge of those links
    // missed it (a request on another shard that read the account just before the change).
    function sentToCurrentAddress(row) {
        const user = store.users.byId(row.userId);
        return !!user && user.status === 'active' && normalizeEmail(user.email) === normalizeEmail(dataOf(row).email);
    }

    // svc.atomically; a busy store (another process held the write lock past the busy timeout, so
    // the transaction did not start or was rolled back) is answered 503 server_busy: nothing
    // changed, and the same request or link can be sent again.
    function atomicallyOrBusy(fn) {
        try {
            return svc.atomically(fn);
        } catch (err) {
            if (err && err.code === 'busy' && !err.expose) throw serverBusy(1);
            throw err;
        }
    }

    /**
     * A live pending signup of a link token, or null (does not use it).
     */
    function liveSignup(token) {
        if (typeof token !== 'string' || !LINK_TOKEN_RE.test(token)) return null;
        const p = store.signups.byTokenHash(sha256Hex(token));
        return p && p.expiresAt > now() ? p : null;
    }

    /** Whether a confirmation link (an account's, or a pending signup's) is live; GET /verify-email. */
    function peekVerification(token) {
        return !!(peekToken('email_verify', token) || liveSignup(token));
    }

    /**
     * POST /verify-email: consumes the token and confirms the address, in one transaction (a busy
     * store: 503 server_busy, the link still works). The link of a pending signup creates its
     * account instead (confirmSignup).
     * @returns {'confirmed'|'invalid'|'taken'}
     */
    function verifyEmail(token, ip = null) {
        if (typeof token !== 'string' || !LINK_TOKEN_RE.test(token)) return 'invalid';
        const user = atomicallyOrBusy(() => {
            const row = store.tokens.consume('email_verify', sha256Hex(token), now());
            if (!row || (row.expiresAt != null && row.expiresAt <= now())) return null;
            const u = store.users.byId(row.userId);
            if (!u || u.status !== 'active' || normalizeEmail(u.email) !== normalizeEmail(dataOf(row).email)) return null;
            if (!u.emailVerified) store.users.update(u.id, { emailVerified: true });
            return u;
        });
        if (!user) return confirmSignup(token, ip);
        sessions.invalidate({ userId: user.id });
        events.record('email_verified', { userId: user.id, ip });
        return 'confirmed';
    }

    // The link of a pending signup: its account is created, the address confirmed, and the signup
    // deleted, in one transaction (a busy store: 503 server_busy, nothing changed, the link still
    // works). 'taken': another account took the username or the address since the signup (the
    // signup is dropped).
    function confirmSignup(token, ip) {
        const r = atomicallyOrBusy(() => {
            const p = liveSignup(token);
            if (!p) return { status: 'invalid' };
            store.signups.delete(p.id);
            if (store.users.byUsername(p.username) || store.users.byEmail(p.email)) return { status: 'taken' };
            try {
                const id = store.users.create({ username: p.username, email: p.email, passwordHash: p.passwordHash, emailVerified: true,
                    createdAt: now() });
                return { status: 'confirmed', id };
            } catch (err) {
                if (err && (err.code === 'username_taken' || err.code === 'email_taken')) return { status: 'taken' };
                throw err;
            }
        });
        if (r.status === 'confirmed') events.record('register', { userId: r.id, ip });
        return r.status;
    }

    /** POST /auth/verify-email/resend: 202 whatever happens. */
    async function resendVerification({ email, ip }) {
        const em = normalizeEmail(email);
        const fresh = await once(mailKey('verify', em), MAIL_THROTTLE_MS);
        if (!fresh || !isValidEmail(em)) return { status: 'accepted' };
        const user = store.users.byEmail(em);
        if (user && user.status === 'active' && !user.emailVerified) {
            sendVerification(user);
            events.record('verification_resent', { userId: user.id, ip });
        }
        // The address's pending signup, if any, gets its 24 h again whether or not the address has
        // an account (how long its username stays held must not tell), with the same transaction in
        // both cases; a new link replaces the previous one only when the address has no account (as
        // at signup). Best effort: a busy store renews nothing and the answer is still 202.
        const token = randomToken('', 32);
        let p = null;
        try {
            p = svc.atomically(() => {
                const row = store.signups.byEmail(em);
                if (!row || row.expiresAt <= now()) return null;
                store.signups.renew(row.id, { tokenHash: user ? row.tokenHash : sha256Hex(token), expiresAt: now() + TOKEN_TTL_MS.email_verify });
                return row;
            });
        } catch (err) {
            if (!(err && err.code === 'busy' && !err.expose)) throw err;
        }
        if (p && !user) mailSignupLink(p.username, p.email, token);
        return { status: 'accepted' };
    }

    /** POST /auth/password/forgot: 202 whatever happens. */
    async function forgotPassword({ email, ip }) {
        const em = normalizeEmail(email);
        const fresh = await once(mailKey('reset', em), MAIL_THROTTLE_MS);
        const user = isValidEmail(em) ? store.users.byEmail(em) : null;
        if (fresh && user && user.status === 'active') {
            const token = randomToken('', 32);
            store.tokens.create({ kind: 'password_reset', tokenHash: sha256Hex(token), userId: user.id, data: { email: user.email }, expiresAt: now() + TOKEN_TTL_MS.password_reset });
            svc.mail('passwordReset', user.email, { username: user.username, link: svc.links.reset(token), minutes: TOKEN_TTL_MS.password_reset / 60000 });
            events.record('password_reset_requested', { userId: user.id, ip });
        }
        return { status: 'accepted' };
    }

    function invalidResetToken() {
        return new AuthError(400, 'invalid_token', 'This reset link is invalid, was already used, or has expired.');
    }

    /**
     * POST /auth/password/reset (and the HTML form). Revokes every session; never touches MFA.
     * The link works only while the account has the address it was mailed to (a change of
     * address ends the links of the former one).
     */
    async function resetPassword({ token, newPassword, ip }) {
        const row = peekToken('password_reset', token);
        if (!row) throw invalidResetToken();
        const user = store.users.byId(row.userId);
        if (!user || user.status !== 'active') throw invalidResetToken();
        const policy = checkPasswordPolicy(newPassword, { minLength: config.passwordMinLength, username: user.username, email: user.email });
        if (policy) throw weak(policy);
        const passwordHash = await hasher.hash(newPassword, svc.hashBudget(ip).next());
        // One transaction: no change of address can land between the check of the link's address
        // and the new password, and the pending e-mail change and the other reset links go with the
        // old password.
        const done = atomicallyOrBusy(() => {
            const used = store.tokens.consume('password_reset', sha256Hex(token), now());
            if (!used || !sentToCurrentAddress(used)) return false;
            store.users.update(user.id, { passwordHash, emailVerified: true });
            cancelEmailChange(user.id);
            dropResetLinks(user.id);
            return true;
        });
        if (!done) throw invalidResetToken();
        sessions.revokeAll(user.id);
        events.record('password_reset', { userId: user.id, ip });
        svc.mail('passwordChanged', user.email, { username: user.username, when: new Date(now()), byReset: true });
        return { status: 'password_reset' };
    }

    /**
     * Re-authentication for account changes: the password and, when MFA is on and
     * `secondFactor` is not 'none', a TOTP code ('totp') or a code or recovery code ('any').
     * `budget` is the request's hash budget (auth/index.js hashBudget), for a request that hashes
     * again afterwards.
     * @returns {Promise<object>} the user row; its passwordHash is the hash the password matched
     */
    async function reauth(userId, { password, code, recoveryCode }, { secondFactor = 'any', ip = null, budget = svc.hashBudget(ip) } = {}) {
        const key = 'r' + userId;
        const wait = reauthFailures.retryAfter(key);
        if (wait > 0) throw tooManyAttempts(wait);
        const user = activeUser(userId);
        if (!user.passwordHash) {
            await hasher.verifyDummy(password || '', budget.next());
            throw new AuthError(400, 'password_not_set', 'This account has no password yet; set one with "Forgot password" first.');
        }
        const { ok } = await hasher.verify(user.passwordHash, password, budget.next());
        // The password may have been reset or changed while the check waited and ran.
        const current = ok ? await svc.stillCurrent(userId, user.passwordHash, password, budget.next()) : null;
        if (!current) {
            reauthFailures.fail(key);
            events.record('reauth_failed', { userId, ip, detail: { factor: 'password' } });
            throw new AuthError(403, 'invalid_password', 'Wrong password.');
        }
        if (secondFactor !== 'none' && current.mfaEnabled) {
            if (!code && !recoveryCode) throw new AuthError(403, 'mfa_code_required', 'Enter a code of your authenticator app.');
            const ok2 = await mfa.checkSecondFactor(current, { code, recoveryCode, allowRecovery: secondFactor === 'any', ip });
            if (!ok2) {
                reauthFailures.fail(key);
                events.record('reauth_failed', { userId, ip, detail: { factor: 'mfa' } });
                throw new AuthError(403, 'invalid_code', 'Wrong or already used code.');
            }
            reauthFailures.reset(key);
            // The row after the second factor (its MFA state), unless the password changed meanwhile.
            const after = store.users.byId(userId);
            return after && after.passwordHash === current.passwordHash ? after : current;
        }
        reauthFailures.reset(key);
        return current;
    }

    /** POST /account/password: revokes the other sessions. */
    async function changePassword(ctxUser, sessionId, { currentPassword, newPassword, ip }) {
        // Both hashes of the request (the check of the current password, the hash of the new one)
        // share one queue timeout, so that the answer comes before the game gives up.
        const budget = svc.hashBudget(ip);
        const user = await reauth(ctxUser.userId, { password: currentPassword }, { secondFactor: 'none', ip, budget });
        const policy = checkPasswordPolicy(newPassword, { minLength: config.passwordMinLength, username: user.username, email: user.email });
        if (policy) throw weak(policy);
        const next = await hasher.hash(newPassword, budget.next());
        // Written only over the hash the current password matched: a reset or another change that
        // landed meanwhile wins (a login's rehash of the same password does not count as a change).
        if (!svc.setPasswordHashIf(user.id, user.passwordHash, next)) {
            const fresh = await svc.stillCurrent(user.id, user.passwordHash, currentPassword, budget.next());
            if (!fresh || !svc.setPasswordHashIf(user.id, fresh.passwordHash, next)) throw new AuthError(403, 'invalid_password', 'Wrong password.');
        }
        sessions.revokeAll(user.id, sessionId);
        cancelEmailChange(user.id);
        dropResetLinks(user.id);
        events.record('password_changed', { userId: user.id, ip });
        svc.mail('passwordChanged', user.email, { username: user.username, when: new Date(now()), byReset: false });
        return { status: 'password_changed' };
    }

    /** GET /account/me. */
    function me(userId) {
        const user = activeUser(userId);
        const t = now();
        const ratings = (store.ratings.forUser(userId) || []).map((r) => ({
            category: r.category, rating: r.rating, games: r.games, wins: r.wins, draws: r.draws, losses: r.losses,
            peak: r.peak, provisional: r.provisional,
        }));
        const sanctions = (store.sanctions.list(userId) || [])
            .filter((s) => !s.liftedAt && (s.startsAt ?? 0) <= t && (s.endsAt == null || s.endsAt > t))
            .map((s) => ({ kind: s.kind, reason: s.reason ?? null, startsAt: s.startsAt ?? null, endsAt: s.endsAt ?? null }));
        const ban = store.sanctions.activeBan(userId, t);
        return { user: svc.accountView(user), ratings, sanctions, ban: ban ? { until: ban.endsAt ?? null } : null };
    }

    /** POST /account/mfa/totp/setup. */
    async function mfaSetup(ctxUser, { password, ip }) {
        const pre = activeUser(ctxUser.userId);
        if (pre.mfaEnabled) throw new AuthError(409, 'mfa_already_enabled', 'Two-step verification is already enabled.');
        const user = await reauth(ctxUser.userId, { password }, { secondFactor: 'none', ip });
        events.record('mfa_setup_started', { userId: user.id, ip });
        return mfa.setup(user);
    }

    /** POST /account/mfa/totp/enable. */
    function mfaEnable(ctxUser, { code, ip }) {
        const key = 'r' + ctxUser.userId;
        const wait = reauthFailures.retryAfter(key);
        if (wait > 0) throw tooManyAttempts(wait);
        const user = activeUser(ctxUser.userId);
        const r = mfa.enable(user, code);
        if (!r.ok) {
            reauthFailures.fail(key);
            throw new AuthError(403, 'invalid_code', 'Wrong code; check the time of your device and try again.');
        }
        reauthFailures.reset(key);
        events.record('mfa_enabled', { userId: user.id, ip });
        return { status: 'mfa_enabled', recoveryCodes: r.recoveryCodes };
    }

    /** POST /account/mfa/totp/disable. */
    async function mfaDisable(ctxUser, { password, code, recoveryCode, ip }) {
        const pre = activeUser(ctxUser.userId);
        if (!pre.mfaEnabled) throw new AuthError(409, 'mfa_not_enabled', 'Two-step verification is not enabled.');
        if (!code && !recoveryCode) throw new AuthError(403, 'mfa_code_required', 'Enter a code of your authenticator app or a recovery code.');
        const user = await reauth(ctxUser.userId, { password, code, recoveryCode }, { secondFactor: 'any', ip });
        mfa.disable(user);
        events.record('mfa_disabled', { userId: user.id, ip });
        svc.mail('mfaDisabled', user.email, { username: user.username, when: new Date(now()) });
        return { status: 'mfa_disabled' };
    }

    /** POST /account/mfa/recovery-codes. */
    async function regenerateRecoveryCodes(ctxUser, { password, code, ip }) {
        const pre = activeUser(ctxUser.userId);
        if (!pre.mfaEnabled) throw new AuthError(409, 'mfa_not_enabled', 'Two-step verification is not enabled.');
        const user = await reauth(ctxUser.userId, { password, code }, { secondFactor: 'totp', ip });
        const recoveryCodes = mfa.regenerate(user);
        events.record('recovery_codes_regenerated', { userId: user.id, ip });
        return { recoveryCodes };
    }

    /**
     * POST /account/delete: anonymises the account and revokes every session. The security events
     * still waiting in this process's batch (this request's recovery_code_used, the last second's
     * logins) are written first, so that the anonymisation erases their IP addresses too; the
     * deletion's own event, recorded once it is done, has none.
     */
    async function deleteAccount(ctxUser, { password, code, recoveryCode, ip }) {
        const user = await reauth(ctxUser.userId, { password, code, recoveryCode }, { secondFactor: 'any', ip });
        events.flush();
        store.users.anonymize(user.id);
        sessions.revokeAll(user.id);
        events.record('account_deleted', { userId: user.id });
        return { status: 'deleted' };
    }

    /** PUT /account/preferences ('all' | 'none', stored as the Store's boolean). */
    function setPreferences(ctxUser, { acceptChallenges }) {
        const user = activeUser(ctxUser.userId);
        store.users.update(user.id, { acceptChallenges: acceptChallenges !== 'none' });
        return { preferences: { acceptChallenges } };
    }

    // ---- e-mail change (header) ----

    /** Drops the user's pending e-mail change (its link stops working). */
    function cancelEmailChange(userId) {
        store.tokens.deleteForUser(userId, 'email_change');
    }

    /** Drops the user's password reset links (after a password reset or change). */
    function dropResetLinks(userId) {
        store.tokens.deleteForUser(userId, 'password_reset');
    }

    function emailTaken() {
        return new AuthError(409, 'email_taken', 'An account already uses this e-mail address.');
    }

    // The links sent to the former address stop working: run in the transaction that changes the
    // address, so that the change never commits without it.
    function dropAddressLinks(userId) {
        for (const kind of ['email_change', 'password_reset', 'email_verify']) store.tokens.deleteForUser(userId, kind);
    }

    // Once the address of `user` changed from `from` to `email` (committed): the cached sessions are
    // read again, the change is recorded, the former address is told.
    function emailChanged(user, from, email, ip) {
        sessions.refresh(user.id);
        events.record('email_changed', { userId: user.id, ip });
        if (from) svc.mail('emailChanged', from, { username: user.username, maskedEmail: maskEmail(email), when: new Date(now()) });
    }

    /**
     * POST /account/email (header).
     * @returns {Promise<{ status: number, body: object }>}
     */
    async function changeEmail(ctxUser, { newEmail, password, code, recoveryCode, ip }) {
        const pre = activeUser(ctxUser.userId);
        const em = normalizeEmail(newEmail);
        if (!isValidEmail(em)) throw new AuthError(400, 'invalid_email', 'This e-mail address is not valid.');
        const same = () => new AuthError(400, 'same_email', 'This is already the e-mail address of your account.');
        if (normalizeEmail(pre.email) === em) throw same();
        const budget = svc.hashBudget(ip);
        const user = await reauth(ctxUser.userId, { password, code, recoveryCode }, { secondFactor: 'any', ip, budget });
        if (normalizeEmail(user.email) === em) throw same();
        const from = user.email || null;
        const holder = store.users.byEmail(em);
        const owner = holder && holder.id !== user.id ? holder : null;

        // Runs write() in one transaction, only while the stored password is still the one the
        // request proved (compare and set, as changePassword): a password reset or change that
        // landed since the check, on any shard (the request waits for the primary below), wins and
        // the request is refused as with a wrong password; a sign-in's rehash of the same password
        // is checked once against the new hash and is no change.
        const withPassword = async (write) => {
            const attempt = (hash) => atomicallyOrBusy(() => {
                const u = store.users.byId(user.id);
                if (!u || u.status !== 'active' || u.passwordHash !== hash) return false;
                write();
                return true;
            });
            if (attempt(user.passwordHash)) return;
            const again = await svc.stillCurrent(user.id, user.passwordHash, password, budget.next());
            if (!again || !attempt(again.passwordHash)) throw new AuthError(403, 'invalid_password', 'Wrong password.');
        };

        if (!config.requireEmailVerification) {
            const claimed = async (o) => notifyAddressClaimed(o, await once(mailKey('emailchange', em), NOTICE_THROTTLE_MS));
            if (owner) {
                await claimed(owner);
                throw emailTaken();
            }
            try {
                await withPassword(() => {
                    store.users.update(user.id, { email: em, emailVerified: true });
                    dropAddressLinks(user.id);
                });
            } catch (err) {
                if (err && err.code === 'email_taken') {
                    const o = store.users.byEmail(em);
                    if (o && o.id !== user.id) await claimed(o);
                    throw emailTaken();
                }
                throw err;
            }
            emailChanged(user, from, em, ip);
            return { status: 200, body: { status: 'email_changed', email: em } };
        }

        // The same work and the same answer whether or not the address is free.
        const fresh = await once(mailKey('emailchange', em), NOTICE_THROTTLE_MS);
        // One confirmation link per new address every 5 minutes, as for the other link mails (the
        // address is anyone's: no flood of a stranger's inbox). Within that time the request keeps
        // its pending change to that address, whose link was mailed already, and mails nothing.
        const linkFresh = await once(mailKey('emailchange-link', em), MAIL_THROTTLE_MS);
        const token = randomToken('', 32);
        await withPassword(() => {
            if (!linkFresh) {
                const live = dataOf(store.tokens.liveForUser(user.id, 'email_change', now()));
                if (normalizeEmail(live.email) === em && normalizeEmail(live.from) === normalizeEmail(from)) return;
            }
            cancelEmailChange(user.id);
            store.tokens.create({
                kind: 'email_change', tokenHash: sha256Hex(token), userId: user.id, data: { email: em, from },
                expiresAt: now() + TOKEN_TTL_MS.email_change,
            });
        });
        events.record('email_change_requested', { userId: user.id, ip });
        const hours = TOKEN_TTL_MS.email_change / 3600000;
        if (owner) notifyAddressClaimed(owner, fresh);
        else if (linkFresh) svc.mail('emailChangeConfirm', em, { username: user.username, link: svc.links.emailChange(token), hours });
        if (from) svc.mail('emailChangeRequested', from, { username: user.username, maskedEmail: maskEmail(em), when: new Date(now()), hours });
        return { status: 202, body: { status: 'verification_sent' } };
    }

    /**
     * A live e-mail change link's account and new address, without consuming it (the page
     * GET /confirm-email-change), or null.
     * @returns {{ username: string, email: string }|null}
     */
    function peekEmailChange(token) {
        const row = peekToken('email_change', token);
        if (!row) return null;
        const data = dataOf(row);
        const user = store.users.byId(row.userId);
        if (!user || user.status !== 'active' || normalizeEmail(user.email) !== normalizeEmail(data.from)) return null;
        return { username: user.username, email: data.email };
    }

    /**
     * POST /confirm-email-change: consumes the link's token and applies the change (header).
     * @returns {{ status: 'changed', email: string } | { status: 'invalid' } | { status: 'taken' }}
     */
    function confirmEmailChange(token, ip = null) {
        const invalid = { status: 'invalid' };
        if (typeof token !== 'string' || !LINK_TOKEN_RE.test(token)) return invalid;
        // One transaction: the link is used up, the address changes and the links of the former
        // address are dropped together, or nothing happens (a busy store: 503, the link still works).
        const r = atomicallyOrBusy(() => {
            const row = store.tokens.consume('email_change', sha256Hex(token), now());
            if (!row || (row.expiresAt != null && row.expiresAt <= now())) return invalid;
            const data = dataOf(row);
            const em = normalizeEmail(data.email);
            const user = store.users.byId(row.userId);
            if (!user || user.status !== 'active' || !isValidEmail(em)) return invalid;
            // The account's address changed since the request (another change, a moderator): stale.
            if (normalizeEmail(user.email) !== normalizeEmail(data.from)) return invalid;
            const holder = store.users.byEmail(em);
            if (holder && holder.id !== user.id) return { status: 'taken', user };
            try {
                store.users.update(user.id, { email: em, emailVerified: true });
            } catch (err) {
                // The UNIQUE index refused it: the link stays used up (the transaction commits).
                if (err && err.code === 'email_taken') return { status: 'taken', user };
                throw err;
            }
            dropAddressLinks(user.id);
            return { status: 'changed', email: em, user };
        });
        if (r.status === 'taken') {
            events.record('email_change_refused', { userId: r.user.id, ip, detail: { reason: 'email_taken' } });
            return { status: 'taken' };
        }
        if (r.status !== 'changed') return invalid;
        emailChanged(r.user, r.user.email, r.email, ip);
        return { status: 'changed', email: r.email };
    }

    /**
     * POST /account/export: re-authenticates, then `build(user)` makes the document (it may be
     * async); records account_exported.
     * @param {object} ctxUser
     * @param {{ password: string, code?: string, recoveryCode?: string, ip?: string|null }} creds
     * @param {(user: object) => object|Promise<object>} build
     */
    async function exportAccount(ctxUser, { password, code, recoveryCode, ip }, build) {
        const user = await reauth(ctxUser.userId, { password, code, recoveryCode }, { secondFactor: 'any', ip });
        const doc = await build(user);
        events.record('account_exported', { userId: user.id, ip });
        return doc;
    }

    /** POST /auth/logout. */
    function logout(ctxUser, session, ip = null) {
        sessions.revoke(ctxUser.userId, session.id, session.tokenHash || null);
        events.record('session_revoked', { userId: ctxUser.userId, ip, detail: { reason: 'logout' } });
        return { status: 'logged_out' };
    }

    /** POST /auth/logout-all. */
    function logoutAll(ctxUser, ip = null) {
        sessions.revokeAll(ctxUser.userId);
        events.record('sessions_revoked_all', { userId: ctxUser.userId, ip, detail: { reason: 'logout_all' } });
        return { status: 'logged_out' };
    }

    /** DELETE /auth/sessions/:id. */
    function revokeSession(ctxUser, id, ip = null) {
        const row = sessions.find(ctxUser.userId, id);
        if (!row) throw new AuthError(404, 'not_found', 'No such session.');
        sessions.revoke(ctxUser.userId, row.id);
        events.record('session_revoked', { userId: ctxUser.userId, ip, detail: { reason: 'user' } });
        return { status: 'revoked' };
    }

    return {
        register, usernameHeld, peekToken, peekVerification, verifyEmail, resendVerification, forgotPassword, resetPassword, reauth,
        changePassword, me, mfaSetup, mfaEnable, mfaDisable, regenerateRecoveryCodes, deleteAccount, setPreferences,
        changeEmail, peekEmailChange, confirmEmailChange, exportAccount,
        logout, logoutAll, revokeSession,
    };
}
