// Google sign-in for a desktop game (docs/DESIGN.md 5.9):
//
//  1. start   The game makes a PKCE pair and sends only its S256 challenge. The server creates an
//             attempt (10 min) and its own state, nonce and PKCE pair for Google, and answers the
//             attempt id and the Google URL the game opens in the system browser.
//  2. callback Google sends the browser to /auth/sso/google/callback?code&state. The server
//             exchanges the code (client secret + its verifier), verifies the ID token, resolves
//             the account and stores the result in the attempt. The page only says "You can go
//             back to Scacelith"; it never shows a token.
//  3. poll    The game polls with the attempt id AND its PKCE verifier (S256 must match the start
//             challenge): an attempt id alone is useless to whoever sees it (browser history,
//             logs). The result is delivered once: a login answer (session or MFA step), or
//             { needsUsername, ssoTicket } for a new account.
//  4. complete (new accounts) { ssoTicket, username } creates the account, links it and logs in.
//
// Account resolution: an existing link -> that user; else a local account with the same e-mail
// is linked when both our address and Google's are confirmed; an unconfirmed local account with
// that e-mail is refused (clear page); otherwise a new account (needs a username).

import { AuthError } from './errors.js';
import { SSO_TOKEN_RE, TOKEN_TTL_MS, dataOf, isLive } from './tokens.js';
import { checkUsername, normalizeEmail, suggestUsername } from './identity.js';
import { pkceChallenge } from './oidc.js';
import { randomToken, safeEqual, sha256Hex } from '../security/keys.js';

export const SSO_POLL_MS = 2000;
const PROVIDER = 'google';

const MESSAGES = {
    sso_cancelled: 'The sign-in was cancelled.',
    sso_failed: 'Google sign-in could not be completed.',
    sso_email_unverified: 'Google has not confirmed the e-mail address of this Google account.',
    sso_account_unverified: 'An account with this e-mail address already exists but its address is not confirmed. Log in with your password, confirm the address with the link we e-mailed you, then use Google sign-in.',
    registration_closed: 'Registration is closed on this server.',
    account_disabled: 'This account can no longer be used.',
};
const STATUS = { sso_cancelled: 409, sso_failed: 502, sso_email_unverified: 403, sso_account_unverified: 409, registration_closed: 403, account_disabled: 403 };

/**
 * @param {object} svc the auth service internals (see auth/index.js); svc.oidc is the provider client
 */
export function createSso(svc) {
    const { config, store, now, events, log, login } = svc;

    const enabled = () => !!(config.ssoGoogleEnabled && svc.oidc);
    function requireEnabled() {
        if (!enabled()) throw new AuthError(404, 'sso_disabled', 'Google sign-in is not enabled on this server.');
    }
    const expired = () => new AuthError(410, 'sso_expired', 'This sign-in attempt has expired; start again.');

    /** POST /auth/sso/google/start. */
    function start({ codeChallenge, ip }) {
        requireEnabled();
        const attemptId = randomToken('sso_');
        const attemptHash = sha256Hex(attemptId);
        const state = randomToken('', 32), nonce = randomToken('', 32), verifier = randomToken('', 32);
        const exp = now() + TOKEN_TTL_MS.sso_attempt;
        store.tokens.create({ kind: 'sso_attempt', tokenHash: attemptHash, userId: null, data: { challenge: codeChallenge, status: 'pending' }, expiresAt: exp });
        store.tokens.create({ kind: 'sso_state', tokenHash: sha256Hex(state), userId: null, data: { attempt: attemptHash, nonce, verifier }, expiresAt: exp });
        events.record('sso_started', { ip, detail: { provider: PROVIDER } });
        return {
            attemptId,
            authUrl: svc.oidc.authorizationUrl({ state, nonce, codeChallenge: pkceChallenge(verifier) }),
            pollMs: SSO_POLL_MS,
            expiresIn: TOKEN_TTL_MS.sso_attempt / 1000,
        };
    }

    function resolveAccount(claims) {
        const sub = String(claims.sub);
        const link = store.sso.find(PROVIDER, sub);
        if (link) {
            const u = store.users.byId(link.userId);
            return u && u.status === 'active' ? { kind: 'login', userId: u.id } : { kind: 'error', error: 'account_disabled' };
        }
        const email = normalizeEmail(claims.email);
        const googleVerified = claims.email_verified === true || claims.email_verified === 'true';
        if (!email || !googleVerified) return { kind: 'error', error: 'sso_email_unverified' };
        const local = store.users.byEmail(email);
        if (local && local.status === 'active') {
            if (!local.emailVerified) return { kind: 'error', error: 'sso_account_unverified' };
            store.sso.link(local.id, PROVIDER, sub, email);
            events.record('sso_linked', { userId: local.id, detail: { provider: PROVIDER } });
            return { kind: 'login', userId: local.id };
        }
        if (config.registration !== 'open') return { kind: 'error', error: 'registration_closed' };
        const suggestion = suggestUsername(claims.given_name || claims.name || '', config) || suggestUsername(email.split('@')[0], config);
        return { kind: 'new', sub, email, suggestion };
    }

    /**
     * GET /auth/sso/google/callback: returns what the page shows.
     * @returns {Promise<{ ok: boolean, title?: string, message?: string }>}
     */
    async function callback({ code, state, error, ip }) {
        if (!enabled()) return { ok: false, title: 'Google sign-in disabled', message: 'Google sign-in is not enabled on this server.' };
        if (typeof state !== 'string' || !/^[A-Za-z0-9_-]{43}$/.test(state)) return { ok: false, message: 'This sign-in link is invalid.' };
        const st = store.tokens.consume('sso_state', sha256Hex(state), now());
        if (!st || (st.expiresAt != null && st.expiresAt <= now())) return { ok: false, title: 'Sign-in expired', message: 'This sign-in has expired or was already completed.' };
        const sd = dataOf(st);
        const att = store.tokens.get('sso_attempt', sd.attempt);
        if (!isLive(att, now())) return { ok: false, title: 'Sign-in expired', message: 'This sign-in has expired; start again from Scacelith.' };
        const setResult = (result) => store.tokens.update('sso_attempt', sd.attempt, { ...dataOf(att), status: 'done', result });

        if (error || typeof code !== 'string' || !code || code.length > 2048) {
            setResult({ kind: 'error', error: 'sso_cancelled' });
            events.record('sso_cancelled', { ip, detail: { provider: PROVIDER } });
            return { ok: false, title: 'Sign-in cancelled', message: MESSAGES.sso_cancelled };
        }
        let claims;
        try {
            const idToken = await svc.oidc.exchangeCode(code, sd.verifier);
            claims = await svc.oidc.verifyIdToken(idToken, { nonce: sd.nonce });
        } catch (err) {
            log.warn('google sign-in failed', { reason: err.reason || 'error', err: { message: err.message } });
            setResult({ kind: 'error', error: 'sso_failed' });
            events.record('sso_failed', { ip, detail: { provider: PROVIDER, reason: err.reason || 'error' } });
            return { ok: false, message: MESSAGES.sso_failed };
        }
        const r = resolveAccount(claims);
        setResult(r);
        if (r.kind === 'error') return { ok: false, message: MESSAGES[r.error] };
        if (r.kind === 'new') return { ok: true, title: 'Almost there', message: 'You can go back to Scacelith and choose your username.' };
        return { ok: true, message: 'You can go back to Scacelith.' };
    }

    /** POST /auth/sso/google/poll. */
    function poll({ attemptId, codeVerifier, clientLabel = null, ip }) {
        requireEnabled();
        if (!SSO_TOKEN_RE.test(attemptId)) throw expired();
        const h = sha256Hex(attemptId);
        const row = store.tokens.get('sso_attempt', h);
        if (!isLive(row, now())) throw expired();
        const d = dataOf(row);
        if (!safeEqual(pkceChallenge(codeVerifier), String(d.challenge || ''))) {
            events.record('sso_bad_verifier', { ip });
            throw new AuthError(403, 'invalid_verifier', 'This sign-in attempt belongs to another client.');
        }
        if (d.status !== 'done' || !d.result) return { status: 'pending' };
        if (!store.tokens.consume('sso_attempt', h, now())) throw expired();
        const r = d.result;
        if (r.kind === 'login') {
            const user = store.users.byId(r.userId);
            if (!user || user.status !== 'active') throw new AuthError(403, 'account_disabled', MESSAGES.account_disabled);
            login.checkAccountAllowed(user);
            events.record('sso_login', { userId: user.id, ip, detail: { provider: PROVIDER } });
            return login.finishLogin(user, { clientLabel, ip, method: PROVIDER });
        }
        if (r.kind === 'new') {
            const ssoTicket = randomToken('sso_');
            store.tokens.create({ kind: 'sso_ticket', tokenHash: sha256Hex(ssoTicket), userId: null, data: { sub: r.sub, email: r.email }, expiresAt: now() + TOKEN_TTL_MS.sso_ticket });
            return { needsUsername: true, ssoTicket, suggestedUsername: r.suggestion || '' };
        }
        const code = MESSAGES[r.error] ? r.error : 'sso_failed';
        throw new AuthError(STATUS[code] || 502, code, MESSAGES[code]);
    }

    /** POST /auth/sso/complete: creates the account of a first Google sign-in. */
    function complete({ ssoTicket, username, clientLabel = null, ip }) {
        requireEnabled();
        if (config.registration !== 'open') throw new AuthError(403, 'registration_closed', MESSAGES.registration_closed);
        const uErr = checkUsername(username, config);
        if (uErr) throw new AuthError(400, 'invalid_username', uErr);
        if (!SSO_TOKEN_RE.test(ssoTicket)) throw expired();
        const h = sha256Hex(ssoTicket);
        const row = store.tokens.get('sso_ticket', h);
        if (!isLive(row, now())) throw expired();
        const d = dataOf(row);
        if (store.users.byUsername(username)) throw new AuthError(409, 'username_taken', 'This username is already taken.');
        if (!store.tokens.consume('sso_ticket', h, now())) throw expired();
        if (store.sso.find(PROVIDER, d.sub)) throw new AuthError(409, 'sso_already_linked', 'This Google account is already linked; sign in with Google again.');
        let id;
        try {
            id = store.users.create({ username, email: d.email, passwordHash: null, emailVerified: true });
        } catch (err) {
            if (err && err.code === 'username_taken') throw new AuthError(409, 'username_taken', 'This username was just taken; sign in with Google again and choose another one.');
            if (err && err.code === 'email_taken') throw new AuthError(409, 'email_taken', 'An account with this e-mail address was just created; sign in with Google again to use it.');
            throw err;
        }
        store.sso.link(id, PROVIDER, d.sub, d.email);
        events.record('sso_account_created', { userId: id, ip, detail: { provider: PROVIDER } });
        const user = store.users.byId(id);
        return login.sessionAnswer(user, { clientLabel, ip, method: PROVIDER });
    }

    return { enabled, start, callback, poll, complete, normalizeEmail };
}
