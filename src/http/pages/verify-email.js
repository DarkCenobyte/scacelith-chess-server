// E-mail confirmation page: GET /verify-email?token= shows a button (link scanners must not
// consume the token), POST /verify-email performs the confirmation.

import { escapeHtml, renderMessage, renderPage } from './layout.js';

/**
 * @param {{ serverName: string, token: string }} p
 * @returns {string}
 */
export function verifyForm({ serverName, token }) {
    return renderPage({
        serverName,
        title: 'Confirm your e-mail address',
        body: '<h1>Confirm your e-mail address</h1>' +
            '<p>Press the button to confirm this address for your account.</p>' +
            '<form method="post" action="/verify-email">' +
            `<input type="hidden" name="token" value="${escapeHtml(token)}">` +
            '<button type="submit">Confirm my e-mail address</button></form>',
    });
}

/** @param {{ serverName: string }} p */
export function verifyDone({ serverName }) {
    return renderMessage({ serverName, title: 'E-mail address confirmed', tone: 'ok',
        message: 'Thank you, your e-mail address is confirmed.', note: 'You can go back to Scacelith and log in.' });
}

/** @param {{ serverName: string }} p */
export function verifyInvalid({ serverName }) {
    return renderMessage({ serverName, title: 'Link invalid or expired', tone: 'error',
        message: 'This confirmation link is invalid, was already used, or has expired.',
        note: 'You can ask for a new confirmation e-mail from the login screen of Scacelith.' });
}
