// PGN reader of the dedicated server: the FIRST game of a PGN text, as protocol u16 moves, for the
// animated GIF of any game (POST /api/v1/gif, src/gif/). Untrusted input: hard caps on bytes,
// plies, tags, tag lengths and variation nesting, and readPgn() never throws anything but a
// PgnError { line, column, message } (1-based, columns in characters).
//
//   const { tags, startFen, moves, result } = readPgn(text, { maxBytes: 256 << 10, maxPlies: 1200 });
//   // tags: [[name, value], ...] in file order; startFen: null for the standard start position,
//   // else the normalised FEN; moves: u16 (from | to << 6 | promo << 12); result: '1-0' | '0-1' |
//   // '1/2-1/2' | '*' (the movetext's termination, else the Result tag, else '*').
//
// It reads what the game's own reader reads (src/chess/pgn.cpp, same lexing and the same lenient
// SAN as chess::Position::parseSAN), so every PGN the server writes (GET /api/v1/games/:id/pgn,
// ChessGame.pgn) and the usual lichess / chess.com exports are accepted:
//  * tag pairs with \" and \\ escapes (and an unescaped quote inside a value when a later quote on
//    the line closes the tag); [SetUp] / [FEN] start positions (SetUp "0" ignores the FEN);
//    [Variant] standard / chess / normal / from position, or Chess960 with a FEN whose castling
//    rights all survive on the standard squares; any other variant is refused;
//  * movetext: move numbers ("12.", "12...", "12…"), comments { ... } and ; to the end of the
//    line, '%' escape lines, NAGs ($1), suffix glyphs (! ? !! ?? !? ?!), text evaluations
//    (+- = -/+ ±), variations ( ... ) skipped (nesting capped), "e.p." dropped;
//  * SAN read leniently: check / mate / annotation suffixes stripped, castling as O-O, 0-0, o-o,
//    OO (and the long forms), promotions as e8=Q, e8Q, e8(Q), e8/Q, captures with x, X, : or
//    none, long algebraic (Ng1-f3, e2e4) and over-disambiguated moves (Nge2 when only one knight
//    can go), a lower-case piece letter when it is not a pawn move (nf3, but bc4 is a b-pawn
//    capture first), figurine SAN (♘f3), and plain UCI (e7e8q) as a last resort; a move must
//    match exactly one legal move;
//  * the end of the first game: its termination marker, a tag pair after its movetext (the next
//    game), or the end of the text.
// Null moves ("--", "Z0") are refused. Moves after an automatic ending (fivefold repetition,
// 75 moves) are kept, as on paper: only legality is checked.

import { Position } from './position.js';

/** Default caps (the caller usually passes smaller maxBytes / maxPlies). */
export const PGN_LIMITS = Object.freeze({
    maxBytes: 1 << 20,
    maxPlies: 1500,
    maxTags: 128,
    maxTagName: 64,
    maxTagValue: 2048,
    maxDepth: 64,
    maxToken: 40,
});

/** The only error readPgn() throws. */
export class PgnError extends Error {
    /**
     * @param {number} line 1-based
     * @param {number} column 1-based (characters)
     * @param {string} message
     */
    constructor(line, column, message) {
        super(message);
        this.name = 'PgnError';
        this.line = line;
        this.column = column;
    }
}

const START_FEN = 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1';

const isSpace = (c) => c === ' ' || c === '\t' || c === '\r' || c === '\n' || c === '\f' || c === '\v';
const isAlpha = (c) => (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z');
const isDigit = (c) => c >= '0' && c <= '9';
const isNameChar = (c) => isAlpha(c) || isDigit(c) || c === '_';

/** Result text in the movetext or the Result tag, normalised ('' when it is not a result). */
export function normalizeResult(s) {
    if (s === '1-0' || s === '0-1' || s === '1/2-1/2' || s === '*') return s;
    if (s === '½-½' || s === '0.5-0.5' || s === '1/2') return '1/2-1/2';
    return '';
}

// ---- Lexer -------------------------------------------------------------------------------------

const T_END = 0, T_TAG = 1, T_COMMENT = 2, T_OPEN = 3, T_CLOSE = 4, T_NAG = 5, T_SYMBOL = 6, T_STAR = 7, T_BAD = 8;

class Lexer {
    constructor(s, lim) {
        this.s = s;
        this.lim = lim;
        this.p = s.charCodeAt(0) === 0xfeff ? 1 : 0;
        this.line = 1;
        this.col = 1;
        // The last quote closing a tag on the stretch [lcStart, lcEnd) of a line (tagPair), found
        // once: many tag pairs on one long line cost one pass over it, not one pass per tag.
        this.lcStart = 0;
        this.lcEnd = 0;
        this.lcLast = -1;
    }

    // A line ends at '\n', and at a '\r' not followed by '\n'.
    lineBreak(q) {
        const c = this.s[q];
        return c === '\n' || (c === '\r' && this.s[q + 1] !== '\n');
    }

    advance() {
        const s = this.s;
        const newLine = this.lineBreak(this.p);
        const code = s.charCodeAt(this.p++);
        if (newLine) {
            this.line++;
            this.col = 1;
        } else if (code < 0xdc00 || code > 0xdfff) {
            this.col++;     // a low surrogate does not start a character
        }
    }

    skipLine() {
        while (this.p < this.s.length && !this.lineBreak(this.p)) this.advance();
    }

    skipSpacesInLine() {
        while (this.p < this.s.length && (this.s[this.p] === ' ' || this.s[this.p] === '\t')) this.advance();
    }

    // At the start of a line: does it open a tag pair? (A comment running into it was never closed.)
    tagLineAhead() {
        const s = this.s;
        let q = this.p;
        while (q < s.length && (s[q] === ' ' || s[q] === '\t')) q++;
        if (s[q] !== '[') return false;
        q++;
        let n = 0;
        while (q < s.length && isNameChar(s[q])) q++, n++;
        while (q < s.length && (s[q] === ' ' || s[q] === '\t')) q++;
        return n > 0 && s[q] === '"';
    }

    // After a quote at q: only spaces then ']' on this line.
    closesTag(q) {
        const s = this.s;
        q++;
        while (q < s.length && (s[q] === ' ' || s[q] === '\t')) q++;
        return s[q] === ']';
    }

    next() {
        const s = this.s;
        for (;;) {
            while (this.p < s.length && isSpace(s[this.p])) this.advance();
            const t = { kind: T_END, text: '', value: '', line: this.line, column: this.col, tagLike: false };
            if (this.p >= s.length) return t;
            const c = s[this.p];
            if (c === '%' && this.col === 1) {
                this.skipLine();
                continue;
            }
            if (c === '<') {
                let q = this.p + 1;
                while (q < s.length && s[q] !== '>' && !this.lineBreak(q)) q++;
                if (s[q] === '>') {
                    while (this.p <= q) this.advance();
                    continue;
                }
                this.advance();
                return bad(t, "'<' without '>' on its line");
            }
            if (c === '.' || c === '…') {
                this.advance();
                continue;
            }
            switch (c) {
            case '[': return this.tagPair(t);
            case '{': return this.braceComment(t);
            case ';':
                this.skipLine();
                t.kind = T_COMMENT;
                return t;
            case '(':
                this.advance();
                t.kind = T_OPEN;
                return t;
            case ')':
                this.advance();
                t.kind = T_CLOSE;
                return t;
            case '*':
                this.advance();
                t.kind = T_STAR;
                return t;
            case '$': return this.nag(t);
            case '!':
            case '?':
                while (this.p < s.length && (s[this.p] === '!' || s[this.p] === '?')) this.advance();
                t.kind = T_NAG;
                return t;
            default:
                break;
            }
            if (c === '+' || c === '=' || (c === '-' && s[this.p + 1] !== '-')) {
                // Text evaluations of some exports ("+-", "=", "-/+"): ignored.
                while (this.p < s.length && '+-=/'.includes(s[this.p])) this.advance();
                continue;
            }
            const code = s.charCodeAt(this.p);
            if (isAlpha(c) || isDigit(c) || c === '-' || code >= 0x80) {
                if (this.symbol(t)) return t;
                continue;   // a lone "e.p."
            }
            this.advance();
            return bad(t, code < 0x20 || code === 0x7f ? 'unexpected control character' : `unexpected character '${c}'`);
        }
    }

    tagPair(t) {
        const s = this.s, lim = this.lim;
        this.advance();     // '['
        this.skipSpacesInLine();
        let name = '';
        while (this.p < s.length && isNameChar(s[this.p])) {
            name += s[this.p];
            this.advance();
            if (name.length > lim.maxTagName) {
                this.skipLine();
                t.tagLike = true;
                return bad(t, 'tag name too long');
            }
        }
        t.tagLike = true;
        if (!name) {
            this.skipLine();
            return bad(t, "tag name expected after '['");
        }
        this.skipSpacesInLine();
        if (s[this.p] !== '"') {
            this.skipLine();
            return bad(t, `quoted value expected in tag ${name}`);
        }
        this.advance();
        // The last closing quote from here to the end of the line. A later tag of a line already
        // scanned reuses that scan: the line's last closing quote is also the last one after this.p
        // when it lies after it, and one at or before this.p counts as none below.
        if (!(this.p >= this.lcStart && this.p < this.lcEnd)) {
            let q = this.p, last = -1;
            for (; q < s.length && !this.lineBreak(q); q++) if (s[q] === '"' && this.closesTag(q)) last = q;
            this.lcStart = this.p;
            this.lcEnd = q;
            this.lcLast = last;
        }
        const lastClose = this.lcLast;
        let value = '';
        let tooLong = false;
        for (;;) {
            if (this.p >= s.length || this.lineBreak(this.p)) return bad(t, `unterminated value of tag ${name}`);
            let ch = s[this.p];
            if (ch === '\\' && (s[this.p + 1] === '"' || s[this.p + 1] === '\\')) {
                this.advance();
                ch = s[this.p];
            } else if (ch === '"') {
                if (lastClose < 0 || lastClose <= this.p || this.closesTag(this.p)) {
                    this.advance();
                    break;
                }
            }
            if (value.length < lim.maxTagValue) value += ch;
            else tooLong = true;
            this.advance();
        }
        this.skipSpacesInLine();
        if (s[this.p] !== ']') {
            this.skipLine();
            return bad(t, `']' expected after the value of tag ${name}`);
        }
        this.advance();
        if (tooLong) return bad(t, `value of tag ${name} too long`);
        t.kind = T_TAG;
        t.text = name;
        t.value = value;
        return t;
    }

    braceComment(t) {
        const s = this.s;
        this.advance();     // '{'
        for (;;) {
            if (this.p >= s.length) return bad(t, 'unterminated comment');
            if (s[this.p] === '}') {
                this.advance();
                break;
            }
            const newLine = this.lineBreak(this.p);
            this.advance();
            if (newLine && this.tagLineAhead()) return bad(t, 'unterminated comment');
        }
        t.kind = T_COMMENT;
        return t;
    }

    nag(t) {
        const s = this.s;
        this.advance();     // '$'
        let n = 0, digits = 0;
        while (this.p < s.length && isDigit(s[this.p])) {
            if (digits < 4) n = n * 10 + (s.charCodeAt(this.p) - 48);
            digits++;
            this.advance();
        }
        if (digits === 0 || n > 255) return bad(t, 'malformed NAG');
        t.kind = T_NAG;
        return t;
    }

    symbol(t) {
        const s = this.s;
        let sym = '';
        let tooLong = false;
        while (this.p < s.length) {
            if (s.startsWith('e.p.', this.p)) {
                for (let k = 0; k < 4; k++) this.advance();
                continue;
            }
            const c = s[this.p];
            const code = s.charCodeAt(this.p);
            if (!(isAlpha(c) || isDigit(c) || code >= 0x80 || '_+#=:-/'.includes(c))) break;
            if (sym.length < this.lim.maxToken) sym += c;
            else tooLong = true;
            this.advance();
        }
        if (!sym) return false;
        if (tooLong) {
            bad(t, 'token too long');
            return true;
        }
        t.kind = T_SYMBOL;
        t.text = sym;
        return true;
    }
}

function bad(t, why) {
    t.kind = T_BAD;
    t.text = why;
    return t;
}

// ---- SAN -------------------------------------------------------------------------------------

// Figurines to letters (the pawn figurines disappear).
const FIGURINES = { '♔': 'K', '♕': 'Q', '♖': 'R', '♗': 'B', '♘': 'N', '♙': '',
    '♚': 'K', '♛': 'Q', '♜': 'R', '♝': 'B', '♞': 'N', '♟': '' };

const PIECE_OF_LETTER = { P: 1, N: 2, B: 3, R: 4, Q: 5, K: 6, p: 1, n: 2, b: 3, r: 4, q: 5, k: 6 };

function squareOf(s) {
    if (s.length !== 2) return -1;
    let f = s.charCodeAt(0);
    if (f >= 65 && f <= 72) f += 32;
    f -= 97;
    const r = s.charCodeAt(1) - 49;
    return f >= 0 && f <= 7 && r >= 0 && r <= 7 ? r * 8 + f : -1;
}

// chess::Position::parseSANStrict: the one legal move the text names, or -1.
function parseSanStrict(pos, s, legal) {
    if (!s) return -1;
    const c = s.replace(/[0o]/g, 'O');
    const kingSide = c === 'O-O' || c === 'OO', queenSide = c === 'O-O-O' || c === 'OOO';
    if (kingSide || queenSide) {
        for (const m of legal) {
            const from = m & 63, to = (m >> 6) & 63;
            if ((pos.pieceAt(from) & 7) === 6 && Math.abs((to & 7) - (from & 7)) === 2 && ((to & 7) > (from & 7)) === kingSide) return m;
        }
        return -1;
    }
    let piece = 1;
    let body = s;
    if ('NBRQK'.includes(body[0])) {
        piece = PIECE_OF_LETTER[body[0]];
        body = body.slice(1);
    }
    let promo = 0;
    if (piece === 1) {
        if (body.endsWith(')')) body = body.slice(0, -1);
        if (body.length >= 3) {
            const pt = PIECE_OF_LETTER[body[body.length - 1]] ?? 0;
            const prev = body[body.length - 2];
            if (pt >= 2 && pt <= 5 && ((prev >= '1' && prev <= '8') || prev === '=' || prev === '(' || prev === '/')) {
                promo = pt;
                body = body.slice(0, -1);
                while (body && '=(/'.includes(body[body.length - 1])) body = body.slice(0, -1);
            }
        }
    }
    let core = '';
    for (const ch of body) if (ch !== 'x' && ch !== 'X' && ch !== ':' && ch !== '-') core += ch;
    if (core.length < 2 || core.length > 4) return -1;
    const to = squareOf(core.slice(-2));
    if (to < 0) return -1;
    let dFile = -1, dRank = -1;
    for (let i = 0; i + 2 < core.length; i++) {
        const ch = core[i];
        if (ch >= 'a' && ch <= 'h' && dFile < 0) dFile = ch.charCodeAt(0) - 97;
        else if (ch >= '1' && ch <= '8' && dRank < 0) dRank = ch.charCodeAt(0) - 49;
        else return -1;
    }
    let found = -1, count = 0;
    for (const m of legal) {
        const from = m & 63;
        if (((m >> 6) & 63) !== to || (pos.pieceAt(from) & 7) !== piece) continue;
        if (dFile >= 0 && (from & 7) !== dFile) continue;
        if (dRank >= 0 && (from >> 3) !== dRank) continue;
        if ((m >> 12) !== promo) continue;
        found = m;
        count++;
    }
    return count === 1 ? found : -1;
}

/**
 * Reads one move in lenient SAN (chess::Position::parseSAN).
 * @param {Position} pos
 * @param {string} text
 * @returns {number} the u16 legal move, or -1.
 */
export function parseSan(pos, text) {
    let s = '';
    for (const ch of String(text)) {
        if (Object.hasOwn(FIGURINES, ch)) s += FIGURINES[ch];
        else if (!isSpace(ch)) s += ch;
    }
    const num = /^\d+\.+/.exec(s);
    if (num) s = s.slice(num[0].length);
    for (let again = true; again && s;) {
        again = false;
        if ('+#!?'.includes(s[s.length - 1])) {
            s = s.slice(0, -1);
            again = true;
        } else if (s.length > 4 && s.endsWith('e.p.')) {
            s = s.slice(0, -4);
            again = true;
        }
    }
    if (!s) return -1;
    const legal = pos.legalMoves();
    let m = parseSanStrict(pos, s, legal);
    if (m >= 0) return m;
    if ('nbrqk'.includes(s[0])) {
        m = parseSanStrict(pos, s[0].toUpperCase() + s.slice(1), legal);
        if (m >= 0) return m;
    }
    return pos.parseUCI(s.toLowerCase());
}

// ---- Reader ------------------------------------------------------------------------------------

function chess960Variant(v) {
    return v === 'chess960' || v === 'chess 960' || v === 'fischerandom' || v === 'fischer random' || v === '960';
}

// The start position from the tags (game reader's startPosition): [Position, fen|null].
function startPosition(tags, tagAt) {
    const index = (name) => tags.findIndex(([n]) => n === name);
    const vi = index('Variant'), fi = index('FEN'), si = index('SetUp');
    let c960 = false;
    if (vi >= 0) {
        const v = tags[vi][1].trim().toLowerCase();
        if (chess960Variant(v)) c960 = true;
        else if (v && v !== 'standard' && v !== 'chess' && v !== 'normal' && v !== 'from position') {
            throw new PgnError(tagAt[vi].line, tagAt[vi].column, `variant '${tags[vi][1]}' is not supported`);
        }
    }
    const useFen = fi >= 0 && !(si >= 0 && tags[si][1].trim() === '0');
    if (!useFen) {
        if (c960) throw new PgnError(tagAt[vi].line, tagAt[vi].column, 'Chess960 game without a FEN tag');
        return [Position.start(), null];
    }
    const fen = tags[fi][1].trim();
    const p = Position.fromFEN(fen);
    if (!p) throw new PgnError(tagAt[fi].line, tagAt[fi].column, c960 ? 'Chess960 castling rights are not supported' : 'invalid FEN');
    if (c960) {
        const fields = fen.split(/\s+/);
        let asked = 0;
        for (const ch of fields[2] ?? '') asked |= ch === 'K' ? 1 : ch === 'Q' ? 2 : ch === 'k' ? 4 : ch === 'q' ? 8 : 0;
        if (asked !== p.castling) throw new PgnError(tagAt[fi].line, tagAt[fi].column, 'Chess960 castling from this setup is not supported');
    }
    const normal = p.fen();
    return [p, normal === START_FEN ? null : normal];
}

/**
 * Reads the first game of a PGN text.
 * @param {string|Uint8Array} input the text (bytes are read as UTF-8, or Latin-1 when not UTF-8)
 * @param {{ maxBytes?: number, maxPlies?: number, maxTags?: number, maxTagName?: number,
 *   maxTagValue?: number, maxDepth?: number }} [limits] see PGN_LIMITS
 * @returns {{ tags: Array<[string, string]>, startFen: string|null, moves: number[], result: string }}
 * @throws {PgnError}
 */
export function readPgn(input, limits = {}) {
    try {
        return read(input, { ...PGN_LIMITS, ...Object.fromEntries(Object.entries(limits).filter(([, v]) => v !== undefined)) });
    } catch (e) {
        if (e instanceof PgnError) throw e;
        throw new PgnError(1, 1, `unreadable PGN (${e && e.message ? e.message : e})`);
    }
}

function read(input, lim) {
    let text;
    if (typeof input === 'string') {
        if (input.length > lim.maxBytes || Buffer.byteLength(input, 'utf8') > lim.maxBytes) {
            throw new PgnError(1, 1, `PGN too large (more than ${lim.maxBytes} bytes)`);
        }
        text = input;
    } else if (input instanceof Uint8Array) {
        if (input.length > lim.maxBytes) throw new PgnError(1, 1, `PGN too large (more than ${lim.maxBytes} bytes)`);
        try {
            text = new TextDecoder('utf-8', { fatal: true }).decode(input);
        } catch {
            text = Buffer.from(input.buffer, input.byteOffset, input.length).toString('latin1');
        }
    } else {
        throw new PgnError(1, 1, 'PGN text expected');
    }

    const lex = new Lexer(text, lim);
    const tags = [], tagAt = [];
    const moves = [];
    let pos = null, startFen = null;
    let depth = 0;
    let movetextResult = '';
    let any = false;
    let lastOpen = null;
    const begin = () => {
        if (pos) return;
        [pos, startFen] = startPosition(tags, tagAt);
    };
    for (;;) {
        const t = lex.next();
        if (t.kind === T_END) break;
        if (t.kind === T_TAG) {
            if (pos) break;     // the next game (this one had no termination marker)
            any = true;
            if (tags.length >= lim.maxTags) throw new PgnError(t.line, t.column, `too many tags (more than ${lim.maxTags})`);
            tags.push([t.text, t.value]);
            tagAt.push({ line: t.line, column: t.column });
            continue;
        }
        if (t.kind === T_BAD) {
            if (t.tagLike && pos) break;    // a broken tag pair opens the next game
            throw new PgnError(t.line, t.column, t.text);
        }
        if (t.kind === T_COMMENT) continue;
        any = true;
        begin();
        if (t.kind === T_OPEN) {
            if (++depth > lim.maxDepth) throw new PgnError(t.line, t.column, 'variations nested too deeply');
            lastOpen = t;
            continue;
        }
        if (t.kind === T_CLOSE) {
            if (depth === 0) throw new PgnError(t.line, t.column, "')' without a variation");
            depth--;
            continue;
        }
        if (t.kind === T_NAG) continue;
        const r = t.kind === T_STAR ? '*' : normalizeResult(t.text);
        if (r) {
            if (depth > 0) throw new PgnError(t.line, t.column, 'unterminated variation before the result');
            movetextResult = r;
            break;
        }
        if (depth > 0) continue;
        const sym = t.text;
        if (/^\d+$/.test(sym)) continue;    // move number
        if (sym === '--' || sym === 'Z0') throw new PgnError(t.line, t.column, 'null moves are not supported');
        if (!/[A-Za-z0-9]/.test(sym)) continue;     // an annotation glyph ("±", "∞")
        if (moves.length >= lim.maxPlies) throw new PgnError(t.line, t.column, `too many moves (more than ${lim.maxPlies})`);
        const m = parseSan(pos, sym);
        if (m < 0) throw new PgnError(t.line, t.column, `illegal move '${sym}'`);
        pos.play(m);
        moves.push(m);
    }
    if (!any) throw new PgnError(1, 1, 'no game found');
    if (depth > 0) throw new PgnError(lastOpen.line, lastOpen.column, 'unterminated variation');
    begin();    // a game of tags only: its FEN and Variant are checked too
    const tagResult = normalizeResult((tags.find(([n]) => n === 'Result') ?? ['', ''])[1].trim());
    return { tags, startFen, moves, result: movetextResult || tagResult || '*' };
}
