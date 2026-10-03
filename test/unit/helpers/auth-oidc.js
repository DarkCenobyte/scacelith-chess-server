// A fake OpenID Connect provider (Google-like) on 127.0.0.1: token endpoint (authorization code +
// PKCE S256 + client secret + the redirect URI of the code's authorization request), JWKS
// endpoint with Cache-Control, RS256 ID tokens signed with a locally generated key. The browser
// step is simulated by authorize() / authorizeError(), which read the authorization URL the server
// built and return the query Google would send the browser back with (tools/live-cpp-check.js
// turns it into Google's 302 to the loopback redirect URI for the C++ live check).

import crypto from 'node:crypto';
import http from 'node:http';
import { LOOPBACK_REDIRECT_RE } from '../../../src/auth/oidc.js';

export function signJwt(payload, { key, kid, alg = 'RS256' }) {
    const h = Buffer.from(JSON.stringify({ alg, kid, typ: 'JWT' })).toString('base64url');
    const p = Buffer.from(JSON.stringify(payload)).toString('base64url');
    const sig = alg === 'RS256' ? crypto.sign('RSA-SHA256', Buffer.from(`${h}.${p}`), key).toString('base64url') : '';
    return `${h}.${p}.${sig}`;
}

export const ISSUER = 'https://accounts.google.com';

/**
 * @param {{ clientId: string, clientSecret: string, now: () => number }} opts
 */
export async function startFakeOidc({ clientId, clientSecret, now }) {
    const keyPair = () => crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
    const state = { kid: 'k1', ...keyPair(), jwksFetches: 0, tokenCalls: [], tamper: null, extraKeys: [] };
    const codes = new Map();

    // The checks of Google's consent page on an authorization request.
    function request(q) {
        if (q.get('client_id') !== clientId || !LOOPBACK_REDIRECT_RE.test(q.get('redirect_uri') || '') || q.get('response_type') !== 'code'
            || q.get('code_challenge_method') !== 'S256' || !q.get('state')) throw new Error('bad authorization request');
    }
    /** The user consents: the query Google redirects with; the code remembers its redirect URI. */
    function consent(q, claims) {
        request(q);
        const code = crypto.randomBytes(16).toString('base64url');
        codes.set(code, { nonce: q.get('nonce'), challenge: q.get('code_challenge'), redirectUri: q.get('redirect_uri'), claims });
        return { code, state: q.get('state'), iss: ISSUER };
    }
    function refusal(q, error) {
        request(q);
        return { error, state: q.get('state') };
    }

    const server = http.createServer((req, res) => {
        const chunks = [];
        req.on('data', (c) => chunks.push(c));
        req.on('end', () => {
            const url = new URL(req.url, 'http://x');
            const json = (status, body, headers = {}) => { res.writeHead(status, { 'Content-Type': 'application/json', ...headers }); res.end(JSON.stringify(body)); };
            if (req.method === 'GET' && url.pathname === '/certs') {
                state.jwksFetches++;
                const keys = [{ ...state.publicKey.export({ format: 'jwk' }), kid: state.kid, alg: 'RS256', use: 'sig' }, ...state.extraKeys];
                return json(200, { keys }, { 'Cache-Control': 'public, max-age=3600, must-revalidate' });
            }
            if (req.method === 'POST' && url.pathname === '/token') {
                const f = Object.fromEntries(new URLSearchParams(Buffer.concat(chunks).toString()));
                state.tokenCalls.push(f);
                if (f.client_id !== clientId || f.client_secret !== clientSecret) return json(401, { error: 'invalid_client' });
                if (f.grant_type !== 'authorization_code') return json(400, { error: 'invalid_request' });
                const c = codes.get(f.code);
                codes.delete(f.code);
                if (!c) return json(400, { error: 'invalid_grant' });
                // As Google: the redirect URI of the code's authorization request, byte for byte (RFC 6749 s.4.1.3).
                if (f.redirect_uri !== c.redirectUri) return json(400, { error: 'invalid_grant', error_description: 'redirect_uri' });
                const challenge = crypto.createHash('sha256').update(f.code_verifier || '').digest('base64url');
                if (challenge !== c.challenge) return json(400, { error: 'invalid_grant', error_description: 'PKCE' });
                const t = Math.floor(now() / 1000);
                let claims = { iss: ISSUER, azp: clientId, aud: clientId, iat: t, exp: t + 3600, nonce: c.nonce, ...c.claims };
                let signKey = state.privateKey, kid = state.kid, alg = 'RS256';
                if (state.tamper) ({ claims, signKey, kid, alg = 'RS256' } = state.tamper({ claims, signKey, kid }) || { claims, signKey, kid });
                return json(200, { access_token: 'ya29.fake', token_type: 'Bearer', expires_in: 3599, scope: 'openid email profile', id_token: signJwt(claims, { key: signKey, kid, alg }) });
            }
            json(404, { error: 'not_found' });
        });
    });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const base = `http://127.0.0.1:${server.address().port}`;

    return {
        state,
        endpoints: {
            authorizationEndpoint: 'https://accounts.google.com/o/oauth2/v2/auth',
            tokenEndpoint: `${base}/token`,
            jwksUri: `${base}/certs`,
            issuers: ['accounts.google.com', ISSUER],
        },
        /** The user consents in the browser: { code, state, iss }, the query Google sends to the redirect URI. */
        authorize: (authUrl, claims) => consent(new URL(authUrl).searchParams, claims),
        /** The user refuses (or Google fails): { error, state }. */
        authorizeError: (authUrl, error) => refusal(new URL(authUrl).searchParams, error),
        rotateKey(kid) { Object.assign(state, keyPair(), { kid }); },
        close: () => new Promise((r) => { server.closeAllConnections?.(); server.close(r); }),
    };
}
