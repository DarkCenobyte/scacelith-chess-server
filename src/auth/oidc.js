// OpenID Connect client for Google sign-in (authorization code flow, PKCE S256): an installed-app
// ("Desktop app") client, loopback redirect per attempt (RFC 8252 s.7.3), client secret held only
// by this server. It builds the authorization URL, exchanges the code at the token endpoint and
// verifies the ID token (RS256 signature with the provider's JWKS, cached according to
// Cache-Control; iss, aud, azp, exp, iat with a small skew, nonce).
//
// The redirect URI is given per call: http://127.0.0.1:<the game's port>/oauth2/google/<origin
// tag>, the game's own listener (auth/sso.js). Both calls refuse any other form
// (OidcError 'bad_redirect_uri'), so that the URI the exchange sends is always one the
// authorization request could carry.
//
// Only node:https is used (node:http only when `allowHttp` is set, for tests against a local
// fake provider). Answers are size-limited and time-limited.

import crypto from 'node:crypto';
import http from 'node:http';
import https from 'node:https';
import { safeEqual } from '../security/keys.js';

export const GOOGLE_OIDC = Object.freeze({
    authorizationEndpoint: 'https://accounts.google.com/o/oauth2/v2/auth',
    tokenEndpoint: 'https://oauth2.googleapis.com/token',
    jwksUri: 'https://www.googleapis.com/oauth2/v3/certs',
    issuers: Object.freeze(['accounts.google.com', 'https://accounts.google.com']),
});

export class OidcError extends Error {
    constructor(reason, message) { super(message || reason); this.reason = reason; }
}

/** The loopback redirect URI of an attempt (contract: docs/API.md, Google sign-in). */
export const LOOPBACK_REDIRECT_RE = /^http:\/\/127\.0\.0\.1:(\d{4,5})\/oauth2\/google\/[A-Za-z0-9_-]{22}$/;

function checkRedirectUri(redirectUri) {
    const m = typeof redirectUri === 'string' ? LOOPBACK_REDIRECT_RE.exec(redirectUri) : null;
    if (!m || +m[1] < 1024 || +m[1] > 65535) throw new OidcError('bad_redirect_uri', 'redirect URI is not a loopback one');
}

/**
 * The origin tag of a server: the first 22 characters of base64url(SHA-256("scacelith-sso-origin-v1\n"
 * + origin)), origin being the lower-case host (an IPv6 literal in brackets), ':' and the API port
 * as players reach it ("play.example.org:443"). It is the last part of the loopback redirect path,
 * which the game recomputes from the server it is connected to: a server can only get a Google URL
 * carrying its own tag, so it cannot relay a sign-in started with another server.
 * @param {string} origin
 * @returns {string}
 */
export function ssoOriginTag(origin) {
    return crypto.createHash('sha256').update(`scacelith-sso-origin-v1\n${origin}`, 'utf8').digest('base64url').slice(0, 22);
}

/**
 * Minimal HTTPS request.
 * @param {string} url
 * @param {{ method?: string, headers?: object, body?: string, timeoutMs?: number, maxBytes?: number, allowHttp?: boolean }} [opts]
 * @returns {Promise<{ status: number, headers: object, body: string }>}
 */
export function httpRequest(url, { method = 'GET', headers = {}, body, timeoutMs = 10000, maxBytes = 1 << 20, allowHttp = false } = {}) {
    return new Promise((resolve, reject) => {
        const u = new URL(url);
        if (u.protocol !== 'https:' && !(allowHttp && u.protocol === 'http:')) { reject(new OidcError('insecure_endpoint', 'endpoint must be https')); return; }
        const mod = u.protocol === 'https:' ? https : http;
        const h = { Accept: 'application/json', ...headers };
        if (body !== undefined) h['Content-Length'] = Buffer.byteLength(body);
        const req = mod.request(u, { method, headers: h, timeout: timeoutMs }, (res) => {
            const chunks = [];
            let size = 0;
            res.on('data', (c) => {
                size += c.length;
                if (size > maxBytes) { req.destroy(new OidcError('response_too_large', 'provider answer too large')); return; }
                chunks.push(c);
            });
            res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString('utf8') }));
            res.on('error', (e) => reject(new OidcError('network', e.message)));
        });
        req.on('timeout', () => req.destroy(new OidcError('timeout', 'provider timeout')));
        req.on('error', (e) => reject(e instanceof OidcError ? e : new OidcError('network', e.message)));
        if (body !== undefined) req.write(body);
        req.end();
    });
}

/**
 * Splits and decodes a JWS compact token (without verifying it).
 * @param {string} token
 * @returns {{ header: object, payload: object, signingInput: string, signature: Buffer }}
 */
export function decodeJwt(token) {
    if (typeof token !== 'string' || token.length > 16384) throw new OidcError('malformed_token');
    const parts = token.split('.');
    if (parts.length !== 3 || parts.some((p) => !/^[A-Za-z0-9_-]*$/.test(p))) throw new OidcError('malformed_token');
    let header, payload;
    try {
        header = JSON.parse(Buffer.from(parts[0], 'base64url').toString('utf8'));
        payload = JSON.parse(Buffer.from(parts[1], 'base64url').toString('utf8'));
    } catch {
        throw new OidcError('malformed_token');
    }
    if (!header || typeof header !== 'object' || !payload || typeof payload !== 'object') throw new OidcError('malformed_token');
    return { header, payload, signingInput: `${parts[0]}.${parts[1]}`, signature: Buffer.from(parts[2], 'base64url') };
}

function maxAgeMs(cacheControl) {
    const m = /max-age=(\d+)/i.exec(String(cacheControl || ''));
    const s = m ? +m[1] : 3600;
    return Math.min(24 * 3600, Math.max(60, s)) * 1000;
}

/** S256 PKCE challenge of a verifier. */
export function pkceChallenge(verifier) {
    return crypto.createHash('sha256').update(verifier, 'ascii').digest('base64url');
}

/**
 * @param {{ clientId: string, clientSecret: string, endpoints?: typeof GOOGLE_OIDC,
 *           now?: () => number, skewMs?: number, allowHttp?: boolean, request?: typeof httpRequest }} opts
 */
export function createOidcClient({ clientId, clientSecret, endpoints = GOOGLE_OIDC, now = Date.now,
    skewMs = 120000, allowHttp = false, request = httpRequest }) {
    let jwks = { keys: new Map(), expiresAt: 0, fetchedAt: 0 };
    let inflight = null;

    async function fetchJwks() {
        const r = await request(endpoints.jwksUri, { allowHttp });
        if (r.status !== 200) throw new OidcError('jwks_unavailable', `JWKS answer ${r.status}`);
        let doc;
        try { doc = JSON.parse(r.body); } catch { throw new OidcError('jwks_unavailable', 'JWKS is not JSON'); }
        const keys = new Map();
        for (const k of Array.isArray(doc.keys) ? doc.keys : []) {
            if (k.kty !== 'RSA' || !k.kid || (k.use && k.use !== 'sig') || (k.alg && k.alg !== 'RS256')) continue;
            try { keys.set(k.kid, crypto.createPublicKey({ key: { kty: 'RSA', n: k.n, e: k.e }, format: 'jwk' })); } catch { /* skip a bad key */ }
        }
        const t = now();
        jwks = { keys, expiresAt: t + maxAgeMs(r.headers['cache-control']), fetchedAt: t };
    }

    async function keyFor(kid) {
        const t = now();
        const stale = t >= jwks.expiresAt;
        const unknown = !jwks.keys.has(kid) && t - jwks.fetchedAt > 60000;
        if (stale || unknown) {
            if (!inflight) inflight = fetchJwks().finally(() => { inflight = null; });
            await inflight;
        }
        const key = jwks.keys.get(kid);
        if (!key) throw new OidcError('unknown_key', 'no provider key for this token');
        return key;
    }

    /**
     * The URL of the provider's consent page.
     * @param {{ state: string, nonce: string, codeChallenge: string, redirectUri: string }} p
     * @returns {string}
     */
    function authorizationUrl({ state, nonce, codeChallenge, redirectUri }) {
        checkRedirectUri(redirectUri);
        const u = new URL(endpoints.authorizationEndpoint);
        u.searchParams.set('client_id', clientId);
        u.searchParams.set('redirect_uri', redirectUri);
        u.searchParams.set('response_type', 'code');
        u.searchParams.set('scope', 'openid email profile');
        u.searchParams.set('state', state);
        u.searchParams.set('nonce', nonce);
        u.searchParams.set('code_challenge', codeChallenge);
        u.searchParams.set('code_challenge_method', 'S256');
        u.searchParams.set('prompt', 'select_account');
        return u.toString();
    }

    /**
     * Exchanges an authorization code (with our PKCE verifier and the redirect URI of its
     * authorization request, byte for byte) for the ID token.
     * @returns {Promise<string>} the ID token
     */
    async function exchangeCode(code, codeVerifier, redirectUri) {
        checkRedirectUri(redirectUri);
        const body = new URLSearchParams({
            grant_type: 'authorization_code', code, redirect_uri: redirectUri,
            client_id: clientId, client_secret: clientSecret, code_verifier: codeVerifier,
        }).toString();
        const r = await request(endpoints.tokenEndpoint, {
            method: 'POST', body, allowHttp, headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        });
        let doc = null;
        try { doc = JSON.parse(r.body); } catch { /* handled below */ }
        if (r.status !== 200 || !doc || typeof doc.id_token !== 'string') {
            throw new OidcError('token_exchange_failed', `token endpoint answer ${r.status}${doc && doc.error ? ' ' + String(doc.error).slice(0, 60) : ''}`);
        }
        return doc.id_token;
    }

    /**
     * Verifies an ID token and returns its claims.
     * @param {string} idToken
     * @param {{ nonce: string }} expect
     * @returns {Promise<object>} claims (sub, email, email_verified, name...)
     */
    async function verifyIdToken(idToken, { nonce }) {
        const { header, payload: c, signingInput, signature } = decodeJwt(idToken);
        if (header.alg !== 'RS256') throw new OidcError('bad_algorithm', 'ID token algorithm must be RS256');
        if (typeof header.kid !== 'string') throw new OidcError('unknown_key');
        const key = await keyFor(header.kid);
        if (!crypto.verify('RSA-SHA256', Buffer.from(signingInput, 'ascii'), key, signature)) throw new OidcError('bad_signature', 'ID token signature invalid');
        if (!endpoints.issuers.includes(c.iss)) throw new OidcError('bad_issuer');
        const aud = Array.isArray(c.aud) ? c.aud : [c.aud];
        if (!aud.includes(clientId)) throw new OidcError('bad_audience');
        if (aud.length > 1 && c.azp !== clientId) throw new OidcError('bad_audience');
        if (c.azp !== undefined && c.azp !== clientId) throw new OidcError('bad_audience');
        const t = now() / 1000, skew = skewMs / 1000;
        if (typeof c.exp !== 'number' || c.exp + skew < t) throw new OidcError('expired');
        if (typeof c.iat !== 'number' || c.iat - skew > t) throw new OidcError('issued_in_future');
        if (typeof c.nonce !== 'string' || !nonce || !safeEqual(c.nonce, nonce)) throw new OidcError('bad_nonce');
        if (typeof c.sub !== 'string' || !c.sub || c.sub.length > 255) throw new OidcError('bad_subject');
        return c;
    }

    return { authorizationUrl, exchangeCode, verifyIdToken, issuers: endpoints.issuers, _jwks: () => jwks };
}
