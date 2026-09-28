// The page the browser lands on after Google (GET /auth/sso/google/callback). It never shows a
// token: the game picks up the result by polling with its PKCE verifier.

import { renderMessage } from './layout.js';

/**
 * @param {{ serverName: string, ok: boolean, title?: string, message?: string }} p
 * @returns {string}
 */
export function ssoResult({ serverName, ok, title, message }) {
    if (ok) {
        return renderMessage({ serverName, title: title || 'Signed in with Google', tone: 'ok',
            message: message || 'You can go back to Scacelith.', note: 'You may close this browser tab.' });
    }
    return renderMessage({ serverName, title: title || 'Sign-in failed', tone: 'error',
        message: message || 'The sign-in with Google could not be completed.',
        note: 'You can go back to Scacelith and try again.' });
}
