// Protocol entry point: the generated codec plus small hand-written helpers.
export * from './codec.gen.js';

// Move <-> u16 (from | to << 6 | promo << 12; promo: 0 none, 2 N, 3 B, 4 R, 5 Q).
export function encodeMove(from, to, promo = 0) { return (from & 63) | ((to & 63) << 6) | ((promo & 7) << 12); }
export function decodeMove(m) { return { from: m & 63, to: (m >> 6) & 63, promo: (m >> 12) & 7 }; }

// FNV-1a 32-bit of an ASCII string: it hashes the low byte of each UTF-16 code unit, so it matches
// the C++ hash of the UTF-8 bytes only for ASCII (FEN is ASCII). posHash is fnv1a32 of the first
// four FEN fields.
export function fnv1a32(s) {
    let h = 0x811c9dc5;
    for (let i = 0; i < s.length; i++) {
        h ^= s.charCodeAt(i) & 0xff;
        h = Math.imul(h, 0x01000193) >>> 0;
    }
    return h >>> 0;
}

/**
 * posHash of a position given as FEN: fnv1a32 of its first four fields joined by single spaces.
 * The FEN must be normalised like chess::Position::fen() (castling "KQkq" order or "-", en passant
 * square only when an en passant capture is legal); Position.fen() of src/chess does that.
 * @param {string} fen
 * @returns {number} u32
 */
export function fenDigest(fen) {
    return fnv1a32(String(fen).trim().split(/\s+/).slice(0, 4).join(' '));
}

const PROMO_CHARS = ['', '', 'n', 'b', 'r', 'q'];
const sqName = (s) => String.fromCharCode(97 + (s & 7), 49 + (s >> 3));

/**
 * UCI text of a protocol move ("e2e4", "e7e8q"). No legality check.
 * @param {number} m u16 move
 * @returns {string}
 */
export function moveToUci(m) {
    const { from, to, promo } = decodeMove(m);
    return sqName(from) + sqName(to) + (PROMO_CHARS[promo] || '');
}

/**
 * Protocol move of a UCI text ("e2e4", "e7e8q"), or -1 when the text is not a move. No legality check.
 * @param {string} s
 * @returns {number}
 */
export function uciToMove(s) {
    const m = /^([a-h])([1-8])([a-h])([1-8])([nbrq]?)$/.exec(String(s).trim().toLowerCase());
    if (!m) return -1;
    const sq = (f, r) => (f.charCodeAt(0) - 97) + 8 * (r.charCodeAt(0) - 49);
    return encodeMove(sq(m[1], m[2]), sq(m[3], m[4]), m[5] ? PROMO_CHARS.indexOf(m[5]) : 0);
}

/**
 * Type byte of an encoded message without decoding it, or -1 when the buffer is empty.
 * @param {Uint8Array} buf
 * @returns {number}
 */
export function peekType(buf) { return buf && buf.length > 0 ? buf[0] : -1; }

/** Reusable decode options (no allocation per call): decode(buf, DECODE_C2S) on the server. */
export const DECODE_C2S = Object.freeze({ dir: 'c2s' });
/** decode(buf, DECODE_S2C) on a client. */
export const DECODE_S2C = Object.freeze({ dir: 's2c' });
