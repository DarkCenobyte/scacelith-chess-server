// Google sign-in (docs/DESIGN.md 5.9; flow described in src/auth/sso.js). Disabled (404
// sso_disabled) unless SSO_GOOGLE_ENABLED.
//
//   POST /auth/sso/google/start { codeChallenge } -> { attemptId, authUrl, pollMs, expiresIn }
//   POST /auth/sso/google/poll { attemptId, codeVerifier, clientLabel? }
//        -> { status: 'pending' } | { token, expiresAt, user } | { mfaRequired, mfaToken, expiresIn }
//         | { needsUsername: true, ssoTicket, suggestedUsername }
//        errors: 403 invalid_verifier | banned | email_unverified | sso_email_unverified, 409 sso_account_unverified
//                | sso_cancelled, 410 sso_expired, 502 sso_failed
//   POST /auth/sso/complete { ssoTicket, username, clientLabel? } -> { token, expiresAt, user }
//   GET  /auth/sso/google/callback?code&state (HTML page, no token shown)
//
// Limits: start 30 per 10 minutes per address and 90 per IPv6 /48 (sso_start, shared; it writes
// two token rows); complete takes the `auth` limit of routes/auth.js (the same bucket, its /48
// included); poll 120 and the callback page 30 per minute per address (local).

import { ssoResult } from '../pages/sso-callback.js';
import { authRateOf } from './auth.js';

/**
 * @param {import('../router.js').Router} router
 * @param {{ config: object, auth: object }} deps
 */
export function register(router, { config, auth }) {
    const authRate = authRateOf(config);
    const startRate = { key: 'sso_start', limit: 30, prefixLimit: 90, windowMs: 600000, shared: true };
    const pollRate = { key: 'sso_poll', limit: 120, windowMs: 60000 };
    const pageRate = { key: 'sso_page', limit: 30, windowMs: 60000 };
    const label = { type: 'string', max: 64, optional: true };

    router.post('/auth/sso/google/start', (ctx) => ({ body: auth.sso.start({ codeChallenge: ctx.body.codeChallenge, ip: ctx.ip }) }), {
        auth: 'none', rate: startRate,
        body: { codeChallenge: { type: 'string', min: 43, max: 43, pattern: /^[A-Za-z0-9_-]{43}$/ } },
    });
    router.post('/auth/sso/google/poll', (ctx) => ({ body: auth.sso.poll({ ...ctx.body, ip: ctx.ip }) }), {
        auth: 'none', rate: pollRate,
        body: { attemptId: { type: 'string', min: 1, max: 64 }, codeVerifier: { type: 'string', min: 43, max: 128, pattern: /^[A-Za-z0-9._~-]{43,128}$/ }, clientLabel: label },
    });
    router.post('/auth/sso/complete', (ctx) => ({ body: auth.sso.complete({ ...ctx.body, ip: ctx.ip }) }), {
        auth: 'none', rate: authRate,
        body: { ssoTicket: { type: 'string', min: 1, max: 64 }, username: { type: 'string', min: 1, max: 64 }, clientLabel: label },
    });

    router.page('GET', '/auth/sso/google/callback', async (ctx) => {
        const q = ctx.query;
        const r = await auth.sso.callback({ code: q.code, state: q.state, error: q.error, ip: ctx.ip });
        return { status: r.ok ? 200 : 400, html: ssoResult({ serverName: config.serverName, ...r }) };
    }, { rate: pageRate });
}
