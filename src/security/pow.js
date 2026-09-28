// Proof of work (docs/DESIGN.md section 8).
//
// An endpoint that wants one answers HTTP 428
//   { "error": "pow_required", "pow": { "challenge": "<opaque ASCII>", "bits": 18, "expiresAt": ms } }
// and the client repeats the request with "pow": { "challenge", "nonce" } in the JSON body, where
// nonce is a decimal ASCII string such that SHA-256(challenge + ":" + nonce) starts with `bits`
// zero bits (most significant bit of the first byte first).
//
// Challenge format: base64url(JSON payload) + "." + base64url(HMAC-SHA256(powKey, payloadPart)),
// payload = { v: 1, h: <keyed hash of the client network>, e: <endpoint>, b: <bits>, x: <expiry ms>,
// r: <random> }. The server keeps no state until the answer comes back; single use is enforced
// with the primary's `once.consume` (keyed by the signature) for the challenge's remaining life.

import crypto from 'node:crypto';
import { ipKey } from './ratelimit.js';

export const POW_TTL_MS = 2 * 60000;
const NONCE_RE = /^[0-9]{1,20}$/;
const CHALLENGE_RE = /^[A-Za-z0-9_-]{16,400}\.[A-Za-z0-9_-]{43}$/;

/**
 * Number of leading zero bits of a buffer.
 * @param {Buffer} buf
 * @returns {number}
 */
export function leadingZeroBits(buf) {
    let n = 0;
    for (let i = 0; i < buf.length; i++) {
        const b = buf[i];
        if (b === 0) { n += 8; continue; }
        return n + Math.clz32(b) - 24;
    }
    return n;
}

/**
 * True when SHA-256(challenge + ":" + nonce) has at least `bits` leading zero bits.
 * @param {string} challenge
 * @param {string} nonce
 * @param {number} bits
 * @returns {boolean}
 */
export function checkWork(challenge, nonce, bits) {
    const h = crypto.createHash('sha256').update(challenge + ':' + nonce, 'utf8').digest();
    return leadingZeroBits(h) >= bits;
}

/**
 * Finds a nonce for `challenge` (what the game client does; used by tests and tools).
 * @param {string} challenge
 * @param {number} bits
 * @returns {string}
 */
export function solvePow(challenge, bits) {
    for (let n = 0; ; n++) {
        const s = String(n);
        if (checkWork(challenge, s, bits)) return s;
    }
}

/**
 * Proof-of-work service.
 * @param {{ key: Buffer, ipKeyHash: Buffer, now?: () => number, ttlMs?: number,
 *           once: (key: string, ttlMs: number) => Promise<boolean> }} opts
 *   `once(key, ttl)` resolves true the first time a key is seen (primary `once.consume`).
 */
export function createPow({ key, ipKeyHash, now = Date.now, ttlMs = POW_TTL_MS, once }) {
    const netHash = (ip) => crypto.createHmac('sha256', ipKeyHash).update(ipKey(ip)).digest('base64url').slice(0, 16);
    const sign = (part) => crypto.createHmac('sha256', key).update(part, 'ascii').digest('base64url');

    /**
     * A new challenge bound to the client network and the endpoint.
     * @returns {{ challenge: string, bits: number, expiresAt: number }}
     */
    function issue({ ip, endpoint, bits }) {
        const expiresAt = now() + ttlMs;
        const payload = { v: 1, h: netHash(ip), e: endpoint, b: bits, x: expiresAt, r: crypto.randomBytes(12).toString('base64url') };
        const part = Buffer.from(JSON.stringify(payload), 'utf8').toString('base64url');
        return { challenge: `${part}.${sign(part)}`, bits, expiresAt };
    }

    /**
     * Checks an answer. `bits` is the difficulty required now (a challenge issued with fewer bits
     * is refused).
     * @returns {Promise<{ ok: true } | { ok: false, reason: 'malformed'|'signature'|'endpoint'|'network'|'expired'|'bits'|'work'|'replayed' }>}
     */
    async function verify({ ip, endpoint, bits, challenge, nonce }) {
        if (typeof challenge !== 'string' || !CHALLENGE_RE.test(challenge)) return { ok: false, reason: 'malformed' };
        if (typeof nonce !== 'string' || !NONCE_RE.test(nonce)) return { ok: false, reason: 'malformed' };
        const dot = challenge.indexOf('.');
        const part = challenge.slice(0, dot);
        const sig = Buffer.from(challenge.slice(dot + 1), 'base64url');
        const expect = Buffer.from(sign(part), 'base64url');
        if (sig.length !== expect.length || !crypto.timingSafeEqual(sig, expect)) return { ok: false, reason: 'signature' };
        let p;
        try { p = JSON.parse(Buffer.from(part, 'base64url').toString('utf8')); } catch { return { ok: false, reason: 'malformed' }; }
        if (!p || p.v !== 1) return { ok: false, reason: 'malformed' };
        if (p.e !== endpoint) return { ok: false, reason: 'endpoint' };
        if (p.h !== netHash(ip)) return { ok: false, reason: 'network' };
        const t = now();
        if (!(p.x > t) || p.x > t + ttlMs + 1000) return { ok: false, reason: 'expired' };
        if (!(p.b >= bits)) return { ok: false, reason: 'bits' };
        if (!checkWork(challenge, nonce, p.b)) return { ok: false, reason: 'work' };
        const fresh = await once('pow:' + challenge.slice(dot + 1), p.x - t + 1000);
        if (!fresh) return { ok: false, reason: 'replayed' };
        return { ok: true };
    }

    return { issue, verify };
}
