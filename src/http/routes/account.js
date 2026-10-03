// /api/v1/account/* (docs/DESIGN.md 5.9, docs/API.md). Every route needs a session (bearer token),
// except the HTML pages of the e-mail change link at the end.
//
//   GET  /account/me -> { user, ratings: [...], sanctions: [...], ban: { until } | null }
//        user: { id, username, email, emailVerified, mfaEnabled, googleLinked, hasPassword,
//                acceptChallenges: 'all'|'none', createdAt, lastLoginAt, pendingEmail }
//        (pendingEmail: the new address of a pending e-mail change, or null; the integrity level
//        is never exposed)
//   POST /account/password { currentPassword, newPassword } -> { status: 'password_changed' } (other sessions revoked)
//   POST /account/mfa/totp/setup { password } -> { secret, uri, algorithm, digits, period }
//   POST /account/mfa/totp/enable { code } -> { status: 'mfa_enabled', recoveryCodes: [10] }
//   POST /account/mfa/totp/disable { password, code? , recoveryCode? } -> { status: 'mfa_disabled' }
//   POST /account/mfa/recovery-codes { password, code } -> { recoveryCodes: [10] }
//   POST /account/delete { password, code?, recoveryCode? } -> { status: 'deleted' }
//   POST /account/email { newEmail, password, code?, recoveryCode? }
//        -> 202 { status: 'verification_sent' } (REQUIRE_EMAIL_VERIFICATION: the same answer
//           whether or not another account uses the address; the link goes to the new address)
//         | 200 { status: 'email_changed', email } (without e-mail verification: changed at once)
//        errors: 400 invalid_email | same_email, 409 email_taken (only without e-mail verification),
//                503 server_busy{retryAfter: 1} (the store stayed locked: nothing changed), and
//                the re-authentication errors below (auth/accounts.js, e-mail change)
//   POST /account/export { password, code?, recoveryCode? } -> the account's data (routes/account-export.js)
//   PUT  /account/preferences { acceptChallenges: 'all'|'none' } -> { preferences }
// Re-authentication errors: 403 invalid_password | mfa_code_required | invalid_code, 400 password_not_set,
// 429 too_many_attempts, 503 server_busy{retryAfter} (password hash queue full, nothing changed),
// 429 rate_limited{retryAfter} (the hash queue is at least half full and PASSWORD_HASH_WAITERS_PER_SOURCE
// password hashes of this client already wait; nothing changed, and the reauth limit's token is given back).
//
// Limits of the routes that ask for the password or a code: `reauth` (AUTH_RATE_PER_IP per 10
// minutes per address, AUTH_RATE_PER_PREFIX per IPv6 /48, shared) and `reauth_user`
// (AUTH_REAUTH_PER_USER per 10 minutes per account, shared: a stolen session used from many
// addresses cannot guess the password faster); the second factors also count against
// AUTH_MFA_PER_ACCOUNT (auth/mfa.js). The reads take `account` (60 per minute per account).
//
// The e-mail change link (pages outside /api, like /verify-email):
//   GET  /confirm-email-change?token= -> the new address and a confirmation button (the token is
//        not consumed: link scanners open links), or 400 "link invalid or expired"
//   POST /confirm-email-change (form: token) -> "E-mail address changed" | 400 invalid or expired
//        | 409 "Address already used" (another account took the address since the request)
//        | 503 server_busy, Retry-After 1 (the store stayed locked: nothing changed, the link
//          still works)

import * as emailPages from '../pages/email-change.js';
import { AUTH_ABUSE_WEIGHT, CODE_FIELD, EMAIL_FIELD, LINK_TOKEN_FIELD, PAGE_RATE, PASSWORD_FIELD, authRateOf } from './auth.js';

/**
 * The limits of a route that asks for the password: `reauth` per address and IPv6 /48, then
 * `reauth_user` per account (routes/account-export.js takes them too).
 * @param {object} config
 */
export function reauthRatesOf(config) {
    return [
        { key: 'reauth', limit: config.authRatePerIp, prefixLimit: config.authRatePerPrefix, windowMs: 600000, shared: true, abuseWeight: AUTH_ABUSE_WEIGHT },
        { key: 'reauth_user', limit: config.authReauthPerUser, windowMs: 600000, by: 'user', shared: true },
    ];
}

/**
 * @param {import('../router.js').Router} router
 * @param {{ config: object, auth: object }} deps
 */
export function register(router, { config, auth }) {
    const reauthRates = reauthRatesOf(config);
    const readRate = { key: 'account', limit: 60, windowMs: 60000, by: 'user' };
    const opts = (body, rate = reauthRates) => ({ auth: 'required', rate, body });

    router.get('/account/me', (ctx) => ({ body: auth.me(ctx.user.userId) }), { auth: 'required', rate: readRate });

    router.post('/account/password', async (ctx) => ({ body: await auth.changePassword(ctx.user, ctx.session.id, { ...ctx.body, ip: ctx.ip }) }),
        opts({ currentPassword: PASSWORD_FIELD, newPassword: PASSWORD_FIELD }));

    router.post('/account/mfa/totp/setup', async (ctx) => ({ body: await auth.mfaSetup(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ password: PASSWORD_FIELD }));
    router.post('/account/mfa/totp/enable', (ctx) => ({ body: auth.mfaEnable(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ code: { type: 'string', min: 6, max: 6, pattern: /^[0-9]{6}$/ } }));
    router.post('/account/mfa/totp/disable', async (ctx) => ({ body: await auth.mfaDisable(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ password: PASSWORD_FIELD, code: { ...CODE_FIELD, optional: true }, recoveryCode: { ...CODE_FIELD, optional: true } }));
    router.post('/account/mfa/recovery-codes', async (ctx) => ({ body: await auth.regenerateRecoveryCodes(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ password: PASSWORD_FIELD, code: CODE_FIELD }));

    router.post('/account/delete', async (ctx) => ({ body: await auth.deleteAccount(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ password: PASSWORD_FIELD, code: { ...CODE_FIELD, optional: true }, recoveryCode: { ...CODE_FIELD, optional: true } }));

    router.post('/account/email', async (ctx) => auth.changeEmail(ctx.user, { ...ctx.body, ip: ctx.ip }),
        opts({ newEmail: EMAIL_FIELD, password: PASSWORD_FIELD, code: { ...CODE_FIELD, optional: true }, recoveryCode: { ...CODE_FIELD, optional: true } }));

    router.put('/account/preferences', (ctx) => ({ body: auth.setPreferences(ctx.user, ctx.body) }),
        opts({ acceptChallenges: { type: 'enum', values: ['all', 'none'] } }, readRate));

    // ---- the e-mail change link (GET shows a button, POST acts) ----
    const serverName = config.serverName;
    const linkRate = authRateOf(config);
    router.page('GET', '/confirm-email-change', (ctx) => {
        const token = ctx.query.token;
        const pending = auth.peekEmailChange(token);
        if (!pending) return { status: 400, html: emailPages.emailChangeInvalid({ serverName }) };
        return { html: emailPages.emailChangeForm({ serverName, token, email: pending.email, username: pending.username }) };
    }, { rate: PAGE_RATE });
    router.page('POST', '/confirm-email-change', (ctx) => {
        const r = auth.confirmEmailChange(ctx.body.token, ctx.ip);
        if (r.status === 'changed') return { html: emailPages.emailChangeDone({ serverName, email: r.email }) };
        if (r.status === 'taken') return { status: 409, html: emailPages.emailChangeTaken({ serverName }) };
        return { status: 400, html: emailPages.emailChangeInvalid({ serverName }) };
    }, { rate: linkRate, body: { token: LINK_TOKEN_FIELD } });
}
