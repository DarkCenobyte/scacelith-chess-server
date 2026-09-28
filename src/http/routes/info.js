// GET /api/v1/info: what the game needs to know before connecting (docs/DESIGN.md 5.9).

import { PROTOCOL_MIN, PROTOCOL_VERSION, SCHEMA_HASH, WS_SUBPROTOCOL } from '../../protocol/index.js';
import { USERNAME_PATTERN } from '../../auth/identity.js';
import { PASSWORD_MAX_BYTES } from '../../security/password.js';

/**
 * The /info document.
 * @param {object} config
 * @param {object} store
 * @returns {object}
 */
export function serverInfo(config, store) {
    let serverId = null;
    try { serverId = store.meta.get('server_id') ?? null; } catch { serverId = null; }
    return {
        name: config.serverName,
        serverId,
        motd: config.serverMotd,
        protocol: { min: PROTOCOL_MIN, max: PROTOCOL_VERSION, schema: SCHEMA_HASH, subprotocol: WS_SUBPROTOCOL },
        wsPort: config.publicWsPort,
        wsPath: '/ws',
        registration: config.registration,
        emailVerification: config.requireEmailVerification,
        sso: { google: !!(config.ssoGoogleEnabled && config.googleClientId) },
        mfa: true,
        pow: { register: config.powRegisterBits },
        categories: config.categories.map((c) => ({ id: c.id, baseSec: c.baseMs / 1000, incSec: c.incMs / 1000 })),
        limits: {
            usernameMin: config.usernameMin,
            usernameMax: config.usernameMax,
            usernamePattern: USERNAME_PATTERN.source,
            passwordMinLength: config.passwordMinLength,
            passwordMaxBytes: PASSWORD_MAX_BYTES,
            customTimeControls: config.allowCustomTimeControls,
            reportsPerDay: config.reportsPerDay,
            wsMaxMessageBytes: config.wsMaxMessageBytes,
        },
    };
}

/**
 * @param {import('../router.js').Router} router
 * @param {{ config: object, store: object }} deps
 */
export function register(router, { config, store }) {
    router.get('/info', () => ({ body: serverInfo(config, store) }), { auth: 'none' });
}
