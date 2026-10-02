// E-mail confirmation page: GET /verify-email?token= shows a button (link scanners must not
// consume the token), POST /verify-email performs the confirmation (and creates the account of a
// pending signup, auth/accounts.js).

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

/**
 * Another account took the username or the address of a pending signup before its link was used.
 * @param {{ serverName: string }} p
 */
export function verifyTaken({ serverName }) {
    return renderMessage({ serverName, title: 'Account not created', tone: 'error',
        message: 'Another account took this username or this e-mail address before the link was used.',
        note: 'Create your account again from Scacelith, with another username, or sign in if this address already has an account.' });
}

/** @param {{ serverName: string }} p */
export function verifyInvalid({ serverName }) {
    return renderMessage({ serverName, title: 'Link invalid or expired', tone: 'error',
        message: 'This confirmation link is invalid, was already used, or has expired.',
        note: 'If you have just signed up, press "Resend the e-mail" on the page Scacelith shows after signing up, or create ' +
            'your account again from Scacelith (the same username and address work), to receive a new link (at most one every ' +
            '5 minutes). An existing account can ask for a new confirmation e-mail from the login screen of Scacelith.' });
}
