// Protocol moves (u16: from | to << 6 | promo << 12) to UCI text, and game-record field
// normalisation (the store may hand back typed arrays, plain arrays or raw little-endian BLOBs).

const FILES = 'abcdefgh';
const PROMO = { 2: 'n', 3: 'b', 4: 'r', 5: 'q' };

/** Square index (a1 = 0, h8 = 63) to algebraic text. */
export function squareName(sq) {
    return FILES[sq & 7] + String((sq >> 3) + 1);
}

/**
 * u16 protocol move to UCI (castling is already the king's move, e1g1, as UCI wants it).
 * @param {number} m
 * @returns {string}
 */
export function moveToUci(m) {
    const from = m & 63, to = (m >> 6) & 63, promo = (m >> 12) & 7;
    return squareName(from) + squareName(to) + (PROMO[promo] || '');
}

/** UCI text to the u16 protocol move (-1 when malformed). */
export function uciToMove(s) {
    const m = /^([a-h])([1-8])([a-h])([1-8])([nbrq]?)$/.exec(String(s));
    if (!m) return -1;
    const sq = (f, r) => FILES.indexOf(f) + (Number(r) - 1) * 8;
    const promo = { '': 0, n: 2, b: 3, r: 4, q: 5 }[m[5]];
    return sq(m[1], m[2]) | (sq(m[3], m[4]) << 6) | (promo << 12);
}

function fromBytes(u8, width) {
    const n = Math.floor(u8.byteLength / width);
    const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
    const out = new Array(n);
    for (let i = 0; i < n; i++) out[i] = width === 2 ? dv.getUint16(i * 2, true) : dv.getUint32(i * 4, true);
    return out;
}

/**
 * Normalises a numeric column of a game record: Uint16Array/Uint32Array/Array as is, a raw
 * BLOB (Buffer/Uint8Array) decoded as little-endian integers of `width` bytes, a JSON text
 * parsed. Returns a plain array ([] when absent).
 * @param {*} v
 * @param {2|4} width
 * @returns {number[]}
 */
export function numberList(v, width) {
    if (v == null) return [];
    if (Array.isArray(v)) return v.map(Number);
    if (v instanceof Uint8Array) return fromBytes(v, width);
    if (ArrayBuffer.isView(v)) return Array.from(v);
    if (v instanceof ArrayBuffer) return fromBytes(new Uint8Array(v), width);
    if (typeof v === 'string') {
        try { const a = JSON.parse(v); return Array.isArray(a) ? a.map(Number) : []; } catch { return []; }
    }
    return [];
}
