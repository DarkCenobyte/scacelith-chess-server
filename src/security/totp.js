// TOTP (RFC 6238 over RFC 4226 HOTP: HMAC-SHA1, 6 digits, 30 s steps, +-1 step accepted),
// base32 secrets, otpauth:// URIs, AES-256-GCM encryption of the secrets at rest, and the
// single-use recovery codes (docs/DESIGN.md section 8).
//
// Replay protection is the caller's job: verifyTotp() returns the matched time step and the caller
// stores it atomically (store.users.advanceMfaStep), refusing any step not newer than the last one.

import crypto from 'node:crypto';

export const TOTP_DIGITS = 6;
export const TOTP_PERIOD_S = 30;
export const TOTP_SECRET_BYTES = 20;
export const RECOVERY_CODE_COUNT = 10;

const B32 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
// Crockford's alphabet in lower case: no i, l, o, u. 32 symbols -> 5 bits per character, 10
// characters -> 50 bits per recovery code.
const RC_ALPHABET = '0123456789abcdefghjkmnpqrstvwxyz';

/**
 * RFC 4648 base32 without padding.
 * @param {Buffer} buf
 * @returns {string}
 */
export function base32Encode(buf) {
    let out = '', bits = 0, value = 0;
    for (const byte of buf) {
        value = (value << 8) | byte;
        bits += 8;
        while (bits >= 5) { out += B32[(value >>> (bits - 5)) & 31]; bits -= 5; }
    }
    if (bits > 0) out += B32[(value << (5 - bits)) & 31];
    return out;
}

/**
 * Decodes base32 (case-insensitive, spaces and padding ignored); null when invalid.
 * @param {string} str
 * @returns {Buffer|null}
 */
export function base32Decode(str) {
    const s = String(str).toUpperCase().replace(/[\s=]/g, '');
    const out = [];
    let bits = 0, value = 0;
    for (const ch of s) {
        const v = B32.indexOf(ch);
        if (v < 0) return null;
        value = ((value << 5) | v) & 0xffff;
        bits += 5;
        if (bits >= 8) { out.push((value >>> (bits - 8)) & 0xff); bits -= 8; }
    }
    return Buffer.from(out);
}

/**
 * HOTP value (RFC 4226) of `counter`.
 * @param {Buffer} secret
 * @param {number} counter
 * @param {number} [digits=6]
 * @param {string} [algorithm='sha1']
 * @returns {string} zero-padded decimal code
 */
export function hotp(secret, counter, digits = TOTP_DIGITS, algorithm = 'sha1') {
    const msg = Buffer.alloc(8);
    msg.writeUInt32BE(Math.floor(counter / 0x100000000) >>> 0, 0);
    msg.writeUInt32BE(counter >>> 0, 4);
    const h = crypto.createHmac(algorithm, secret).update(msg).digest();
    const off = h[h.length - 1] & 0x0f;
    const bin = ((h[off] & 0x7f) << 24) | (h[off + 1] << 16) | (h[off + 2] << 8) | h[off + 3];
    return String(bin % 10 ** digits).padStart(digits, '0');
}

/**
 * Time step of `nowMs`.
 * @param {number} nowMs
 * @returns {number}
 */
export function totpStep(nowMs, period = TOTP_PERIOD_S) {
    return Math.floor(nowMs / 1000 / period);
}

/**
 * TOTP code at `nowMs`.
 * @param {Buffer} secret
 * @param {number} nowMs
 * @returns {string}
 */
export function totp(secret, nowMs, { digits = TOTP_DIGITS, period = TOTP_PERIOD_S, algorithm = 'sha1' } = {}) {
    return hotp(secret, totpStep(nowMs, period), digits, algorithm);
}

/**
 * Checks a code against the steps now-window..now+window, skipping steps not newer than
 * `lastStep` (already used). Every candidate is computed and compared in constant time.
 * @param {Buffer} secret
 * @param {string} code
 * @param {{ now: number, window?: number, lastStep?: number }} opts
 * @returns {number} the matched step, or -1
 */
export function verifyTotp(secret, code, { now, window = 1, lastStep = -1 }) {
    if (typeof code !== 'string' || !/^[0-9]{6}$/.test(code)) return -1;
    const cur = totpStep(now);
    const given = Buffer.from(code, 'ascii');
    let matched = -1;
    for (let d = -window; d <= window; d++) {
        const step = cur + d;
        const ok = crypto.timingSafeEqual(Buffer.from(hotp(secret, step), 'ascii'), given);
        if (ok && step > lastStep && matched < 0) matched = step;
    }
    return matched;
}

/** @returns {Buffer} a new 20-byte TOTP secret */
export function generateTotpSecret() {
    return crypto.randomBytes(TOTP_SECRET_BYTES);
}

/**
 * The otpauth:// URI authenticator apps read (usually from a QR code).
 * @param {{ issuer: string, account: string, secret: Buffer }} p
 * @returns {string}
 */
export function otpauthUri({ issuer, account, secret }) {
    const iss = encodeURIComponent(issuer);
    return `otpauth://totp/${iss}:${encodeURIComponent(account)}?secret=${base32Encode(secret)}&issuer=${iss}` +
        `&algorithm=SHA1&digits=${TOTP_DIGITS}&period=${TOTP_PERIOD_S}`;
}

/**
 * AES-256-GCM box for small secrets. The associated data binds a ciphertext to its owner (a
 * secret copied to another account does not decrypt).
 * Format: "v1." + base64url(iv 12 | ciphertext | tag 16).
 * @param {Buffer} key 32 bytes
 */
export function createSecretBox(key) {
    if (!Buffer.isBuffer(key) || key.length !== 32) throw new Error('secret box key must be 32 bytes');
    return {
        /** @param {Buffer} plain @param {string} aad @returns {string} */
        seal(plain, aad) {
            const iv = crypto.randomBytes(12);
            const c = crypto.createCipheriv('aes-256-gcm', key, iv);
            c.setAAD(Buffer.from(aad, 'utf8'));
            const ct = Buffer.concat([c.update(plain), c.final()]);
            return 'v1.' + Buffer.concat([iv, ct, c.getAuthTag()]).toString('base64url');
        },
        /** @param {string} sealed @param {string} aad @returns {Buffer|null} */
        open(sealed, aad) {
            if (typeof sealed !== 'string' || !sealed.startsWith('v1.')) return null;
            const raw = Buffer.from(sealed.slice(3), 'base64url');
            if (raw.length < 12 + 16 + 1) return null;
            try {
                const d = crypto.createDecipheriv('aes-256-gcm', key, raw.subarray(0, 12));
                d.setAAD(Buffer.from(aad, 'utf8'));
                d.setAuthTag(raw.subarray(raw.length - 16));
                return Buffer.concat([d.update(raw.subarray(12, raw.length - 16)), d.final()]);
            } catch {
                return null;
            }
        },
    };
}

/**
 * New recovery codes, formatted xxxx-xxxx-xx.
 * @param {number} [n=10]
 * @returns {string[]}
 */
export function generateRecoveryCodes(n = RECOVERY_CODE_COUNT) {
    const out = [];
    for (let i = 0; i < n; i++) {
        const b = crypto.randomBytes(10);
        let s = '';
        for (let j = 0; j < 10; j++) s += RC_ALPHABET[b[j] & 31];
        out.push(`${s.slice(0, 4)}-${s.slice(4, 8)}-${s.slice(8)}`);
    }
    return out;
}

/**
 * Canonical form of a typed recovery code (lower case, separators removed, o -> 0, i/l -> 1),
 * or null when it cannot be one.
 * @param {string} s
 * @returns {string|null}
 */
export function normalizeRecoveryCode(s) {
    if (typeof s !== 'string' || s.length > 32) return null;
    const c = s.toLowerCase().replace(/[\s-]/g, '').replace(/o/g, '0').replace(/[il]/g, '1');
    if (c.length !== 10) return null;
    for (const ch of c) if (!RC_ALPHABET.includes(ch)) return null;
    return c;
}

/**
 * Stored form of a recovery code: HMAC-SHA256 with the derived pepper, bound to the user.
 * @param {Buffer} pepper
 * @param {number|string} userId
 * @param {string} normalized output of normalizeRecoveryCode()
 * @returns {string} hex
 */
export function hashRecoveryCode(pepper, userId, normalized) {
    return crypto.createHmac('sha256', pepper).update(`${userId}:${normalized}`, 'utf8').digest('hex');
}

/** True when `s` has the shape of a TOTP code. */
export function isTotpCode(s) { return typeof s === 'string' && /^[0-9]{6}$/.test(s); }
