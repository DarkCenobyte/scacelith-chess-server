// RFC 5322 plain-text messages: headers (From, To, Subject, Date, Message-ID, MIME), RFC 2047
// encoded-word subjects and display names, 7bit bodies when possible and quoted-printable
// otherwise. Header values are checked for CR/LF (no header injection).

import crypto from 'node:crypto';

export class MailError extends Error {
    constructor(code, message) { super(message || code); this.code = code; }
}

const ADDRESS_RE = /^[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]{1,64}@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*$/;

/**
 * True when `s` is a plain ASCII e-mail address this module can send to.
 * @param {string} s
 * @returns {boolean}
 */
export function isValidAddress(s) {
    return typeof s === 'string' && s.length <= 254 && ADDRESS_RE.test(s);
}

/**
 * Parses "Name <addr@host>" or "addr@host".
 * @param {string} s
 * @returns {{ name: string, address: string }}
 */
export function parseMailbox(s) {
    const str = String(s).trim();
    const m = /^(.*?)\s*<([^<>]+)>$/.exec(str);
    const r = m ? { name: m[1].replace(/^"|"$/g, '').trim(), address: m[2].trim() } : { name: '', address: str };
    if (!isValidAddress(r.address)) throw new MailError('bad_address', 'invalid mailbox');
    if (/[\r\n]/.test(r.name)) throw new MailError('bad_header', 'line break in a header');
    return r;
}

function isPrintableAscii(s) { return /^[\x20-\x7e]*$/.test(s); }

/**
 * RFC 2047 encoding of a header text when it is not plain printable ASCII (UTF-8, base64
 * encoded-words of at most 75 characters, folded).
 * @param {string} s
 * @returns {string}
 */
export function encodeHeaderText(s) {
    if (/[\r\n]/.test(s)) throw new MailError('bad_header', 'line break in a header');
    if (isPrintableAscii(s) && s.length <= 900) return s;
    const words = [];
    let chunk = '';
    for (const ch of s) {
        if (Buffer.byteLength(chunk + ch, 'utf8') > 45) { words.push(chunk); chunk = ''; }
        chunk += ch;
    }
    if (chunk) words.push(chunk);
    return words.map((w) => `=?UTF-8?B?${Buffer.from(w, 'utf8').toString('base64')}?=`).join('\r\n ');
}

function formatMailbox({ name, address }) {
    if (!name) return address;
    const n = isPrintableAscii(name) && !/["\\]/.test(name) ? `"${name}"` : encodeHeaderText(name);
    return `${n} <${address}>`;
}

/**
 * RFC 5322 date, e.g. "Mon, 28 Sep 2026 14:03:05 +0000".
 * @param {Date} d
 * @returns {string}
 */
export function rfc5322Date(d) {
    return d.toUTCString().replace(/GMT$/, '+0000');
}

/**
 * Quoted-printable encoding (RFC 2045) of UTF-8 text with CRLF line breaks.
 * @param {string} text
 * @returns {string}
 */
export function quotedPrintable(text) {
    const out = [];
    for (const line of text.split('\r\n')) {
        const bytes = Buffer.from(line, 'utf8');
        let enc = '';
        for (let i = 0; i < bytes.length; i++) {
            const b = bytes[i];
            const last = i === bytes.length - 1;
            let piece;
            if ((b === 0x20 || b === 0x09) && !last) piece = String.fromCharCode(b);
            else if (b >= 0x21 && b <= 0x7e && b !== 0x3d) piece = String.fromCharCode(b);
            else piece = '=' + b.toString(16).toUpperCase().padStart(2, '0');
            enc += piece;
        }
        // Soft line breaks: at most 76 characters per encoded line, never inside an =XX escape.
        while (enc.length > 76) {
            let cut = 75;
            if (enc[cut - 1] === '=') cut -= 1;
            else if (enc[cut - 2] === '=') cut -= 2;
            out.push(enc.slice(0, cut) + '=');
            enc = enc.slice(cut);
        }
        out.push(enc);
    }
    return out.join('\r\n');
}

/**
 * Builds the complete message (headers + body, CRLF line breaks, without the SMTP terminator).
 * @param {{ from: string, to: string, subject: string, text: string, date?: Date, messageId?: string }} m
 * @returns {{ raw: string, from: string, to: string, messageId: string }} envelope addresses and message
 */
export function buildMessage({ from, to, subject, text, date = new Date(), messageId }) {
    const f = parseMailbox(from);
    const t = parseMailbox(to);
    const domain = f.address.split('@')[1];
    const id = messageId || `<${crypto.randomBytes(16).toString('hex')}@${domain}>`;
    const body = String(text).replace(/\r?\n/g, '\r\n');
    const sevenBit = /^[\x00-\x7f]*$/.test(body) && body.split('\r\n').every((l) => l.length <= 998);
    const headers = [
        `From: ${formatMailbox(f)}`,
        `To: ${formatMailbox(t)}`,
        `Subject: ${encodeHeaderText(String(subject))}`,
        `Date: ${rfc5322Date(date)}`,
        `Message-ID: ${id}`,
        'MIME-Version: 1.0',
        'Content-Type: text/plain; charset=utf-8',
        `Content-Transfer-Encoding: ${sevenBit ? '7bit' : 'quoted-printable'}`,
        'Auto-Submitted: auto-generated',
    ];
    const raw = headers.join('\r\n') + '\r\n\r\n' + (sevenBit ? body : quotedPrintable(body));
    return { raw, from: f.address, to: t.address, messageId: id };
}
