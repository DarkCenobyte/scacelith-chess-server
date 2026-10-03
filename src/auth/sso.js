// Google sign-in for a desktop game (docs/DESIGN.md 5.9, docs/API.md "Google sign-in"): the
// installed-app flow with a loopback redirect (RFC 8252 s.7.3).
//
//  1. start   The game listens on 127.0.0.1 (a port the system chose), makes a PKCE pair and
//             sends { codeChallenge, redirectPort }. The server creates an attempt (10 min) with
//             its own state, nonce and PKCE pair for Google, and the redirect URI
//             http://127.0.0.1:<redirectPort>/oauth2/google/<this server's origin tag>
//             (config.ssoRedirectTag, auth/oidc.js ssoOriginTag), built from the port alone. It
//             answers the attempt id, the Google URL and the state; the game checks the URL (the
//             tag of the server it is connected to) and opens it in the system browser.
//  2.         Google sends the browser to the game's listener with code and state; this server is
//             not involved. A browser that someone else's link brought to Google lands on its own
//             127.0.0.1, so whoever started that attempt never gets the code.
//  3. finish  The game posts the attempt id, its PKCE verifier (S256 must match the start
//             challenge: an attempt id alone is useless), the state and the code. The attempt is
//             consumed; the state and the issuer are checked; the code is exchanged (client
//             secret, the attempt's verifier and redirect URI) and the ID token verified. The
//             answer: a login answer (session or MFA step) for a linked Google account,
//             { needsUsername, ssoTicket } for a new account, or { needsPassword, linkTicket,
//             username } when an account with a password uses the address.
//  4. link    { linkTicket, password }: the account's password, typed in the game (5 tries per
//             ticket, the login's failure counter and proof of work). The link is stored then, or,
//             with two-step verification on, once POST /auth/login/mfa accepts a code; the
//             account's address gets a notice (mail ssoLinked).
//  5. complete (new accounts) { ssoTicket, username } creates the account, links it and logs in;
//             the address gets a notice (mail ssoAccountCreated). A sign-in by an existing link
//             sends none.
//
// INVARIANT: A Google identity is attached to an existing account only after the person proved
// the Google address (ID token, email_verified) AND the account (its current password, plus its
// second factor when on), in the game. It never depends on users.email_verified or
// REQUIRE_EMAIL_VERIFICATION. A Google-created account is linked at creation (complete()).
//
// Account resolution: an existing link -> that user; else an active account with the Google
// address: the password step when it has a usable password, else 409 sso_account_exists (the
// account is not named); otherwise a new account (needs a username; a pending signup of the
// address is no account: its link then finds the address taken, auth/accounts.js).

import { AuthError, serverBusy } from './errors.js';
import { SSO_TOKEN_RE, TOKEN_TTL_MS, dataOf, isLive } from './tokens.js';
import { checkUsername, normalizeEmail, suggestUsername } from './identity.js';
import { pkceChallenge } from './oidc.js';
import { randomToken, safeEqual, sha256Hex } from '../security/keys.js';

const PROVIDER = 'google';
/** Password tries per link ticket. */
export const LINK_TRIES = 5;

const MESSAGES = {
    sso_failed: 'Google sign-in could not be completed.',
    sso_email_unverified: 'Google has not confirmed the e-mail address of this Google account.',
    sso_account_exists: 'An account already uses this e-mail address and cannot be linked to Google sign-in here. Sign in to it as usual, or ask the server\'s operator.',
    sso_already_linked: 'This Google account was linked to another account meanwhile.',
    sso_expired: 'This sign-in has expired; start again from Scacelith.',
    registration_closed: 'Registration is closed on this server.',
    account_disabled: 'This account can no longer be used.',
};
const STATUS = {
    sso_failed: 502, sso_email_unverified: 403, sso_account_exists: 409, sso_already_linked: 409, sso_expired: 410,
    registration_closed: 403, account_disabled: 403,
};

/** A password the account can sign in with (not a Google-only account, not a bench account's '!' hash). */
const usablePassword = (user) => typeof user.passwordHash === 'string' && !user.passwordHash.startsWith('!');

/**
 * @param {object} svc the auth service internals (see auth/index.js); svc.oidc is the provider client
 */
export function createSso(svc) {
    const { config, store, now, events, log, login } = svc;

    const enabled = () => !!(config.ssoGoogleEnabled && svc.oidc);
    function requireEnabled() {
        if (!enabled()) throw new AuthError(404, 'sso_disabled', 'Google sign-in is not enabled on this server.');
    }
    const fail = (code) => new AuthError(STATUS[code], code, MESSAGES[code]);
    const expired = () => fail('sso_expired');

    /** POST /auth/sso/google/start. */
    function start({ codeChallenge, redirectPort, ip }) {
        requireEnabled();
        const attemptId = randomToken('sso_');
        const state = randomToken('', 32), nonce = randomToken('', 32), verifier = randomToken('', 32);
        // From the port alone and this server's own tag: never a host, path or URI of the client.
        const redirectUri = `http://127.0.0.1:${redirectPort}/oauth2/google/${config.ssoRedirectTag}`;
        const authUrl = svc.oidc.authorizationUrl({ state, nonce, codeChallenge: pkceChallenge(verifier), redirectUri });
        store.tokens.create({
            kind: 'sso_attempt', tokenHash: sha256Hex(attemptId), userId: null,
            data: { challenge: codeChallenge, stateHash: sha256Hex(state), nonce, verifier, redirectUri },
            expiresAt: now() + TOKEN_TTL_MS.sso_attempt,
        });
        events.record('sso_started', { ip, detail: { provider: PROVIDER } });
        return { attemptId, authUrl, state, expiresIn: TOKEN_TTL_MS.sso_attempt / 1000 };
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
            // Never linked here, whatever the account's address confirmation: link() asks its password.
            if (usablePassword(local)) return { kind: 'link', userId: local.id, username: local.username, sub, email };
            return { kind: 'error', error: 'sso_account_exists' };
        }
        if (config.registration !== 'open') return { kind: 'error', error: 'registration_closed' };
        const suggestion = suggestUsername(claims.given_name || claims.name || '', config) || suggestUsername(email.split('@')[0], config);
        return { kind: 'new', sub, email, suggestion };
    }

    /** POST /auth/sso/google/finish: the code Google sent to the game's listener. */
    async function finish({ attemptId, codeVerifier, state, code, iss, clientLabel = null, ip }) {
        requireEnabled();
        if (!SSO_TOKEN_RE.test(attemptId)) throw expired();
        const h = sha256Hex(attemptId);
        const row = store.tokens.get('sso_attempt', h);
        if (!isLive(row, now())) throw expired();
        const d = dataOf(row);
        // An attempt stored before this flow (browser callback) has none of these.
        if (typeof d.stateHash !== 'string' || typeof d.redirectUri !== 'string' || typeof d.verifier !== 'string') throw expired();
        if (!safeEqual(pkceChallenge(codeVerifier), String(d.challenge || ''))) {
            events.record('sso_bad_verifier', { ip });
            throw new AuthError(403, 'invalid_verifier', 'This sign-in attempt belongs to another client.');
        }
        if (!store.tokens.consume('sso_attempt', h, now())) throw expired();
        // Never Google's text, the code or the state in the answer, the log or the event.
        const failed = (reason, err) => {
            log.warn('google sign-in failed', { reason, ...(err ? { err: { message: err.message } } : {}) });
            events.record('sso_failed', { ip, detail: { provider: PROVIDER, reason } });
            return fail('sso_failed');
        };
        if (!safeEqual(sha256Hex(state), d.stateHash)) throw failed('state_mismatch');
        if (iss !== undefined && !svc.oidc.issuers.includes(iss)) throw failed('bad_iss');
        let claims;
        try {
            const idToken = await svc.oidc.exchangeCode(code, d.verifier, d.redirectUri);
            claims = await svc.oidc.verifyIdToken(idToken, { nonce: d.nonce });
        } catch (err) {
            throw failed(err.reason || 'error', err);
        }
        const r = resolveAccount(claims);
        if (r.kind === 'login') {
            const user = store.users.byId(r.userId);
            if (!user || user.status !== 'active') throw fail('account_disabled');
            login.checkAccountAllowed(user);
            events.record('sso_login', { userId: user.id, ip, detail: { provider: PROVIDER } });
            return login.finishLogin(user, { clientLabel, ip, method: PROVIDER });
        }
        if (r.kind === 'new') {
            const ssoTicket = randomToken('sso_');
            store.tokens.create({ kind: 'sso_ticket', tokenHash: sha256Hex(ssoTicket), userId: null, data: { sub: r.sub, email: r.email }, expiresAt: now() + TOKEN_TTL_MS.sso_ticket });
            return { needsUsername: true, ssoTicket, suggestedUsername: r.suggestion || '' };
        }
        if (r.kind === 'link') {
            const linkTicket = randomToken('sso_');
            store.tokens.create({
                kind: 'sso_link', tokenHash: sha256Hex(linkTicket), userId: r.userId,
                data: { userId: r.userId, sub: r.sub, email: r.email, tries: 0 }, expiresAt: now() + TOKEN_TTL_MS.sso_link,
            });
            events.record('sso_link_required', { userId: r.userId, detail: { provider: PROVIDER } });
            return { needsPassword: true, linkTicket, username: r.username, expiresIn: TOKEN_TTL_MS.sso_link / 1000 };
        }
        throw fail(r.error);
    }

    /**
     * Stores the Google link of `user` after its password (and, with `expectMfa`, its second
     * factor) was proven for `proven` ({ sub, email, pwh }), in one transaction with the checks
     * that the account is still the one proven: active, the same address and password hash, two-step
     * verification as it was (410 sso_expired otherwise). 409 sso_already_linked when the Google
     * identity was linked to another account meanwhile. The link confirms the address, and the
     * address gets a notice (mail ssoLinked).
     */
    function linkProven(user, proven, ip, { expectMfa }) {
        let linked = null;
        try {
            svc.atomically(() => {
                const u = store.users.byId(user.id);
                if (!u || u.status !== 'active' || normalizeEmail(u.email) !== proven.email || sha256Hex(u.passwordHash || '') !== proven.pwh
                    || !!u.mfaEnabled !== expectMfa) throw expired();
                const holder = store.sso.find(PROVIDER, proven.sub);
                if (holder && holder.userId !== u.id) throw fail('sso_already_linked');
                try {
                    store.sso.link(u.id, PROVIDER, proven.sub, proven.email);
                } catch (err) {
                    if (err && err.code === 'sso_taken') throw fail('sso_already_linked');
                    throw err;
                }
                if (!u.emailVerified) store.users.update(u.id, { emailVerified: true });
                linked = u;
            });
        } catch (err) {
            if (err && err.code === 'busy' && !err.expose) throw serverBusy(1);
            throw err;
        }
        events.record('sso_linked', { userId: user.id, ip, detail: { provider: PROVIDER, method: expectMfa ? 'password+totp' : 'password' } });
        svc.mail('ssoLinked', linked.email, { username: linked.username, when: new Date(now()) });
    }

    /** POST /auth/sso/google/link: the account's password, typed in the game, before its Google link. */
    async function link({ linkTicket, password, clientLabel = null, pow, ip }) {
        requireEnabled();
        if (!SSO_TOKEN_RE.test(linkTicket)) throw expired();
        const h = sha256Hex(linkTicket);
        const row = store.tokens.get('sso_link', h);
        if (!isLive(row, now())) throw expired();
        const d = dataOf(row);
        const user = store.users.byId(d.userId);
        if (!user || user.status !== 'active') {
            store.tokens.consume('sso_link', h, now());
            throw expired();
        }
        // One of the ticket's tries, taken after the login's wait (429) and proof of work (428) and
        // before the hash: parallel requests cannot pass LINK_TRIES.
        let tries = 0;
        const reserve = () => {
            const r = store.tokens.reserveTry('sso_link', h, LINK_TRIES, now());
            if (!r) throw expired();
            tries = dataOf(r).tries;
        };
        let current;
        try {
            current = await login.verifyPassword({ key: 'l:' + user.username.toLowerCase(), user, password, ip, pow, method: 'google_link', reserve });
        } catch (err) {
            // The last try's wrong password ends the ticket; a wait, a proof of work or a refusal of
            // the hash queue keep it (the queue's after taking the try).
            if (err instanceof AuthError && err.code === 'invalid_credentials' && tries >= LINK_TRIES) {
                store.tokens.consume('sso_link', h, now());
                throw expired();
            }
            throw err;
        }
        if (!store.tokens.consume('sso_link', h, now())) throw expired();
        if (current.status !== 'active' || normalizeEmail(current.email) !== d.email || !usablePassword(current)) throw expired();
        login.checkAccountAllowed(current, { addressProven: true });
        // The hash the password matched now (after a rehash): the MFA step and linkProven check it.
        const proven = { sub: d.sub, email: d.email, pwh: sha256Hex(current.passwordHash) };
        if (current.mfaEnabled) return login.finishLogin(current, { clientLabel, ip, method: PROVIDER, extra: { link: proven, pwh: proven.pwh } });
        linkProven(current, proven, ip, { expectMfa: false });
        return login.sessionAnswer(store.users.byId(current.id) || current, { clientLabel, ip, method: 'google+password' });
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
        // A username held by a pending signup of another address is taken as well (auth/accounts.js).
        if (store.users.byUsername(username) || svc.accounts.usernameHeld(username, d.email)) {
            throw new AuthError(409, 'username_taken', 'This username is already taken.');
        }
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
        svc.mail('ssoAccountCreated', user.email, { username: user.username, when: new Date(now()) });
        return login.sessionAnswer(user, { clientLabel, ip, method: PROVIDER });
    }

    return { enabled, start, finish, link, linkProven, complete };
}
