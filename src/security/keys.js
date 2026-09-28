// Keys derived from the server's master secret (HKDF-SHA256, docs/DESIGN.md section 8).
//
// Every purpose gets its own key, so that a key used to sign proof-of-work challenges can never be
// confused with the pepper of the recovery codes or the MFA encryption key. Changing SERVER_SECRET
// invalidates outstanding challenges and recovery codes and makes the TOTP secrets unreadable,
// unless MFA_ENCRYPTION_KEY is set (it then encrypts the TOTP secrets on its own).

import crypto from 'node:crypto';

/**
 * HKDF-SHA256 of `secret` for one purpose.
 * @param {Buffer} secret input keying material
 * @param {string} info purpose label, e.g. 'scacelith/mfa'
 * @param {number} [length=32] bytes
 * @returns {Buffer}
 */
export function deriveKey(secret, info, length = 32) {
    return Buffer.from(crypto.hkdfSync('sha256', secret, Buffer.alloc(0), Buffer.from(info, 'utf8'), length));
}

/**
 * The keys of the auth module, derived once at start-up.
 * @param {{ serverSecret: Buffer, mfaEncryptionKey?: Buffer|null }} config
 * @returns {{ pow: Buffer, recovery: Buffer, mfa: Buffer, ipHash: Buffer, mailThrottle: Buffer }}
 */
export function deriveAuthKeys(config) {
    const master = config.serverSecret;
    if (!Buffer.isBuffer(master) || master.length < 32) throw new Error('SERVER_SECRET must hold at least 32 bytes');
    let mfa;
    const mek = config.mfaEncryptionKey;
    if (Buffer.isBuffer(mek) && mek.length > 0) mfa = mek.length === 32 ? Buffer.from(mek) : deriveKey(mek, 'scacelith/mfa');
    else mfa = deriveKey(master, 'scacelith/mfa');
    return Object.freeze({
        pow: deriveKey(master, 'scacelith/pow'),
        recovery: deriveKey(master, 'scacelith/recovery-codes'),
        mfa,
        ipHash: deriveKey(master, 'scacelith/ip-hash'),
        mailThrottle: deriveKey(master, 'scacelith/mail-throttle'),
    });
}

/**
 * SHA-256 of a string as lower-case hex (the stored form of every token).
 * @param {string} s
 * @returns {string}
 */
export function sha256Hex(s) {
    return crypto.createHash('sha256').update(s, 'utf8').digest('hex');
}

/**
 * Constant-time comparison of two strings or buffers (false when the lengths differ).
 * @param {string|Buffer} a
 * @param {string|Buffer} b
 * @returns {boolean}
 */
export function safeEqual(a, b) {
    const x = Buffer.isBuffer(a) ? a : Buffer.from(String(a), 'utf8');
    const y = Buffer.isBuffer(b) ? b : Buffer.from(String(b), 'utf8');
    if (x.length !== y.length) {
        // Still spend the time of a comparison so that the length is the only thing revealed.
        crypto.timingSafeEqual(x, x);
        return false;
    }
    return crypto.timingSafeEqual(x, y);
}

/**
 * A random token: `prefix` + base64url of `bytes` random bytes (32 bytes -> 43 characters).
 * @param {string} [prefix='']
 * @param {number} [bytes=32]
 * @returns {string}
 */
export function randomToken(prefix = '', bytes = 32) {
    return prefix + crypto.randomBytes(bytes).toString('base64url');
}
