// Single-use tokens kept in store.tokens (only their SHA-256 is stored):
//
//   kind           lifetime   token given to               data
//   email_verify   24 h       e-mail link                  { email }
//   email_change   24 h       e-mail link (new address)    { email: new address, from: address at the request }
//   password_reset 1 h        e-mail link                  { email }
//   mfa_login      5 min      login answer (mfa_...)       { attempts, clientLabel, method, pwh?, link? }
//   sso_attempt    10 min     SSO start answer (sso_...)   { challenge, stateHash, nonce, verifier, redirectUri }
//   sso_ticket     10 min     SSO finish answer (sso_...)  { sub, email }
//   sso_link       10 min     SSO finish answer (sso_...)  { userId, sub, email, tries }
//
// sso_attempt is consumed by finish, sso_ticket by complete. An sso_link row (user_id = the
// account) waits for the account's password: `tries` is reserved before each check (at most 5,
// store tokens.reserveTry), and the row is consumed by the right password, the 5th failure or a
// failed re-check. mfa_login's `link` ({ sub, email, pwh }) is the Google link a correct code
// stores. The Google `state` itself is never stored, only its SHA-256 in the attempt (auth/sso.js).

export const TOKEN_TTL_MS = Object.freeze({
    email_verify: 24 * 3600000,
    email_change: 24 * 3600000,
    password_reset: 3600000,
    mfa_login: 5 * 60000,
    sso_attempt: 10 * 60000,
    sso_ticket: 10 * 60000,
    sso_link: 10 * 60000,
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
