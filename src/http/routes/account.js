// /api/v1/account/* (docs/DESIGN.md 5.9). Every route needs a session (bearer token).
//
//   GET  /account/me -> { user, ratings: [...], sanctions: [...], ban: { until } | null }
//        (the integrity level is never exposed)
//   POST /account/password { currentPassword, newPassword } -> { status: 'password_changed' } (other sessions revoked)
//   POST /account/mfa/totp/setup { password } -> { secret, uri, algorithm, digits, period }
//   POST /account/mfa/totp/enable { code } -> { status: 'mfa_enabled', recoveryCodes: [10] }
//   POST /account/mfa/totp/disable { password, code? , recoveryCode? } -> { status: 'mfa_disabled' }
//   POST /account/mfa/recovery-codes { password, code } -> { recoveryCodes: [10] }
//   POST /account/delete { password, code?, recoveryCode? } -> { status: 'deleted' }
//   PUT  /account/preferences { acceptChallenges: 'all'|'none' } -> { preferences }
// Re-authentication errors: 403 invalid_password | mfa_code_required | invalid_code, 400 password_not_set,
// 429 too_many_attempts, 503 server_busy{retryAfter} (password hash queue full, nothing changed),
// 429 rate_limited{retryAfter} (2 password hashes of this client already wait, nothing changed).

const PASSWORD = { type: 'string', min: 1, max: 1024 };
const CODE = { type: 'string', min: 1, max: 32 };

/**
 * @param {import('../router.js').Router} router
 * @param {{ config: object, auth: object }} deps
 */
export function register(router, { config, auth }) {
    const reauthRate = { key: 'reauth', limit: config.authRatePerIp, prefixLimit: config.authRatePerPrefix, windowMs: 600000, shared: true };
    const readRate = { key: 'account', limit: 60, windowMs: 60000, by: 'user' };
    const opts = (body, rate = reauthRate) => ({ auth: 'required', rate, body });

    router.get('/account/me', (ctx) => ({ body: auth.me(ctx.user.userId) }), { auth: 'required', rate: readRate });

    router.post('/account/password', async (ctx) => ({ body: await auth.changePassword(ctx.user, ctx.session.id, { ...ctx.body, ip: ctx.ip }) }),
        opts({ currentPassword: PASSWORD, newPassword: PASSWORD }));

    router.post('/account/mfa/totp/setup', async (ctx) => ({ body: await auth.mfaSetup(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ password: PASSWORD }));
    router.post('/account/mfa/totp/enable', (ctx) => ({ body: auth.mfaEnable(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ code: { type: 'string', min: 6, max: 6, pattern: /^[0-9]{6}$/ } }));
    router.post('/account/mfa/totp/disable', async (ctx) => ({ body: await auth.mfaDisable(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ password: PASSWORD, code: { ...CODE, optional: true }, recoveryCode: { ...CODE, optional: true } }));
    router.post('/account/mfa/recovery-codes', async (ctx) => ({ body: await auth.regenerateRecoveryCodes(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ password: PASSWORD, code: CODE }));

    router.post('/account/delete', async (ctx) => ({ body: await auth.deleteAccount(ctx.user, { ...ctx.body, ip: ctx.ip }) }),
        opts({ password: PASSWORD, code: { ...CODE, optional: true }, recoveryCode: { ...CODE, optional: true } }));

    router.put('/account/preferences', (ctx) => ({ body: auth.setPreferences(ctx.user, ctx.body) }),
        opts({ acceptChallenges: { type: 'enum', values: ['all', 'none'] } }, readRate));
}
