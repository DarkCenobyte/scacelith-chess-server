// /api/v1/auth/* (except Google sign-in, routes/sso.js) and the e-mail link pages
// /verify-email and /reset-password (docs/DESIGN.md 5.9).
//
// Answers:
//   POST /auth/register        { username, email, password, pow? }
//        -> 202 { status: 'verification_sent' } | 201 { status: 'ready' } (no e-mail confirmation)
//        errors: 400 invalid_username|invalid_email|weak_password{reason}, 403 registration_closed,
//                409 username_taken (email_taken only without e-mail confirmation), 428 pow_required{pow}
//   POST /auth/login           { login, password, clientLabel?, pow? }
//        -> 200 { token, expiresAt, user } | 200 { mfaRequired: true, mfaToken, expiresIn }
//        errors: 401 invalid_credentials, 403 email_unverified | banned{until}, 428 pow_required, 429 too_many_attempts{retryAfter}
//   POST /auth/login/mfa       { mfaToken, code? , recoveryCode? } -> 200 { token, expiresAt, user }
//        errors: 401 invalid_mfa_token | invalid_code, 403 banned, 429
//   POST /auth/logout, /auth/logout-all (bearer) -> 200 { status: 'logged_out' }
//   GET  /auth/sessions (bearer) -> { sessions: [{ id, createdAt, lastSeenAt, expiresAt, clientLabel, current }] }
//   DELETE /auth/sessions/:id (bearer) -> { status: 'revoked' } | 404
//   POST /auth/verify-email/resend { email } -> 202 { status: 'accepted' }
//   POST /auth/password/forgot { email } -> 202 { status: 'accepted' }
//   POST /auth/password/reset { token, newPassword } -> 200 { status: 'password_reset' } | 400 invalid_token | weak_password
// Every endpoint that hashes or checks a password (register, login, password reset) may also
// answer 503 server_busy{retryAfter} (with a Retry-After header) when the password hash queue of
// the worker is full or the wait expired (security/password.js); nothing was changed then.

import * as verifyPages from '../pages/verify-email.js';
import * as resetPages from '../pages/reset-password.js';

export const POW_FIELD = Object.freeze({
    type: 'object', optional: true,
    fields: { challenge: { type: 'string', min: 16, max: 512 }, nonce: { type: 'string', min: 1, max: 20, pattern: /^[0-9]+$/ } },
});
const PASSWORD = { type: 'string', min: 1, max: 1024 };
const EMAIL = { type: 'string', min: 1, max: 254 };
const LINK_TOKEN = { type: 'string', min: 1, max: 128 };

/**
 * @param {import('../router.js').Router} router
 * @param {{ config: object, auth: object }} deps
 */
export function register(router, { config, auth }) {
    const authRate = { key: 'auth', limit: config.authRatePerIp, windowMs: 600000, shared: true };
    const pageRate = { key: 'page', limit: 60, windowMs: 60000 };
    const sessionRate = { key: 'sessions', limit: 60, windowMs: 60000, by: 'user' };

    router.post('/auth/register', async (ctx) => auth.register({ ...ctx.body, ip: ctx.ip }), {
        auth: 'none', rate: authRate,
        body: { username: { type: 'string', min: 1, max: 64 }, email: EMAIL, password: PASSWORD, pow: POW_FIELD },
    });

    router.post('/auth/login', async (ctx) => ({ body: await auth.login({ ...ctx.body, ip: ctx.ip }) }), {
        auth: 'none', rate: authRate,
        body: { login: { type: 'string', min: 1, max: 254 }, password: PASSWORD, clientLabel: { type: 'string', max: 64, optional: true }, pow: POW_FIELD },
    });

    router.post('/auth/login/mfa', async (ctx) => ({ body: await auth.loginMfa({ ...ctx.body, ip: ctx.ip }) }), {
        auth: 'none', rate: authRate,
        body: { mfaToken: { type: 'string', min: 1, max: 64 }, code: { type: 'string', max: 32, optional: true }, recoveryCode: { type: 'string', max: 32, optional: true } },
    });

    router.post('/auth/logout', (ctx) => ({ body: auth.logout(ctx.user, ctx.session, ctx.ip) }), { auth: 'required', rate: sessionRate });
    router.post('/auth/logout-all', (ctx) => ({ body: auth.logoutAll(ctx.user, ctx.ip) }), { auth: 'required', rate: sessionRate });
    router.get('/auth/sessions', (ctx) => ({ body: auth.listSessions(ctx.user, ctx.session.id) }), { auth: 'required', rate: sessionRate });
    router.delete('/auth/sessions/:id', (ctx) => ({ body: auth.revokeSession(ctx.user, ctx.params.id, ctx.ip) }), { auth: 'required', rate: sessionRate });

    router.post('/auth/verify-email/resend', async (ctx) => ({ status: 202, body: await auth.resendVerification({ email: ctx.body.email, ip: ctx.ip }) }), {
        auth: 'none', rate: authRate, body: { email: EMAIL },
    });
    router.post('/auth/password/forgot', async (ctx) => ({ status: 202, body: await auth.forgotPassword({ email: ctx.body.email, ip: ctx.ip }) }), {
        auth: 'none', rate: authRate, body: { email: EMAIL },
    });
    router.post('/auth/password/reset', async (ctx) => ({ body: await auth.resetPassword({ ...ctx.body, ip: ctx.ip }) }), {
        auth: 'none', rate: authRate, body: { token: LINK_TOKEN, newPassword: PASSWORD },
    });

    // ---- HTML pages of the e-mail links (GET shows a button / form, POST acts) ----
    const serverName = config.serverName;
    router.page('GET', '/verify-email', (ctx) => {
        const token = ctx.query.token;
        if (!auth.peekToken('email_verify', token)) return { status: 400, html: verifyPages.verifyInvalid({ serverName }) };
        return { html: verifyPages.verifyForm({ serverName, token }) };
    }, { rate: pageRate });
    router.page('POST', '/verify-email', (ctx) => {
        if (!auth.verifyEmail(ctx.body.token, ctx.ip)) return { status: 400, html: verifyPages.verifyInvalid({ serverName }) };
        return { html: verifyPages.verifyDone({ serverName }) };
    }, { rate: authRate, body: { token: LINK_TOKEN } });

    router.page('GET', '/reset-password', (ctx) => {
        const token = ctx.query.token;
        if (!auth.peekToken('password_reset', token)) return { status: 400, html: resetPages.resetInvalid({ serverName }) };
        return { html: resetPages.resetForm({ serverName, token, minLength: config.passwordMinLength }) };
    }, { rate: pageRate });
    router.page('POST', '/reset-password', async (ctx) => {
        const { token, newPassword, confirmPassword } = ctx.body;
        const form = (error) => ({ status: 400, html: resetPages.resetForm({ serverName, token, minLength: config.passwordMinLength, error }) });
        if (!auth.peekToken('password_reset', token)) return { status: 400, html: resetPages.resetInvalid({ serverName }) };
        if (newPassword !== confirmPassword) return form('The two passwords are different.');
        try {
            await auth.resetPassword({ token, newPassword, ip: ctx.ip });
        } catch (err) {
            if (err && err.code === 'weak_password') return form(err.message);
            if (err && err.code === 'invalid_token') return { status: 400, html: resetPages.resetInvalid({ serverName }) };
            // The link is still valid: show the form again so that the user can simply resend it.
            if (err && err.code === 'server_busy') {
                return { ...form(err.message), status: 503, headers: { 'Retry-After': String(err.extra.retryAfter) } };
            }
            throw err;
        }
        return { html: resetPages.resetDone({ serverName }) };
    }, { rate: authRate, body: { token: LINK_TOKEN, newPassword: PASSWORD, confirmPassword: PASSWORD } });
}
