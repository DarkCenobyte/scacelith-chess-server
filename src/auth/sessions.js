// Sessions: creation, validation with a per-process cache, revocation and its broadcast.
//
// A session token is "sct_" + 43 base64url characters (32 random bytes); only its SHA-256 (hex)
// is stored. validate() keeps the answer for at most 30 s in a bounded LRU cache (positive and
// negative), checks revocation, absolute expiry (SESSION_MAX_DAYS) and idle expiry
// (SESSION_IDLE_DAYS), and writes lastSeen at most every 5 minutes (sliding the idle expiry).
// A revocation drops the local cache entries at once and asks the primary to broadcast
// `session.revoked` so that every shard drops its entries (auth.invalidate on `auth.invalidate`).
//
// Broadcast convention (DESIGN 5.7 leaves it open): `tokenHashes` empty or absent means "every
// cached session of `userId`" (used when the revoked hashes are not known, e.g. when
// store.sessions.enforceLimit revoked the oldest sessions).

import { LruMap } from '../security/ratelimit.js';
import { randomToken, sha256Hex } from '../security/keys.js';

export const SESSION_TOKEN_RE = /^sct_[A-Za-z0-9_-]{43}$/;
export const SESSION_CACHE_TTL_MS = 30000;
export const SESSION_TOUCH_EVERY_MS = 5 * 60000;
const DAY_MS = 86400000;

/**
 * @param {{ config: object, store: object, log: object, now: () => number,
 *           control: (type: string, payload: object) => Promise<object>, primary: object|null,
 *           cacheSize?: number }} svc
 */
export function createSessionManager(svc) {
    const { config, store, log, now } = svc;
    const cache = new LruMap(svc.cacheSize || 10000);
    const idleMs = config.sessionIdleDays * DAY_MS;
    const maxMs = config.sessionMaxDays * DAY_MS;

    function isActiveRow(r, t) {
        return r && !r.revokedAt && r.expiresAt > t && r.idleExpiresAt > t;
    }

    /** Drops cached sessions (local only). */
    function invalidate({ userId, tokenHashes } = {}) {
        if (Array.isArray(tokenHashes) && tokenHashes.length) {
            for (const h of tokenHashes) cache.delete(h);
            return;
        }
        if (userId === undefined || userId === null) return;
        for (const [h, e] of cache.entries()) {
            if (e.s && e.s.userId === userId) cache.delete(h);
        }
    }

    /** Local invalidation + broadcast to every shard through the primary. */
    function broadcast(userId, tokenHashes) {
        invalidate({ userId, tokenHashes });
        if (!svc.primary) return;
        svc.primary.request('session.revoked', { userId, tokenHashes }).catch((err) => {
            log.warn('session revocation broadcast failed', { userId, err: { message: err.message } });
        });
    }

    /**
     * Opens a session for `user`.
     * @returns {{ token: string, tokenHash: string, expiresAt: number, sessionId: number|string }}
     */
    function create(user, { clientLabel = null, ip = null } = {}) {
        const t = now();
        let active = [];
        try { active = (store.sessions.listForUser(user.id) || []).filter((r) => isActiveRow(r, t)); } catch { active = []; }
        const token = randomToken('sct_');
        const tokenHash = sha256Hex(token);
        const expiresAt = t + maxMs;
        const idleExpiresAt = Math.min(expiresAt, t + idleMs);
        const sessionId = store.sessions.create({ userId: user.id, tokenHash, createdAt: t, expiresAt, idleExpiresAt, clientLabel: clientLabel || null, ip: ip || null });
        const revoked = store.sessions.enforceLimit(user.id, config.maxSessionsPerUser);
        if (active.length + 1 > config.maxSessionsPerUser) {
            broadcast(user.id, Array.isArray(revoked) ? revoked.filter((h) => typeof h === 'string' && h !== tokenHash) : []);
        }
        return { token, tokenHash, expiresAt, sessionId };
    }

    function load(hash, t) {
        let e = { loadedAt: t, s: null };
        const row = store.sessions.byTokenHash(hash);
        if (row && !row.revokedAt) {
            const user = store.users.byId(row.userId);
            if (user && user.status === 'active') {
                e = {
                    loadedAt: t,
                    s: { id: row.id, userId: row.userId, lastSeenAt: row.lastSeenAt ?? row.createdAt, expiresAt: row.expiresAt, idleExpiresAt: row.idleExpiresAt },
                    username: user.username,
                    emailVerified: !!user.emailVerified,
                };
            }
        }
        cache.set(hash, e);
        return e;
    }

    /**
     * Validates a session token.
     * @param {string} token
     * @returns {{ userId: number, username: string, sessionId: number|string, emailVerified: boolean, tokenHash: string } | null}
     */
    function validate(token) {
        if (typeof token !== 'string' || !SESSION_TOKEN_RE.test(token)) return null;
        const hash = sha256Hex(token);
        const t = now();
        let e = cache.get(hash);
        if (!e || t - e.loadedAt >= SESSION_CACHE_TTL_MS || t < e.loadedAt) e = load(hash, t);
        const s = e.s;
        if (!s || s.expiresAt <= t || s.idleExpiresAt <= t) return null;
        if (t - s.lastSeenAt >= SESSION_TOUCH_EVERY_MS) {
            const idle = Math.min(s.expiresAt, t + idleMs);
            try {
                store.sessions.touch(s.id, t, idle);
                s.lastSeenAt = t;
                s.idleExpiresAt = idle;
            } catch (err) {
                log.warn('session touch failed', { err: { message: err.message } });
            }
        }
        return { userId: s.userId, username: e.username, sessionId: s.id, emailVerified: e.emailVerified, tokenHash: hash };
    }

    /** Revokes one session of `userId`. */
    function revoke(userId, sessionId, tokenHash = null) {
        store.sessions.revoke(sessionId);
        broadcast(userId, tokenHash ? [tokenHash] : []);
    }

    /** Revokes every session of `userId` (but `exceptId`); returns the number revoked when known. */
    function revokeAll(userId, exceptId = undefined) {
        const hashes = store.sessions.revokeAllForUser(userId, exceptId);
        const list = Array.isArray(hashes) ? hashes.filter((h) => typeof h === 'string') : [];
        broadcast(userId, exceptId === undefined ? [] : list);
        return list.length;
    }

    /**
     * Drops the cached sessions of `userId` here and on every shard (the `session.revoked`
     * broadcast without token hashes), after a change of the account that the cache holds or that
     * must be read again: nothing is revoked, the next request reloads its session.
     */
    function refresh(userId) {
        broadcast(userId, []);
    }

    /** Active sessions of a user, for the sessions list. */
    function list(userId, currentId) {
        const t = now();
        return (store.sessions.listForUser(userId) || [])
            .filter((r) => isActiveRow(r, t))
            .map((r) => ({
                id: r.id, createdAt: r.createdAt, lastSeenAt: r.lastSeenAt ?? r.createdAt, expiresAt: r.expiresAt,
                clientLabel: r.clientLabel ?? null, current: String(r.id) === String(currentId),
            }))
            .sort((a, b) => b.lastSeenAt - a.lastSeenAt);
    }

    /** The active session row `id` of `userId`, or null. */
    function find(userId, id) {
        const t = now();
        return (store.sessions.listForUser(userId) || []).find((r) => String(r.id) === String(id) && isActiveRow(r, t)) || null;
    }

    return { create, validate, invalidate, refresh, revoke, revokeAll, list, find, cacheSize: () => cache.size };
}
