// Password reset page: GET /reset-password?token= shows the form, POST /reset-password sets the
// new password (the token travels in the form; it is single use and expires after an hour).

import { escapeHtml, renderMessage, renderPage } from './layout.js';
import { PASSWORD_MAX_BYTES } from '../../security/password.js';

/**
 * @param {{ serverName: string, token: string, minLength: number, error?: string }} p
 * @returns {string}
 */
export function resetForm({ serverName, token, minLength, error = '' }) {
    return renderPage({
        serverName,
        title: 'Choose a new password',
        body: '<h1>Choose a new password</h1>' +
            (error ? `<p class="error">${escapeHtml(error)}</p>` : '') +
            `<p>At least ${minLength} characters. Every device signed in to your account will be signed out.</p>` +
            '<form method="post" action="/reset-password">' +
            `<input type="hidden" name="token" value="${escapeHtml(token)}">` +
            `<label for="np">New password</label><input id="np" type="password" name="newPassword" autocomplete="new-password" minlength="${minLength}" maxlength="${PASSWORD_MAX_BYTES}" required>` +
            `<label for="cp">Repeat the new password</label><input id="cp" type="password" name="confirmPassword" autocomplete="new-password" minlength="${minLength}" maxlength="${PASSWORD_MAX_BYTES}" required>` +
            '<button type="submit">Change my password</button></form>',
    });
}

/** @param {{ serverName: string }} p */
export function resetDone({ serverName }) {
    return renderMessage({ serverName, title: 'Password changed', tone: 'ok',
        message: 'Your password has been changed and every device was signed out.',
        note: 'You can go back to Scacelith and log in with your new password.' });
}

/** @param {{ serverName: string }} p */
export function resetInvalid({ serverName }) {
    return renderMessage({ serverName, title: 'Link invalid or expired', tone: 'error',
        message: 'This reset link is invalid, was already used, or has expired.',
        note: 'You can ask for a new one with "Forgot password" in Scacelith.' });
}
