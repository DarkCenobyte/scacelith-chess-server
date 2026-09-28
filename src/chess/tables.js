// Internal to src/chess: constants, 0x88 geometry tables and Zobrist keys.
//
// Board representation: 0x88 mailbox (Uint8Array(128)); square s88 = rank * 16 + file, a square
// is on the board when (s88 & 0x88) === 0. The public API uses the protocol squares 0..63
// (a1 = 0, h8 = 63); S88[] and S64[] convert. Piece code = colour << 3 | type (white pawn 1 ..
// white king 6, black pawn 9 .. black king 14), 0 = empty.

export const WHITE = 0;
export const BLACK = 1;

export const PAWN = 1;
export const KNIGHT = 2;
export const BISHOP = 3;
export const ROOK = 4;
export const QUEEN = 5;
export const KING = 6;

// Move flags: the protocol's MoveFlag bits (src/protocol/schema.js).
export const F_CAPTURE = 1;
export const F_EP = 2;
export const F_CASTLE_K = 4;
export const F_CASTLE_Q = 8;
export const F_DOUBLE = 16;
export const F_PROMO = 32;
export const F_CHECK = 64;
export const F_MATE = 128;

// Castling rights bits (chess::CastlingRights).
export const CR_WK = 1;
export const CR_WQ = 2;
export const CR_BK = 4;
export const CR_BQ = 8;

/** 0..63 -> 0x88. */
export const S88 = new Uint8Array(64);
/** 0x88 -> 0..63 (only meaningful for on-board squares). */
export const S64 = new Uint8Array(128);
for (let s = 0; s < 64; s++) {
    S88[s] = (s >> 3) * 16 + (s & 7);
    S64[S88[s]] = s;
}

export const KNIGHT_STEPS = [33, 31, 18, 14, -14, -18, -31, -33];
export const KING_STEPS = [1, -1, 16, -16, 15, 17, -15, -17];
export const ORTH_STEPS = [1, -1, 16, -16];
export const DIAG_STEPS = [15, 17, -15, -17];

// Indexed by (a - b + 119) for two on-board 0x88 squares a, b:
//  DIR:  the step d such that b + n * d === a (n >= 1) along a queen line, 0 when not aligned;
//  STEP: bit 1 << KNIGHT when a - b is a knight jump, bit 1 << KING when it is a king step.
export const DIR = new Int8Array(240);
export const STEP = new Uint8Array(240);
for (const d of [...ORTH_STEPS, ...DIAG_STEPS]) {
    for (let n = 1; n < 8; n++) DIR[n * d + 119] = d;
}
for (const d of KNIGHT_STEPS) STEP[d + 119] |= 1 << KNIGHT;
for (const d of KING_STEPS) STEP[d + 119] |= 1 << KING;

// Castling rights kept when a move starts or ends on a square (indexed 0..63).
export const CASTLE_MASK = new Uint8Array(64).fill(15);
CASTLE_MASK[0] = 15 & ~CR_WQ;
CASTLE_MASK[4] = 15 & ~(CR_WK | CR_WQ);
CASTLE_MASK[7] = 15 & ~CR_WK;
CASTLE_MASK[56] = 15 & ~CR_BQ;
CASTLE_MASK[60] = 15 & ~(CR_BK | CR_BQ);
CASTLE_MASK[63] = 15 & ~CR_BK;

/** Square names "a1".."h8" (index 0..63). */
export const SQ_NAMES = [];
for (let s = 0; s < 64; s++) SQ_NAMES.push(String.fromCharCode(97 + (s & 7), 49 + (s >> 3)));

/** FEN letter of a piece code ('' for empty / invalid codes). */
export const PIECE_CHAR = ['', 'P', 'N', 'B', 'R', 'Q', 'K', '', '', 'p', 'n', 'b', 'r', 'q', 'k', ''];
/** ASCII code of PIECE_CHAR (digest computation without strings). */
export const PIECE_ASCII = new Uint8Array(16);
for (let c = 0; c < 16; c++) PIECE_ASCII[c] = PIECE_CHAR[c] ? PIECE_CHAR[c].charCodeAt(0) : 0;

// ---- Zobrist keys ---------------------------------------------------------------------------
// The same keys as the game's chess::bb::kZobrist (splitmix64 from seed 0x5CACE117C4E55, same
// generation order), so Position.repetitionKey() equals chess::Position::hash() in hex. Stored as
// two 32-bit halves (int32 values, XOR friendly). Piece keys are indexed by code * 64 + sq64.

export const ZP_LO = new Int32Array(16 * 64);
export const ZP_HI = new Int32Array(16 * 64);
export const ZC_LO = new Int32Array(16);
export const ZC_HI = new Int32Array(16);
export const ZEP_LO = new Int32Array(8);
export const ZEP_HI = new Int32Array(8);

const zobristSide = (() => {
    const M64 = (1n << 64n) - 1n;
    let state = 0x5CACE117C4E55n;
    const next = () => {
        state = (state + 0x9E3779B97F4A7C15n) & M64;
        let z = state;
        z = ((z ^ (z >> 30n)) * 0xBF58476D1CE4E5B9n) & M64;
        z = ((z ^ (z >> 27n)) * 0x94D049BB133111EBn) & M64;
        return z ^ (z >> 31n);
    };
    const lo = (v) => Number(BigInt.asIntN(32, v));
    const hi = (v) => Number(BigInt.asIntN(32, v >> 32n));
    for (let c = 0; c < 2; c++) {
        for (let t = 1; t < 7; t++) {
            for (let s = 0; s < 64; s++) {
                const v = next();
                const i = ((c << 3) | t) * 64 + s;
                ZP_LO[i] = lo(v);
                ZP_HI[i] = hi(v);
            }
        }
    }
    const rights = [next(), next(), next(), next()];
    for (let m = 0; m < 16; m++) {
        let v = 0n;
        for (let i = 0; i < 4; i++) if (m & (1 << i)) v ^= rights[i];
        ZC_LO[m] = lo(v);
        ZC_HI[m] = hi(v);
    }
    for (let f = 0; f < 8; f++) {
        const v = next();
        ZEP_LO[f] = lo(v);
        ZEP_HI[f] = hi(v);
    }
    const v = next();
    return [lo(v), hi(v)];
})();
export const ZSIDE_LO = zobristSide[0];
export const ZSIDE_HI = zobristSide[1];
