// E-mail change confirmation page: GET /confirm-email-change?token= shows the new address and a
// button (link scanners must not consume the token), POST /confirm-email-change applies the
// change (auth/accounts.js confirmEmailChange).

import { escapeHtml, renderMessage, renderPage } from './layout.js';

/**
 * @param {{ serverName: string, token: string, email: string, username: string }} p
 * @returns {string}
 */
export function emailChangeForm({ serverName, token, email, username }) {
    return renderPage({
        serverName,
        title: 'Confirm your new e-mail address',
        body: '<h1>Confirm your new e-mail address</h1>' +
            `<p>Press the button to use <strong>${escapeHtml(email)}</strong> for the account <strong>${escapeHtml(username)}</strong>.</p>` +
            '<p class="note">Messages about the account, password resets included, will then go to this address.</p>' +
            '<form method="post" action="/confirm-email-change">' +
            `<input type="hidden" name="token" value="${escapeHtml(token)}">` +
            '<button type="submit">Use this e-mail address</button></form>',
    });
}

/** @param {{ serverName: string, email: string }} p */
export function emailChangeDone({ serverName, email }) {
    return renderMessage({ serverName, title: 'E-mail address changed', tone: 'ok',
        message: `Your account now uses ${email}.`,
        note: 'Your devices stay signed in. You can go back to Scacelith.' });
}

/** @param {{ serverName: string }} p */
export function emailChangeInvalid({ serverName }) {
    return renderMessage({ serverName, title: 'Link invalid or expired', tone: 'error',
        message: 'This confirmation link is invalid, was already used, or has expired.',
        note: 'Your e-mail address did not change. You can ask for the change again in Scacelith.' });
}

/** @param {{ serverName: string }} p */
export function emailChangeTaken({ serverName }) {
    return renderMessage({ serverName, title: 'Address already used', tone: 'error',
        message: 'Another account now uses this e-mail address, so it cannot be given to yours.',
        note: 'Your e-mail address did not change.' });
}
