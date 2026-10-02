// Position: 0x88 board, legal move generation, single-move validation, FEN, SAN/UCI, perft.
// Mirrors chess::Position of the game (src/chess/position.cpp) move for move; see index.js.
//
// Moves are the protocol u16 (from | to << 6 | promo << 12). Internally a move is an int
// "im" = u16 | flags << 16 (flags = MoveFlag bits without Check/Mate).
//
// Hot path cost (per call, no allocation): isLegal() = one pseudo-legality test + one attack
// test; play() = validation + incremental make (Zobrist, counts, king squares) + a check test,
// plus one legal-move search when the move gives check (mate detection).

import {
    WHITE, BLACK, PAWN, KNIGHT, BISHOP, ROOK, QUEEN, KING,
    F_CAPTURE, F_EP, F_CASTLE_K, F_CASTLE_Q, F_DOUBLE, F_PROMO, F_CHECK, F_MATE,
    CR_WK, CR_WQ, CR_BK, CR_BQ,
    S88, S64, KNIGHT_STEPS, KING_STEPS, DIR, STEP, CASTLE_MASK, SQ_NAMES, PIECE_CHAR, PIECE_ASCII,
    ZP_LO, ZP_HI, ZC_LO, ZC_HI, ZEP_LO, ZEP_HI, ZSIDE_LO, ZSIDE_HI,
} from './tables.js';

const W_PAWN = PAWN, B_PAWN = 8 | PAWN;
const W_KNIGHT = KNIGHT, B_KNIGHT = 8 | KNIGHT;
const W_BISHOP = BISHOP, B_BISHOP = 8 | BISHOP;
const W_ROOK = ROOK, B_ROOK = 8 | ROOK;
const W_QUEEN = QUEEN, B_QUEEN = 8 | QUEEN;
const W_KING = KING, B_KING = 8 | KING;

const MAX_MOVES = 512;          // generation buffer (legal positions have at most 218 moves)
const UNDO_SLOTS = 6;           // captured, castling, ep, halfmove, hashLo, hashHi
const MAX_PERFT_DEPTH = 64;

// Scratch buffers shared by every Position of this isolate (JS is single-threaded; none of the
// methods using them re-enters another one while iterating a buffer).
const SCRATCH = new Int32Array(MAX_MOVES);
const PERFT_BUFS = [];
function perftBuf(ply) {
    while (PERFT_BUFS.length <= ply) PERFT_BUFS.push(new Int32Array(MAX_MOVES));
    return PERFT_BUFS[ply];
}

const PIECE_OF_CHAR = new Map([
    ['P', W_PAWN], ['N', W_KNIGHT], ['B', W_BISHOP], ['R', W_ROOK], ['Q', W_QUEEN], ['K', W_KING],
    ['p', B_PAWN], ['n', B_KNIGHT], ['b', B_BISHOP], ['r', B_ROOK], ['q', B_QUEEN], ['k', B_KING],
]);
const UPPER = ' PNBRQK';
const PROMO_UCI = ' pnbrqk';

/** chess::parseSquare: "e4" -> 28 (file letter may be upper case), -1 on error. */
function parseSquare(s) {
    if (s.length !== 2) return -1;
    let fc = s.charCodeAt(0);
    if (fc >= 65 && fc <= 72) fc += 32;
    const f = fc - 97, r = s.charCodeAt(1) - 49;
    if (f < 0 || f > 7 || r < 0 || r > 7) return -1;
    return r * 8 + f;
}

/** chess::pieceFromChar (either case) -> piece type, 0 when not a piece letter. */
function pieceTypeOfChar(c) {
    const code = PIECE_OF_CHAR.get(c);
    return code ? code & 7 : 0;
}

/** chess splitSpaces: fields separated by space, tab, CR or LF. */
function splitSpaces(s) {
    const out = [];
    let cur = '';
    for (let i = 0; i < s.length; i++) {
        const c = s[i];
        if (c === ' ' || c === '\t' || c === '\n' || c === '\r') {
            if (cur) {
                out.push(cur);
                cur = '';
            }
        } else {
            cur += c;
        }
    }
    if (cur) out.push(cur);
    return out;
}

/** chess parseInt: 1..9 decimal digits, no sign. Returns -1 on error. */
function parseCounter(s) {
    if (s.length === 0 || s.length > 9) return -1;
    let v = 0;
    for (let i = 0; i < s.length; i++) {
        const c = s.charCodeAt(i);
        if (c < 48 || c > 57) return -1;
        v = v * 10 + (c - 48);
    }
    return v;
}

function isMoveNumber(m) {
    return typeof m === 'number' && m >= 0 && m <= 0x7fff && (m | 0) === m;
}

/**
 * A chess position with the game's rules (FIDE), legal move generation and notation.
 * Construct with Position.start() or Position.fromFEN(fen).
 */
export class Position {
    constructor() {
        this._b = new Uint8Array(128);   // 0x88 board of piece codes
        this._k = new Uint8Array(2);     // king squares (0x88)
        this._cnt = new Uint8Array(16);  // piece counts by code
        this._side = WHITE;
        this._cr = 0;                    // castling rights (CR_* bits)
        this._ep = -1;                   // 0x88 ep square, only when a legal ep capture exists
        this._hm = 0;
        this._fm = 1;
        this._lo = 0;                    // Zobrist halves (int32)
        this._hi = 0;
        this._dg = -1;                   // cached digest, -1 = stale
        this._u = null;                  // undo stack (lazy, grows with make/unmake nesting)
        this._sp = 0;
    }

    /** @returns {Position} the standard starting position. */
    static start() {
        return START.clone();
    }

    /**
     * Parses a FEN like chess::Position::setFEN (halfmove/fullmove optional). Castling rights
     * whose king or rook is not on its initial square are dropped; the en passant square is kept
     * only when an en passant capture is legal.
     * @param {string} fen
     * @returns {Position|null} null on malformed or impossible input (missing/extra kings, pawns
     *   on the back ranks, side not to move in check, more than 16 pieces or 8 pawns per side).
     */
    static fromFEN(fen) {
        if (typeof fen !== 'string') return null;
        const f = splitSpaces(fen);
        if (f.length < 4 || f.length > 6) return null;
        const p = new Position();
        const b = p._b;
        let rank = 7, file = 0;
        const placement = f[0];
        for (let i = 0; i < placement.length; i++) {
            const c = placement[i];
            if (c === '/') {
                if (file !== 8 || rank === 0) return null;
                --rank;
                file = 0;
            } else if (c >= '1' && c <= '8') {
                file += c.charCodeAt(0) - 48;
                if (file > 8) return null;
            } else {
                const code = PIECE_OF_CHAR.get(c);
                if (!code || file > 7) return null;
                const s = rank * 16 + file;
                b[s] = code;
                p._cnt[code]++;
                if ((code & 7) === KING) p._k[code >> 3] = s;
                ++file;
            }
        }
        if (rank !== 0 || file !== 8) return null;
        const cnt = p._cnt;
        for (let c = 0; c < 2; c++) {
            const base = c << 3;
            if (cnt[base | KING] !== 1) return null;
            let total = 0;
            for (let t = 1; t <= 6; t++) total += cnt[base | t];
            if (total > 16 || cnt[base | PAWN] > 8) return null;
        }
        for (let fl = 0; fl < 8; fl++) {
            if ((b[fl] & 7) === PAWN || (b[0x70 + fl] & 7) === PAWN) return null;
        }

        if (f[1] === 'w') p._side = WHITE;
        else if (f[1] === 'b') p._side = BLACK;
        else return null;

        let rights = 0;
        if (f[2] !== '-') {
            for (const c of f[2]) {
                if (c === 'K') rights |= CR_WK;
                else if (c === 'Q') rights |= CR_WQ;
                else if (c === 'k') rights |= CR_BK;
                else if (c === 'q') rights |= CR_BQ;
                else return null;
            }
        }
        // Keep only rights whose king and rook stand on their initial squares.
        if (b[4] !== W_KING) rights &= ~(CR_WK | CR_WQ);
        if (b[0x74] !== B_KING) rights &= ~(CR_BK | CR_BQ);
        if (b[7] !== W_ROOK) rights &= ~CR_WK;
        if (b[0] !== W_ROOK) rights &= ~CR_WQ;
        if (b[0x77] !== B_ROOK) rights &= ~CR_BK;
        if (b[0x70] !== B_ROOK) rights &= ~CR_BQ;
        p._cr = rights;

        if (f.length >= 5) {
            p._hm = parseCounter(f[4]);
            if (p._hm < 0) return null;
        }
        if (f.length >= 6) {
            p._fm = parseCounter(f[5]);
            if (p._fm < 0) return null;
        }
        if (p._fm < 1) p._fm = 1;

        // The side that just moved cannot be in check.
        const them = p._side ^ 1;
        if (p._attacked(p._k[them], p._side)) return null;

        p._ep = -1;
        p._rehash();
        if (f[3] !== '-') {
            const e64 = parseSquare(f[3]);
            if (e64 < 0) return null;
            const e = S88[e64];
            const up = p._side === WHITE ? 16 : -16;
            if ((e >> 4) === (p._side === WHITE ? 5 : 2) && b[e - up] === ((them << 3) | PAWN) && b[e] === 0 && b[e + up] === 0) {
                p._setEp(e);
            }
        }
        return p;
    }

    /** @returns {Position} an independent copy. */
    clone() {
        const p = new Position();
        p._b.set(this._b);
        p._k.set(this._k);
        p._cnt.set(this._cnt);
        p._side = this._side;
        p._cr = this._cr;
        p._ep = this._ep;
        p._hm = this._hm;
        p._fm = this._fm;
        p._lo = this._lo;
        p._hi = this._hi;
        p._dg = this._dg;
        return p;
    }

    /** Side to move: 0 White, 1 Black. */
    get side() { return this._side; }
    /** Castling rights bits: 1 White O-O, 2 White O-O-O, 4 Black O-O, 8 Black O-O-O. */
    get castling() { return this._cr; }
    /** En passant square 0..63 (only when a legal en passant capture exists), -1 otherwise. */
    get epSquare() { return this._ep < 0 ? -1 : S64[this._ep]; }
    get halfmove() { return this._hm; }
    get fullmove() { return this._fm; }

    /**
     * Piece on a square (0..63): 0 when empty, else type | colour << 3 (type 1 pawn .. 6 king).
     * @param {number} sq
     */
    pieceAt(sq) { return this._b[S88[sq & 63]]; }

    /** @param {number} color @returns {number} square 0..63 of that king. */
    kingSquare(color) { return S64[this._k[color & 1]]; }

    /** @returns {boolean} the side to move is in check. */
    inCheck() {
        const us = this._side;
        return this._attacked(this._k[us], us ^ 1);
    }

    /**
     * @param {number} sq 0..63
     * @param {number} by colour of the attackers
     * @returns {boolean}
     */
    isAttacked(sq, by) { return this._attacked(S88[sq & 63], by & 1); }

    /** @returns {number[]} the legal moves (u16, promotions expanded), in generation order. */
    legalMoves() {
        const n = this._generate(SCRATCH, false);
        const out = new Array(n);
        for (let i = 0; i < n; i++) out[i] = SCRATCH[i] & 0xffff;
        return out;
    }

    /**
     * @param {number} move u16
     * @returns {boolean} true when the move (exact u16, promotion piece included, 0 otherwise) is
     *   legal here. Any other value (bit 15, wrong promotion bits, non-integers...) is false.
     */
    isLegal(move) { return this._validate(move) >= 0; }

    /**
     * Applies a legal move.
     * @param {number} move u16
     * @returns {number} MoveFlag bits (Capture, EnPassant, CastleKing, CastleQueen, DoublePush,
     *   Promotion, Check, Mate).
     * @throws {Error} when the move is not legal (the position is unchanged).
     */
    play(move) {
        const im = this._validate(move);
        if (im < 0) throw new Error(`illegal move ${move} in ${this.fen()}`);
        return this._play(im);
    }

    /** @returns {boolean} the side to move has at least one legal move. */
    hasLegalMove() { return this._generate(SCRATCH, true) > 0; }
    isCheckmate() { return this.inCheck() && !this.hasLegalMove(); }
    isStalemate() { return !this.inCheck() && !this.hasLegalMove(); }

    /** Dead position: K v K, K+B v K, K+N v K, or only bishops all on squares of one colour. */
    hasInsufficientMaterial() {
        const c = this._cnt;
        if (c[W_PAWN] | c[B_PAWN] | c[W_ROOK] | c[B_ROOK] | c[W_QUEEN] | c[B_QUEEN]) return false;
        const knights = c[W_KNIGHT] + c[B_KNIGHT], bishops = c[W_BISHOP] + c[B_BISHOP];
        if (bishops === 0) return knights <= 1;
        if (knights) return false;
        let light = false, dark = false;
        const b = this._b;
        for (let s = 0; s < 120; s++) {
            if (s & 0x88) {
                s += 7;
                continue;
            }
            if ((b[s] & 7) === BISHOP) {
                if (((s >> 4) + (s & 7)) & 1) light = true;
                else dark = true;
            }
        }
        return !(light && dark);
    }

    /**
     * chess::Position::canColorMate (FIDE 6.9 approximation): false when `color` has a bare
     * king, when it has a single minor piece and the opponent a bare king, or when the position
     * is dead; true otherwise.
     * @param {number} color
     */
    canColorMate(color) {
        if (this.hasInsufficientMaterial()) return false;
        const c = this._cnt, mine = (color & 1) << 3, theirs = mine ^ 8;
        let n = 0, t = 0;
        for (let k = 1; k <= 5; k++) {
            n += c[mine | k];
            t += c[theirs | k];
        }
        if (n === 0) return false;
        if (n === 1 && (c[mine | KNIGHT] + c[mine | BISHOP]) === 1 && t === 0) return false;
        return true;
    }

    /** @returns {string} the FEN, exactly as chess::Position::fen() prints it. */
    fen() {
        return `${this._fenPrefix()} ${this._hm} ${this._fm}`;
    }

    /**
     * posHash: FNV-1a 32 of the first four FEN fields ("placement side castling ep").
     * @returns {number} u32
     */
    digest() {
        if (this._dg >= 0) return this._dg;
        const b = this._b;
        let h = 0x811c9dc5 | 0;
        for (let r = 7; r >= 0; r--) {
            let empty = 0;
            for (let f = 0; f < 8; f++) {
                const p = b[r * 16 + f];
                if (!p) {
                    ++empty;
                    continue;
                }
                if (empty) {
                    h = Math.imul(h ^ (48 + empty), 0x01000193);
                    empty = 0;
                }
                h = Math.imul(h ^ PIECE_ASCII[p], 0x01000193);
            }
            if (empty) h = Math.imul(h ^ (48 + empty), 0x01000193);
            if (r) h = Math.imul(h ^ 47, 0x01000193);
        }
        h = Math.imul(h ^ 32, 0x01000193);
        h = Math.imul(h ^ (this._side ? 98 : 119), 0x01000193);
        h = Math.imul(h ^ 32, 0x01000193);
        const cr = this._cr;
        if (!cr) h = Math.imul(h ^ 45, 0x01000193);
        if (cr & CR_WK) h = Math.imul(h ^ 75, 0x01000193);
        if (cr & CR_WQ) h = Math.imul(h ^ 81, 0x01000193);
        if (cr & CR_BK) h = Math.imul(h ^ 107, 0x01000193);
        if (cr & CR_BQ) h = Math.imul(h ^ 113, 0x01000193);
        h = Math.imul(h ^ 32, 0x01000193);
        if (this._ep < 0) {
            h = Math.imul(h ^ 45, 0x01000193);
        } else {
            h = Math.imul(h ^ (97 + (this._ep & 7)), 0x01000193);
            h = Math.imul(h ^ (49 + (this._ep >> 4)), 0x01000193);
        }
        this._dg = h >>> 0;
        return this._dg;
    }

    /**
     * Repetition identity (FIDE 9.2.3: placement, side to move, castling rights, en passant
     * capture possible): the 64-bit Zobrist key as 16 hex digits, equal to chess::Position::hash().
     * @returns {string}
     */
    repetitionKey() {
        return (this._hi >>> 0).toString(16).padStart(8, '0') + (this._lo >>> 0).toString(16).padStart(8, '0');
    }

    /** Low / high 32 bits of the repetition key (no allocation). */
    get keyLo() { return this._lo >>> 0; }
    get keyHi() { return this._hi >>> 0; }

    /**
     * Standard Algebraic Notation of a legal move ("Nbd7", "exd8=Q+", "O-O#"), "" if illegal.
     * @param {number} move u16
     */
    san(move) {
        const im = this._validate(move);
        if (im < 0) return '';
        const flags = im >>> 16, from64 = im & 63, to64 = (im >> 6) & 63;
        const t = this._b[S88[from64]] & 7;
        let s;
        if (flags & F_CASTLE_K) {
            s = 'O-O';
        } else if (flags & F_CASTLE_Q) {
            s = 'O-O-O';
        } else if (t === PAWN) {
            s = (flags & F_CAPTURE) ? SQ_NAMES[from64][0] + 'x' + SQ_NAMES[to64] : SQ_NAMES[to64];
            if (flags & F_PROMO) s += '=' + UPPER[(im >> 12) & 7];
        } else {
            s = UPPER[t];
            // Only the other pieces of the same code can make the move ambiguous: validate theirs
            // instead of generating every legal move.
            const piece = this._b[S88[from64]];
            let ambiguous = false, sameFile = false, sameRank = false;
            for (let of = 0; of < 64; of++) {
                if (of === from64 || this._b[S88[of]] !== piece || this._validate(of | (to64 << 6)) < 0) continue;
                ambiguous = true;
                if ((of & 7) === (from64 & 7)) sameFile = true;
                if ((of >> 3) === (from64 >> 3)) sameRank = true;
            }
            if (ambiguous) {
                if (!sameFile) s += SQ_NAMES[from64][0];
                else if (!sameRank) s += SQ_NAMES[from64][1];
                else s += SQ_NAMES[from64];
            }
            if (flags & F_CAPTURE) s += 'x';
            s += SQ_NAMES[to64];
        }
        this._make(im);
        const side = this._side;
        if (this._attacked(this._k[side], side ^ 1)) s += this._generate(SCRATCH, true) ? '+' : '#';
        this._unmake(im);
        return s;
    }

    /**
     * UCI text of a move ("e7e8q"); "0000" for a value that is not a u16 move. Legality is not
     * checked (like chess::Position::toUCI).
     * @param {number} move
     */
    uci(move) {
        if (!isMoveNumber(move)) return '0000';
        const promo = move >> 12;
        return SQ_NAMES[move & 63] + SQ_NAMES[(move >> 6) & 63] + (promo >= 1 && promo <= 6 ? PROMO_UCI[promo] : '');
    }

    /**
     * Parses UCI ("e2e4", "e7e8q") like chess::Position::parseUCI.
     * @param {string} str
     * @returns {number} the legal u16 move, or -1.
     */
    parseUCI(str) {
        if (typeof str !== 'string' || (str.length !== 4 && str.length !== 5)) return -1;
        const from = parseSquare(str.slice(0, 2)), to = parseSquare(str.slice(2, 4));
        if (from < 0 || to < 0) return -1;
        let promo = 0;
        if (str.length === 5) {
            promo = pieceTypeOfChar(str[4]);
            if (promo < KNIGHT || promo > QUEEN) return -1;
        }
        const m = from | (to << 6) | (promo << 12);
        return this._validate(m) >= 0 ? m : -1;
    }

    /**
     * Number of leaf nodes of the legal move tree (perft).
     * @param {number} depth
     */
    perft(depth) {
        if (depth <= 0) return 1;
        if (depth > MAX_PERFT_DEPTH) throw new RangeError('perft depth too large');
        return this._perft(depth, 0);
    }

    // ---- internals ----------------------------------------------------------------------------

    _fenPrefix() {
        const b = this._b;
        let s = '';
        for (let r = 7; r >= 0; r--) {
            let empty = 0;
            for (let f = 0; f < 8; f++) {
                const p = b[r * 16 + f];
                if (!p) {
                    ++empty;
                    continue;
                }
                if (empty) {
                    s += empty;
                    empty = 0;
                }
                s += PIECE_CHAR[p];
            }
            if (empty) s += empty;
            if (r) s += '/';
        }
        s += this._side ? ' b ' : ' w ';
        const cr = this._cr;
        if (!cr) s += '-';
        if (cr & CR_WK) s += 'K';
        if (cr & CR_WQ) s += 'Q';
        if (cr & CR_BK) s += 'k';
        if (cr & CR_BQ) s += 'q';
        s += ' ';
        s += this._ep < 0 ? '-' : SQ_NAMES[S64[this._ep]];
        return s;
    }

    _rehash() {
        let lo = 0, hi = 0;
        const b = this._b;
        for (let s = 0; s < 120; s++) {
            if (s & 0x88) {
                s += 7;
                continue;
            }
            const p = b[s];
            if (p) {
                const z = p * 64 + S64[s];
                lo ^= ZP_LO[z];
                hi ^= ZP_HI[z];
            }
        }
        lo ^= ZC_LO[this._cr];
        hi ^= ZC_HI[this._cr];
        if (this._ep >= 0) {
            lo ^= ZEP_LO[this._ep & 7];
            hi ^= ZEP_HI[this._ep & 7];
        }
        if (this._side === BLACK) {
            lo ^= ZSIDE_LO;
            hi ^= ZSIDE_HI;
        }
        this._lo = lo;
        this._hi = hi;
        this._dg = -1;
    }

    /** Is 0x88 square s attacked by colour `by` on the current board? */
    _attacked(s, by) {
        const b = this._b;
        let a;
        if (by === WHITE) {
            a = s - 15;
            if (!(a & 0x88) && b[a] === W_PAWN) return true;
            a = s - 17;
            if (!(a & 0x88) && b[a] === W_PAWN) return true;
        } else {
            a = s + 15;
            if (!(a & 0x88) && b[a] === B_PAWN) return true;
            a = s + 17;
            if (!(a & 0x88) && b[a] === B_PAWN) return true;
        }
        const bb = by << 3;
        const knight = bb | KNIGHT, king = bb | KING;
        for (let i = 0; i < 8; i++) {
            a = s + KNIGHT_STEPS[i];
            if (!(a & 0x88) && b[a] === knight) return true;
            a = s + KING_STEPS[i];
            if (!(a & 0x88) && b[a] === king) return true;
        }
        const rook = bb | ROOK, bishop = bb | BISHOP, queen = bb | QUEEN;
        // Orthogonal rays.
        for (a = s + 1; !(a & 0x88); a += 1) {
            const p = b[a];
            if (p) {
                if (p === rook || p === queen) return true;
                break;
            }
        }
        for (a = s - 1; !(a & 0x88); a -= 1) {
            const p = b[a];
            if (p) {
                if (p === rook || p === queen) return true;
                break;
            }
        }
        for (a = s + 16; !(a & 0x88); a += 16) {
            const p = b[a];
            if (p) {
                if (p === rook || p === queen) return true;
                break;
            }
        }
        for (a = s - 16; !(a & 0x88); a -= 16) {
            const p = b[a];
            if (p) {
                if (p === rook || p === queen) return true;
                break;
            }
        }
        // Diagonal rays.
        for (a = s + 15; !(a & 0x88); a += 15) {
            const p = b[a];
            if (p) {
                if (p === bishop || p === queen) return true;
                break;
            }
        }
        for (a = s + 17; !(a & 0x88); a += 17) {
            const p = b[a];
            if (p) {
                if (p === bishop || p === queen) return true;
                break;
            }
        }
        for (a = s - 15; !(a & 0x88); a -= 15) {
            const p = b[a];
            if (p) {
                if (p === bishop || p === queen) return true;
                break;
            }
        }
        for (a = s - 17; !(a & 0x88); a -= 17) {
            const p = b[a];
            if (p) {
                if (p === bishop || p === queen) return true;
                break;
            }
        }
        return false;
    }

    /**
     * Does the pseudo-legal (non-castling) move from -> to (0x88) leave the mover's king safe?
     * Plays it on the board only (no state change), tests, restores.
     */
    _safeAfter(from, to, flags) {
        const b = this._b, us = this._side;
        const piece = b[from], cap = b[to];
        b[to] = piece;
        b[from] = 0;
        let epSq = -1, epPiece = 0;
        if (flags & F_EP) {
            epSq = us === WHITE ? to - 16 : to + 16;
            epPiece = b[epSq];
            b[epSq] = 0;
        }
        const ksq = (piece & 7) === KING ? to : this._k[us];
        const safe = !this._attacked(ksq, us ^ 1);
        b[from] = piece;
        b[to] = cap;
        if (epSq >= 0) b[epSq] = epPiece;
        return safe;
    }

    /** Legality filter used by the generator (castling moves are generated fully checked). */
    _legalPseudo(from, to, flags, check, ksq) {
        if (from === ksq || check || (flags & F_EP)) return this._safeAfter(from, to, flags);
        // Not in check: a piece that is not on a line with its king, or that stays on the same
        // ray from the king, cannot expose the king.
        const d = DIR[ksq - from + 119];
        if (d === 0 || DIR[ksq - to + 119] === d) return true;
        return this._safeAfter(from, to, flags);
    }

    /**
     * Generates the legal moves (im ints) into buf. firstOnly: stop after the first legal move.
     * @returns {number} count
     */
    _generate(buf, firstOnly) {
        const b = this._b, us = this._side, them = us ^ 1;
        const ksq = this._k[us];
        const check = this._attacked(ksq, them);
        const own = us << 3;
        let n = 0;
        const up = us === WHITE ? 16 : -16;
        const startRank = us === WHITE ? 1 : 6, lastRank = us === WHITE ? 7 : 0;
        const ep = this._ep;
        for (let s = 0; s < 120; s++) {
            if (s & 0x88) {
                s += 7;
                continue;
            }
            const p = b[s];
            if (p === 0 || (p & 8) !== own) continue;
            const t = p & 7;
            const f64 = S64[s];
            if (t === PAWN) {
                const to = s + up;
                if (b[to] === 0) {
                    if ((to >> 4) === lastRank) {
                        if (this._legalPseudo(s, to, F_PROMO, check, ksq)) {
                            const base = f64 | (S64[to] << 6) | (F_PROMO << 16);
                            buf[n++] = base | (QUEEN << 12);
                            buf[n++] = base | (ROOK << 12);
                            buf[n++] = base | (BISHOP << 12);
                            buf[n++] = base | (KNIGHT << 12);
                        }
                    } else {
                        if (this._legalPseudo(s, to, 0, check, ksq)) buf[n++] = f64 | (S64[to] << 6);
                        const to2 = to + up;
                        if ((s >> 4) === startRank && b[to2] === 0 && this._legalPseudo(s, to2, F_DOUBLE, check, ksq)) {
                            buf[n++] = f64 | (S64[to2] << 6) | (F_DOUBLE << 16);
                        }
                    }
                }
                for (let side = -1; side <= 1; side += 2) {
                    const to = s + up + side;
                    if (to & 0x88) continue;
                    const c = b[to];
                    if (c !== 0 && (c & 8) !== own) {
                        if ((to >> 4) === lastRank) {
                            if (this._legalPseudo(s, to, F_PROMO | F_CAPTURE, check, ksq)) {
                                const base = f64 | (S64[to] << 6) | ((F_PROMO | F_CAPTURE) << 16);
                                buf[n++] = base | (QUEEN << 12);
                                buf[n++] = base | (ROOK << 12);
                                buf[n++] = base | (BISHOP << 12);
                                buf[n++] = base | (KNIGHT << 12);
                            }
                        } else if (this._legalPseudo(s, to, F_CAPTURE, check, ksq)) {
                            buf[n++] = f64 | (S64[to] << 6) | (F_CAPTURE << 16);
                        }
                    } else if (to === ep && this._safeAfter(s, to, F_EP)) {
                        buf[n++] = f64 | (S64[to] << 6) | ((F_CAPTURE | F_EP) << 16);
                    }
                }
            } else if (t === KNIGHT || t === KING) {
                const steps = t === KNIGHT ? KNIGHT_STEPS : KING_STEPS;
                for (let i = 0; i < 8; i++) {
                    const to = s + steps[i];
                    if (to & 0x88) continue;
                    const c = b[to];
                    if (c !== 0 && (c & 8) === own) continue;
                    const fl = c ? F_CAPTURE : 0;
                    if (this._legalPseudo(s, to, fl, check, ksq)) buf[n++] = f64 | (S64[to] << 6) | (fl << 16);
                }
            } else {
                const d0 = t === ROOK ? 4 : 0, d1 = t === BISHOP ? 4 : 8;
                for (let i = d0; i < d1; i++) {
                    const d = QUEEN_STEPS[i];
                    for (let to = s + d; !(to & 0x88); to += d) {
                        const c = b[to];
                        if (c === 0) {
                            if (this._legalPseudo(s, to, 0, check, ksq)) buf[n++] = f64 | (S64[to] << 6);
                            continue;
                        }
                        if ((c & 8) !== own && this._legalPseudo(s, to, F_CAPTURE, check, ksq)) {
                            buf[n++] = f64 | (S64[to] << 6) | (F_CAPTURE << 16);
                        }
                        break;
                    }
                }
            }
            if (firstOnly && n) return n;
        }
        // Castling (fully checked: rights, empty squares, not out of / through / into check).
        const ks = us === WHITE ? CR_WK : CR_BK, qs = us === WHITE ? CR_WQ : CR_BQ;
        if ((this._cr & (ks | qs)) && !check) {
            const k = us === WHITE ? 0x04 : 0x74, k64 = S64[k];
            if ((this._cr & ks) && b[k + 1] === 0 && b[k + 2] === 0 && !this._attacked(k + 1, them) && !this._attacked(k + 2, them)) {
                buf[n++] = k64 | ((k64 + 2) << 6) | (F_CASTLE_K << 16);
            }
            if ((this._cr & qs) && b[k - 1] === 0 && b[k - 2] === 0 && b[k - 3] === 0 && !this._attacked(k - 1, them) && !this._attacked(k - 2, them)) {
                buf[n++] = k64 | ((k64 - 2) << 6) | (F_CASTLE_Q << 16);
            }
        }
        return n;
    }

    /**
     * Validates one u16 move without generating the move list.
     * @returns {number} the internal move (u16 | flags << 16) when legal, -1 otherwise.
     */
    _validate(m) {
        if (!isMoveNumber(m)) return -1;
        const from64 = m & 63, to64 = (m >> 6) & 63, promo = m >> 12;
        if (from64 === to64) return -1;
        const from = S88[from64], to = S88[to64];
        const b = this._b, us = this._side;
        const p = b[from];
        if (p === 0 || (p >> 3) !== us) return -1;
        const cap = b[to];
        if (cap !== 0 && (cap >> 3) === us) return -1;
        const t = p & 7;
        let flags = cap ? F_CAPTURE : 0;
        const delta = to - from;
        if (t === PAWN) {
            const up = us === WHITE ? 16 : -16;
            if (delta === up) {
                if (cap) return -1;
            } else if (delta === up + up) {
                if (cap || b[from + up] !== 0 || (from >> 4) !== (us === WHITE ? 1 : 6)) return -1;
                flags |= F_DOUBLE;
            } else if (delta === up - 1 || delta === up + 1) {
                if (!cap) {
                    if (to !== this._ep) return -1;
                    flags |= F_CAPTURE | F_EP;
                }
            } else {
                return -1;
            }
            if ((to >> 4) === (us === WHITE ? 7 : 0)) {
                if (promo < KNIGHT || promo > QUEEN) return -1;
                flags |= F_PROMO;
            } else if (promo) {
                return -1;
            }
        } else {
            if (promo) return -1;
            if (t === KNIGHT) {
                if (!(STEP[delta + 119] & (1 << KNIGHT))) return -1;
            } else if (t === KING) {
                if (!(STEP[delta + 119] & (1 << KING))) {
                    // Castling: the king's move by two files on its home square.
                    const home = us === WHITE ? 0x04 : 0x74;
                    if (from !== home || (delta !== 2 && delta !== -2)) return -1;
                    const them = us ^ 1;
                    if (delta === 2) {
                        if (!(this._cr & (us === WHITE ? CR_WK : CR_BK))) return -1;
                        if (b[from + 1] !== 0 || b[from + 2] !== 0) return -1;
                        if (this._attacked(from, them) || this._attacked(from + 1, them) || this._attacked(from + 2, them)) return -1;
                        return m | (F_CASTLE_K << 16);
                    }
                    if (!(this._cr & (us === WHITE ? CR_WQ : CR_BQ))) return -1;
                    if (b[from - 1] !== 0 || b[from - 2] !== 0 || b[from - 3] !== 0) return -1;
                    if (this._attacked(from, them) || this._attacked(from - 1, them) || this._attacked(from - 2, them)) return -1;
                    return m | (F_CASTLE_Q << 16);
                }
            } else {
                const d = DIR[delta + 119];
                if (d === 0) return -1;
                const diag = d === 15 || d === 17 || d === -15 || d === -17;
                if (t === ROOK ? diag : (t === BISHOP && !diag)) return -1;
                for (let s = from + d; s !== to; s += d) if (b[s] !== 0) return -1;
            }
        }
        if (!this._safeAfter(from, to, flags)) return -1;
        return m | (flags << 16);
    }

    /** Sets the ep square (0x88) when an en passant capture onto it is legal (hash updated). */
    _setEp(e) {
        if (this._ep >= 0) {
            this._lo ^= ZEP_LO[this._ep & 7];
            this._hi ^= ZEP_HI[this._ep & 7];
        }
        this._ep = -1;
        const b = this._b, us = this._side;
        const pawn = (us << 3) | PAWN;
        const c1 = us === WHITE ? e - 15 : e + 15, c2 = us === WHITE ? e - 17 : e + 17;
        if ((!(c1 & 0x88) && b[c1] === pawn && this._safeAfter(c1, e, F_EP)) ||
            (!(c2 & 0x88) && b[c2] === pawn && this._safeAfter(c2, e, F_EP))) {
            this._ep = e;
            this._lo ^= ZEP_LO[e & 7];
            this._hi ^= ZEP_HI[e & 7];
        }
        this._dg = -1;
    }

    /** Makes a legal internal move, pushing its undo record. */
    _make(im) {
        const from64 = im & 63, to64 = (im >> 6) & 63, flags = im >>> 16;
        const from = S88[from64], to = S88[to64];
        const b = this._b, cnt = this._cnt, us = this._side;
        const piece = b[from], cap = b[to];
        let lo = this._lo, hi = this._hi;
        const o = this._sp * UNDO_SLOTS;
        let u = this._u;
        if (u === null || o + UNDO_SLOTS > u.length) {
            // play() keeps one record, perft and san() nest a few: start small, double.
            const grown = new Int32Array(u === null ? 2 * UNDO_SLOTS : u.length * 2);
            if (u !== null) grown.set(u);
            this._u = u = grown;
        }
        u[o] = cap;
        u[o + 1] = this._cr;
        u[o + 2] = this._ep;
        u[o + 3] = this._hm;
        u[o + 4] = lo;
        u[o + 5] = hi;
        this._sp++;

        if (this._ep >= 0) {
            lo ^= ZEP_LO[this._ep & 7];
            hi ^= ZEP_HI[this._ep & 7];
            this._ep = -1;
        }
        lo ^= ZC_LO[this._cr];
        hi ^= ZC_HI[this._cr];
        let z;
        if (cap) {
            z = cap * 64 + to64;
            lo ^= ZP_LO[z];
            hi ^= ZP_HI[z];
            cnt[cap]--;
        }
        if (flags & F_EP) {
            const cs = us === WHITE ? to - 16 : to + 16;
            const cp = b[cs];
            b[cs] = 0;
            z = cp * 64 + S64[cs];
            lo ^= ZP_LO[z];
            hi ^= ZP_HI[z];
            cnt[cp]--;
        }
        z = piece * 64 + from64;
        lo ^= ZP_LO[z];
        hi ^= ZP_HI[z];
        b[from] = 0;
        let placed = piece;
        if (flags & F_PROMO) {
            placed = (us << 3) | ((im >> 12) & 7);
            cnt[piece]--;
            cnt[placed]++;
        }
        b[to] = placed;
        z = placed * 64 + to64;
        lo ^= ZP_LO[z];
        hi ^= ZP_HI[z];
        if ((piece & 7) === KING) {
            this._k[us] = to;
            if (flags & (F_CASTLE_K | F_CASTLE_Q)) {
                const rf = (flags & F_CASTLE_K) ? to + 1 : to - 2, rt = (flags & F_CASTLE_K) ? to - 1 : to + 1;
                const rook = b[rf];
                b[rf] = 0;
                b[rt] = rook;
                z = rook * 64;
                lo ^= ZP_LO[z + S64[rf]] ^ ZP_LO[z + S64[rt]];
                hi ^= ZP_HI[z + S64[rf]] ^ ZP_HI[z + S64[rt]];
            }
        }
        this._cr &= CASTLE_MASK[from64] & CASTLE_MASK[to64];
        lo ^= ZC_LO[this._cr];
        hi ^= ZC_HI[this._cr];
        this._hm = ((piece & 7) === PAWN || cap) ? 0 : this._hm + 1;
        if (us === BLACK) this._fm++;
        this._side = us ^ 1;
        this._lo = lo ^ ZSIDE_LO;
        this._hi = hi ^ ZSIDE_HI;
        this._dg = -1;
        if (flags & F_DOUBLE) this._setEp((from + to) >> 1);
    }

    /** Undoes the last _make(im). */
    _unmake(im) {
        const from64 = im & 63, to64 = (im >> 6) & 63, flags = im >>> 16;
        const from = S88[from64], to = S88[to64];
        const b = this._b, cnt = this._cnt;
        const us = this._side ^ 1;
        this._sp--;
        const u = this._u, o = this._sp * UNDO_SLOTS;
        const cap = u[o];
        this._cr = u[o + 1];
        this._ep = u[o + 2];
        this._hm = u[o + 3];
        this._lo = u[o + 4];
        this._hi = u[o + 5];
        this._side = us;
        if (us === BLACK) this._fm--;
        this._dg = -1;
        let piece = b[to];
        if (flags & F_PROMO) {
            cnt[piece]--;
            piece = (us << 3) | PAWN;
            cnt[piece]++;
        }
        b[from] = piece;
        b[to] = cap;
        if (cap) cnt[cap]++;
        if (flags & F_EP) {
            const cp = ((us ^ 1) << 3) | PAWN;
            b[us === WHITE ? to - 16 : to + 16] = cp;
            cnt[cp]++;
        }
        if ((piece & 7) === KING) {
            this._k[us] = from;
            if (flags & (F_CASTLE_K | F_CASTLE_Q)) {
                const rf = (flags & F_CASTLE_K) ? to + 1 : to - 2, rt = (flags & F_CASTLE_K) ? to - 1 : to + 1;
                b[rf] = b[rt];
                b[rt] = 0;
            }
        }
    }

    /** Plays a validated internal move for good; returns the MoveFlag bits incl. Check/Mate. */
    _play(im) {
        this._make(im);
        this._sp = 0;
        let flags = im >>> 16;
        const side = this._side;
        if (this._attacked(this._k[side], side ^ 1)) {
            flags |= F_CHECK;
            if (this._generate(SCRATCH, true) === 0) flags |= F_MATE;
        }
        return flags;
    }

    _perft(depth, ply) {
        const buf = perftBuf(ply);
        const n = this._generate(buf, false);
        if (depth === 1) return n;
        let total = 0;
        for (let i = 0; i < n; i++) {
            const im = buf[i];
            this._make(im);
            total += this._perft(depth - 1, ply + 1);
            this._unmake(im);
        }
        return total;
    }
}

const QUEEN_STEPS = [15, 17, -15, -17, 1, -1, 16, -16];   // diagonals then orthogonals

const START = Position.fromFEN('rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1');

export { parseSquare as _parseSquare };
