// Protocol entry point: the generated codec plus small hand-written helpers.
export * from './codec.gen.js';

// Move <-> u16 (from | to << 6 | promo << 12; promo: 0 none, 2 N, 3 B, 4 R, 5 Q).
export function encodeMove(from, to, promo = 0) { return (from & 63) | ((to & 63) << 6) | ((promo & 7) << 12); }
export function decodeMove(m) { return { from: m & 63, to: (m >> 6) & 63, promo: (m >> 12) & 7 }; }

// FNV-1a 32-bit of an ASCII/UTF-8 string (posHash is fnv1a32 of the first four FEN fields).
export function fnv1a32(s) {
    let h = 0x811c9dc5;
    for (let i = 0; i < s.length; i++) {
        h ^= s.charCodeAt(i) & 0xff;
        h = Math.imul(h, 0x01000193) >>> 0;
    }
    return h >>> 0;
}
