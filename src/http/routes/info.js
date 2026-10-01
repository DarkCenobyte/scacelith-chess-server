// GET /api/v1/info: what the game needs to know before connecting (docs/DESIGN.md 5.9). No limit
// of its own (only the background per-address budget): the store's server id is read once, so
// that an answer costs no SQLite read.

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
    return infoDocument(config, readServerId(store));
}

// The server id never changes once the store has it: read once per store, not on every request.
const serverIds = new WeakMap();
function readServerId(store) {
    if (store && typeof store === 'object' && serverIds.has(store)) return serverIds.get(store);
    let serverId = null;
    try { serverId = store.meta.get('server_id') ?? null; } catch { serverId = null; }
    if (serverId !== null && store && typeof store === 'object') serverIds.set(store, serverId);
    return serverId;
}

function infoDocument(config, serverId) {
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
