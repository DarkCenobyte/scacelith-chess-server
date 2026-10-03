// Google sign-in (docs/DESIGN.md 5.9; flow described in src/auth/sso.js). Disabled (404
// sso_disabled) unless SSO_GOOGLE_ENABLED. Google sends the browser back to the game's own
// listener on 127.0.0.1, never to this server.
//
//   POST /auth/sso/google/start { codeChallenge, redirectPort } -> { attemptId, authUrl, state, expiresIn }
//   POST /auth/sso/google/finish { attemptId, codeVerifier, state, code, iss?, clientLabel? }
//        -> { token, expiresAt, user } | { mfaRequired, mfaToken, expiresIn } (continued with POST /auth/login/mfa)
//         | { needsUsername: true, ssoTicket, suggestedUsername } (POST /auth/sso/complete)
//         | { needsPassword: true, linkTicket, username, expiresIn } (POST /auth/sso/google/link)
//        errors: 403 invalid_verifier | sso_email_unverified | registration_closed | account_disabled | banned
//                | email_unverified, 409 sso_account_exists, 410 sso_expired, 502 sso_failed
//   POST /auth/sso/google/link { linkTicket, password, clientLabel?, pow? }
//        -> { token, expiresAt, user } | { mfaRequired, mfaToken, expiresIn } (the link is stored once the code passes)
//        errors: 401 invalid_credentials (the ticket stays), 403 banned, 409 sso_already_linked, 410 sso_expired,
//                428 pow_required, 429 too_many_attempts | rate_limited, 503 server_busy
//   POST /auth/sso/complete { ssoTicket, username, clientLabel? } -> { token, expiresAt, user }
//
// Limits: start 30 per 10 minutes per address and 90 per IPv6 /48 (sso_start, shared; it writes
// a token row); finish 30 per minute per address (sso_finish, local); link and complete take the
// `auth` limit of routes/auth.js (the same bucket, its /48 included).

import { authRateOf, PASSWORD_FIELD, POW_FIELD } from './auth.js';

/**
 * @param {import('../router.js').Router} router
 * @param {{ config: object, auth: object }} deps
 */
export function register(router, { config, auth }) {
    const authRate = authRateOf(config);
    const startRate = { key: 'sso_start', limit: 30, prefixLimit: 90, windowMs: 600000, shared: true };
    const finishRate = { key: 'sso_finish', limit: 30, windowMs: 60000 };
    const label = { type: 'string', max: 64, optional: true };

    router.post('/auth/sso/google/start', (ctx) => ({ body: auth.sso.start({ ...ctx.body, ip: ctx.ip }) }), {
        auth: 'none', rate: startRate,
        body: {
            codeChallenge: { type: 'string', min: 43, max: 43, pattern: /^[A-Za-z0-9_-]{43}$/ },
            redirectPort: { type: 'integer', min: 1024, max: 65535 },
        },
    });
    router.post('/auth/sso/google/finish', async (ctx) => ({ body: await auth.sso.finish({ ...ctx.body, ip: ctx.ip }) }), {
        auth: 'none', rate: finishRate,
        body: {
            attemptId: { type: 'string', min: 1, max: 64 },
            codeVerifier: { type: 'string', min: 43, max: 128, pattern: /^[A-Za-z0-9._~-]{43,128}$/ },
            state: { type: 'string', min: 43, max: 43, pattern: /^[A-Za-z0-9_-]{43}$/ },
            code: { type: 'string', min: 1, max: 2048, pattern: /^[\x21-\x7e]+$/ },
            iss: { type: 'string', min: 1, max: 256, optional: true },
            clientLabel: label,
        },
    });
    router.post('/auth/sso/google/link', async (ctx) => ({ body: await auth.sso.link({ ...ctx.body, ip: ctx.ip }) }), {
        auth: 'none', rate: authRate,
        body: { linkTicket: { type: 'string', min: 1, max: 64 }, password: PASSWORD_FIELD, clientLabel: label, pow: POW_FIELD },
    });
    router.post('/auth/sso/complete', (ctx) => ({ body: auth.sso.complete({ ...ctx.body, ip: ctx.ip }) }), {
        auth: 'none', rate: authRate,
        body: { ssoTicket: { type: 'string', min: 1, max: 64 }, username: { type: 'string', min: 1, max: 64 }, clientLabel: label },
    });
}
