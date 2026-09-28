// Account flows: registration, e-mail confirmation, password reset and change, account view,
// re-authenticated account changes (MFA, deletion), preferences, sessions, logout.
//
// Enumeration resistance: register / resend / forgot give the same answer whatever the e-mail;
// the "e-mail exists" branch of register hashes the password too (same work) and mails the
// owner of the address instead. E-mails are throttled per address (primary `once.consume`, the
// same call made whether the address exists or not).
//
// Deviations (documented in the report):
//  * REQUIRE_EMAIL_VERIFICATION=false: an account is ready at once (201 { status: 'ready' }) and an
//    existing e-mail is answered 409 email_taken (without confirmation e-mails the "same answer"
//    would only hide the failure from honest users; the owner still gets the notice e-mail).
//  * POST /account/password requires the current password only (the client contract
//    changePassword(current, next) sends no code); PUT /account/preferences requires only the
//    session. Deletion, MFA disable and recovery-code regeneration require password + second factor.
//  * A password reset also marks the e-mail address as confirmed (the link proved it).

import crypto from 'node:crypto';
import { AuthError, tooManyAttempts } from './errors.js';
import { LINK_TOKEN_RE, TOKEN_TTL_MS, dataOf, isLive } from './tokens.js';
import { checkUsername, isValidEmail, normalizeEmail } from './identity.js';
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
        svc.mail('verification', user.email, { username: user.username, link: svc.links.verify(token), hours: 24 });
    }

    async function notifyExistingAddress(user, ip) {
        events.record('register_existing_email', { userId: user.id, ip });
        if (await once(mailKey('regattempt', user.email), NOTICE_THROTTLE_MS)) {
            svc.mail('registrationAttempt', user.email, { username: user.username });
        }
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
        if (store.users.byUsername(username)) throw new AuthError(409, 'username_taken', 'This username is already taken.');
        if (config.powRegisterBits > 0) await svc.requirePow('register', config.powRegisterBits, ip, pow);

        const sameAnswer = { status: 202, body: { status: 'verification_sent' } };
        const existing = store.users.byEmail(em);
        const passwordHash = await hasher.hash(password);     // also for an existing address: same work
        if (existing) {
            await notifyExistingAddress(existing, ip);
            if (!config.requireEmailVerification) throw new AuthError(409, 'email_taken', 'An account already uses this e-mail address.');
            return sameAnswer;
        }
        let id;
        try {
            id = store.users.create({ username, email: em, passwordHash, emailVerified: !config.requireEmailVerification });
        } catch (err) {
            if (err && err.code === 'username_taken') throw new AuthError(409, 'username_taken', 'This username is already taken.');
            if (err && err.code === 'email_taken') {
                const owner = store.users.byEmail(em);
                if (owner) await notifyExistingAddress(owner, ip);
                if (!config.requireEmailVerification) throw new AuthError(409, 'email_taken', 'An account already uses this e-mail address.');
                return sameAnswer;
            }
            throw err;
        }
        events.record('register', { userId: id, ip });
        if (!config.requireEmailVerification) return { status: 201, body: { status: 'ready' } };
        sendVerification({ id, username, email: em });
        return sameAnswer;
    }

    /** A live token row of `kind` for a link token, or null (does not consume it). */
    function peekToken(kind, token) {
        if (typeof token !== 'string' || !LINK_TOKEN_RE.test(token)) return null;
        const row = store.tokens.get(kind, sha256Hex(token));
        return isLive(row, now()) ? row : null;
    }

    /** POST /verify-email: consumes the token and confirms the address. */
    function verifyEmail(token, ip = null) {
        if (typeof token !== 'string' || !LINK_TOKEN_RE.test(token)) return false;
        const row = store.tokens.consume('email_verify', sha256Hex(token), now());
        if (!row || (row.expiresAt != null && row.expiresAt <= now())) return false;
        const user = store.users.byId(row.userId);
        if (!user || user.status !== 'active' || normalizeEmail(user.email) !== normalizeEmail(dataOf(row).email)) return false;
        if (!user.emailVerified) store.users.update(user.id, { emailVerified: true });
        sessions.invalidate({ userId: user.id });
        events.record('email_verified', { userId: user.id, ip });
        return true;
    }

    /** POST /auth/verify-email/resend: 202 whatever happens. */
    async function resendVerification({ email, ip }) {
        const em = normalizeEmail(email);
        const fresh = await once(mailKey('verify', em), MAIL_THROTTLE_MS);
        const user = isValidEmail(em) ? store.users.byEmail(em) : null;
        if (fresh && user && user.status === 'active' && !user.emailVerified) {
            sendVerification(user);
            events.record('verification_resent', { userId: user.id, ip });
        }
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
            svc.mail('passwordReset', user.email, { username: user.username, link: svc.links.reset(token), minutes: 60 });
            events.record('password_reset_requested', { userId: user.id, ip });
        }
        return { status: 'accepted' };
    }

    function invalidResetToken() {
        return new AuthError(400, 'invalid_token', 'This reset link is invalid, was already used, or has expired.');
    }

    /**
     * POST /auth/password/reset (and the HTML form). Revokes every session; never touches MFA.
     */
    async function resetPassword({ token, newPassword, ip }) {
        const row = peekToken('password_reset', token);
        if (!row) throw invalidResetToken();
        const user = store.users.byId(row.userId);
        if (!user || user.status !== 'active') throw invalidResetToken();
        const policy = checkPasswordPolicy(newPassword, { minLength: config.passwordMinLength, username: user.username, email: user.email });
        if (policy) throw weak(policy);
        const passwordHash = await hasher.hash(newPassword);
        if (!store.tokens.consume('password_reset', sha256Hex(token), now())) throw invalidResetToken();
        store.users.update(user.id, { passwordHash, emailVerified: true });
        sessions.revokeAll(user.id);
        events.record('password_reset', { userId: user.id, ip });
        svc.mail('passwordChanged', user.email, { username: user.username, when: new Date(now()), byReset: true });
        return { status: 'password_reset' };
    }

    /**
     * Re-authentication for account changes: the password and, when MFA is on and
     * `secondFactor` is not 'none', a TOTP code ('totp') or a code or recovery code ('any').
     * @returns {Promise<object>} the user row
     */
    async function reauth(userId, { password, code, recoveryCode }, { secondFactor = 'any', ip = null } = {}) {
        const key = 'r' + userId;
        const wait = reauthFailures.retryAfter(key);
        if (wait > 0) throw tooManyAttempts(wait);
        const user = activeUser(userId);
        if (!user.passwordHash) {
            await hasher.verifyDummy(password || '');
            throw new AuthError(400, 'password_not_set', 'This account has no password yet; set one with "Forgot password" first.');
        }
        const { ok } = await hasher.verify(user.passwordHash, password);
        if (!ok) {
            reauthFailures.fail(key);
            events.record('reauth_failed', { userId, ip, detail: { factor: 'password' } });
            throw new AuthError(403, 'invalid_password', 'Wrong password.');
        }
        if (secondFactor !== 'none' && user.mfaEnabled) {
            if (!code && !recoveryCode) throw new AuthError(403, 'mfa_code_required', 'Enter a code of your authenticator app.');
            const ok2 = await mfa.checkSecondFactor(user, { code, recoveryCode, allowRecovery: secondFactor === 'any', ip });
            if (!ok2) {
                reauthFailures.fail(key);
                events.record('reauth_failed', { userId, ip, detail: { factor: 'mfa' } });
                throw new AuthError(403, 'invalid_code', 'Wrong or already used code.');
            }
        }
        reauthFailures.reset(key);
        return store.users.byId(userId) || user;
    }

    /** POST /account/password: revokes the other sessions. */
    async function changePassword(ctxUser, sessionId, { currentPassword, newPassword, ip }) {
        const user = await reauth(ctxUser.userId, { password: currentPassword }, { secondFactor: 'none', ip });
        const policy = checkPasswordPolicy(newPassword, { minLength: config.passwordMinLength, username: user.username, email: user.email });
        if (policy) throw weak(policy);
        store.users.update(user.id, { passwordHash: await hasher.hash(newPassword) });
        sessions.revokeAll(user.id, sessionId);
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
            peak: r.peak, provisional: (r.games ?? 0) < config.provisionalGames,
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

    /** POST /account/delete: anonymises the account and revokes every session. */
    async function deleteAccount(ctxUser, { password, code, recoveryCode, ip }) {
        const user = await reauth(ctxUser.userId, { password, code, recoveryCode }, { secondFactor: 'any', ip });
        store.users.anonymize(user.id);
        sessions.revokeAll(user.id);
        events.record('account_deleted', { userId: user.id, ip });
        return { status: 'deleted' };
    }

    /** PUT /account/preferences. */
    function setPreferences(ctxUser, { acceptChallenges }) {
        const user = activeUser(ctxUser.userId);
        store.users.update(user.id, { acceptChallenges });
        return { preferences: { acceptChallenges } };
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
        sessions.revoke(ctxUser.userId, row.id, row.tokenHash || null);
        events.record('session_revoked', { userId: ctxUser.userId, ip, detail: { reason: 'user' } });
        return { status: 'revoked' };
    }

    return {
        register, peekToken, verifyEmail, resendVerification, forgotPassword, resetPassword, reauth, changePassword,
        me, mfaSetup, mfaEnable, mfaDisable, regenerateRecoveryCodes, deleteAccount, setPreferences,
        logout, logoutAll, revokeSession,
    };
}
