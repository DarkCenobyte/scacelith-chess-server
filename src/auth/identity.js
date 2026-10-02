// Username and e-mail rules.
//
// Usernames: USERNAME_MIN..USERNAME_MAX characters of [A-Za-z0-9_-], starting with a letter or a
// digit; reserved names (and names starting with a reserved prefix, to prevent impersonation of
// staff and system accounts) are refused. Uniqueness is case-insensitive (Store).
// E-mail addresses: plain ASCII addresses (no internationalised local parts), at most 254
// characters, a dotted domain; stored and compared in lower case.

import { isValidAddress } from '../mail/message.js';

export const USERNAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9_-]*$/;

export const RESERVED_USERNAMES = new Set([
    'admin', 'administrator', 'root', 'system', 'sysop', 'scacelith', 'stockfish', 'moderator', 'mod', 'mods',
    'staff', 'support', 'help', 'helpdesk', 'server', 'official', 'owner', 'operator', 'team', 'security', 'abuse',
    'postmaster', 'webmaster', 'hostmaster', 'noreply', 'no-reply', 'info', 'contact', 'anonymous', 'deleted',
    'unknown', 'null', 'undefined', 'none', 'guest', 'everyone', 'here', 'console', 'api', 'www', 'mail',
    'bot', 'engine', 'computer', 'arbiter', 'referee', 'robot', 'ai', 'white', 'black', 'you', 'me',
]);
const RESERVED_PREFIXES = ['admin', 'moderator', 'scacelith', 'stockfish', 'sysop', 'deleted'];

/**
 * Checks a new username. Returns null when acceptable, else an English message.
 * @param {string} name
 * @param {{ usernameMin: number, usernameMax: number }} config
 * @returns {string|null}
 */
export function checkUsername(name, config) {
    if (typeof name !== 'string') return 'The username is required.';
    if (name.length < config.usernameMin || name.length > config.usernameMax) {
        return `The username must have ${config.usernameMin} to ${config.usernameMax} characters.`;
    }
    if (!USERNAME_PATTERN.test(name)) return 'The username may only contain letters, digits, _ and -, and must start with a letter or a digit.';
    const lower = name.toLowerCase();
    if (RESERVED_USERNAMES.has(lower) || RESERVED_PREFIXES.some((p) => lower.startsWith(p))) return 'This username is reserved.';
    return null;
}

/**
 * Canonical form of an e-mail address (trimmed, lower case).
 * @param {string} email
 * @returns {string}
 */
export function normalizeEmail(email) {
    return String(email ?? '').trim().toLowerCase();
}

/**
 * An address with its local part hidden but its first character: "n***@example.org" (notices
 * about an address that must not be shown in full).
 * @param {string} email
 * @returns {string}
 */
export function maskEmail(email) {
    const s = String(email ?? '').trim();
    const at = s.lastIndexOf('@');
    if (at <= 0) return '***';
    return s[0] + '***' + s.slice(at);
}

/**
 * Sanity check of an e-mail address (already normalised).
 * @param {string} email
 * @returns {boolean}
 */
export function isValidEmail(email) {
    if (!isValidAddress(email) || email.length > 254) return false;
    const domain = email.slice(email.indexOf('@') + 1);
    return domain.includes('.') && !domain.startsWith('.') && !domain.endsWith('.') && !email.includes('..');
}

/**
 * A username derived from a display name or an e-mail address ('' when none fits).
 * @param {string} source
 * @param {{ usernameMin: number, usernameMax: number }} config
 * @returns {string}
 */
export function suggestUsername(source, config) {
    let s = String(source || '').normalize('NFKD').replace(/[̀-ͯ]/g, '').replace(/\s+/g, '_').replace(/[^A-Za-z0-9_-]/g, '');
    s = s.replace(/^[_-]+/, '').slice(0, config.usernameMax);
    return checkUsername(s, config) ? '' : s;
}
