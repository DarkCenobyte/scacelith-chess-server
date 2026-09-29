// Single-use tokens kept in store.tokens (only their SHA-256 is stored):
//
//   kind           lifetime   token given to               data
//   email_verify   24 h       e-mail link                  { email }
//   password_reset 1 h        e-mail link                  { email }
//   mfa_login      5 min      login answer (mfa_...)       { attempts, clientLabel, method, pwh? }
//   sso_attempt    10 min     SSO start answer (sso_...)   { challenge, status, result }
//   sso_state      10 min     Google (state parameter)     { attempt, nonce, verifier }
//   sso_ticket     10 min     SSO poll answer (sso_...)    { sub, email }

export const TOKEN_TTL_MS = Object.freeze({
    email_verify: 24 * 3600000,
    password_reset: 3600000,
    mfa_login: 5 * 60000,
    sso_attempt: 10 * 60000,
    sso_state: 10 * 60000,
    sso_ticket: 10 * 60000,
});

export const LINK_TOKEN_RE = /^[A-Za-z0-9_-]{43}$/;
export const MFA_TOKEN_RE = /^mfa_[A-Za-z0-9_-]{43}$/;
export const SSO_TOKEN_RE = /^sso_[A-Za-z0-9_-]{43}$/;

/**
 * The data object of a token row (the Store may return it parsed or as JSON text).
 * @param {object|null} row
 * @returns {object}
 */
export function dataOf(row) {
    if (!row || row.data == null) return {};
    if (typeof row.data === 'string') {
        try { return JSON.parse(row.data) || {}; } catch { return {}; }
    }
    return row.data;
}

/**
 * True when a token row exists, is not used and not expired.
 * @param {object|null} row
 * @param {number} now
 * @returns {boolean}
 */
export function isLive(row, now) {
    if (!row) return false;
    if (row.usedAt || row.consumedAt) return false;
    return row.expiresAt == null || row.expiresAt > now;
}
