// Errors of the auth service. They carry the HTTP answer (status, snake_case code, message and
// extra JSON fields) with the same shape as http/router.js HttpError, so the HTTP layer answers
// them as they are: { "error": code, "message": ..., ...extra }.

export class AuthError extends Error {
    /**
     * @param {number} status HTTP status
     * @param {string} code snake_case error code
     * @param {string} message human-readable English message
     * @param {object} [extra] fields merged into the answer (retryAfter, until, pow, reason...)
     */
    constructor(status, code, message, extra) {
        super(message);
        this.status = status;
        this.code = code;
        this.extra = extra || null;
        this.headers = null;
        this.expose = true;
    }
}

/** 429 with the delay in seconds (Retry-After). */
export function tooManyAttempts(retryAfterMs) {
    return new AuthError(429, 'too_many_attempts', 'Too many attempts; wait before trying again.',
        { retryAfter: Math.max(1, Math.ceil(retryAfterMs / 1000)) });
}

/** Retry-After range of a 503 server_busy answer, in seconds (drawn at random in it). */
export const BUSY_RETRY_AFTER_SEC = Object.freeze({ min: 5, max: 15 });

/**
 * 503 when this process cannot hash a password now (the password hash queue is full or the
 * wait expired, security/password.js). The delay is random so that the clients refused during
 * one burst do not all come back together.
 * @param {number} [retryAfterSec]
 */
export function serverBusy(retryAfterSec = BUSY_RETRY_AFTER_SEC.min + Math.floor(Math.random() * (BUSY_RETRY_AFTER_SEC.max - BUSY_RETRY_AFTER_SEC.min + 1))) {
    return new AuthError(503, 'server_busy', 'The server is busy; try again in a few seconds.', { retryAfter: retryAfterSec });
}

/** The single answer of every failed password login (unknown account, wrong password, no password). */
export function invalidCredentials() {
    return new AuthError(401, 'invalid_credentials', 'Wrong user name, e-mail or password.');
}
