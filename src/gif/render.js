// Animated GIF of a chess game: a 2D board seen from above, the game played move by move.
//
//   const gif = renderGame({
//       startFen: null,                        // null / undefined: the standard start position
//       moves: [1804, 2356, ...],              // protocol u16 moves (from | to << 6 | promo << 12)
//       white: { name: 'alice', rating: 1520 }, black: { name: 'bob', rating: 1497 },
//       result: '1-0',                         // '1-0' | '0-1' | '1/2-1/2' | '*'
//       footer: 'Checkmate',                   // optional: how the game ended (shown with the result)
//       options: { size: 'medium', orientation: 'white', delayMs: 500, coords: true },
//   });                                        // -> Buffer (image/gif)
//
// Picture, top to bottom:
//  * a header band, one row per player, White first: a colour swatch, the name (bold), the
//    rating (dimmed; omitted when null); the side to move has an accent bar at the start of its
//    row and an accent dot at its end; on the last frame of a finished game each row ends with
//    the player's score (1, 0 or the one-half glyph) instead;
//  * the board: cburnett pieces (src/gif/pieces.js) on warm wood squares, the last move
//    highlighted on both its squares, a red glow under a king in check, the coordinates around
//    it (a-h below, 1-8 on the left, following the orientation; options.coords false drops them
//    and narrows the margins);
//  * a footer: the last move ("12..." dimmed, then the SAN in bold); on the last frame of a
//    finished game (or when `footer` is given) the result in the accent colour, how the game
//    ended (`footer`, else what the final position shows: checkmate, stalemate, insufficient
//    material), and the last move on the right.
// Names and texts keep the characters the font has (printable ASCII and a few signs, others print
// as '?') and are cut with an ellipsis when too long; unknown sizes and orientations fall back to
// medium and white.
//
// Frames: the start position (held max(1 s, delay)), one frame per move (delayMs, default 500,
// clamped to 100..3000), the last one held 3 s; the animation loops forever. Frames after the
// first only hold the bounding box of the pixels that changed, the unchanged ones transparent
// (colour index 0); only the rectangles redrawn for the frame (changed squares, header rows,
// footer) are compared. A move costs about 1.3 KiB (small), 2.6 KiB (medium), 4 KiB (large).
//
// Colours: one global palette (about 190 entries), built so that anti-aliased pieces stay smooth
// on every square colour: each square colour (light, dark, both highlighted) blended with black in
// 16 steps (the pieces' outlines), a 32-step grey ramp (black lines on white bodies, light lines
// on black bodies), the red glow of a check over both square colours in 32 steps, and the
// interface colours. Every pixel of a square (piece over square) is composed in true colour, then
// mapped to the nearest palette entry; the 6 x 13 square pictures (light / dark, highlighted, in
// check; empty or one of 12 pieces) are made once per size and cached, so a frame is a few blits.
//
// Sizes (square, picture with / without coordinates): small 32 px (284 x 350 / 268 x 342),
// medium 48 px (424 x 515 / 400 x 503), large 72 px (628 x 762 / 600 x 748). Pure and
// synchronous: the HTTP layer runs it on a worker thread (src/gif/pool.js). Render time on one
// core, warm caches: 80 plies 60-100 ms, 300 plies 100-370 ms (small to large); the first render
// of a size in a thread also rasterizes the pieces and squares (about 150-200 ms).

import { Position, WHITE, BLACK } from '../chess/index.js';
import { pieceSprite } from './pieces.js';
import { gifFont } from './font.js';
import { GifEncoder, Disposal } from './encoder.js';

/** Size presets. */
export const SIZES = Object.freeze({
    small: Object.freeze({ square: 32, bold: '12b', regular: '12n', coord: '12n', pad: 6, rowH: 20, rowGap: 2, margin: 14, bar: 2 }),
    medium: Object.freeze({ square: 48, bold: '16b', regular: '16n', coord: '16n', pad: 8, rowH: 28, rowGap: 3, margin: 20, bar: 3 }),
    large: Object.freeze({ square: 72, bold: '24b', regular: '24n', coord: '16b', pad: 12, rowH: 40, rowGap: 4, margin: 26, bar: 4 }),
});

/** Most plies a GIF shows (a longer game is refused). */
export const MAX_PLIES = 1200;

/** Delay bounds and defaults (milliseconds). */
export const DELAY = Object.freeze({ min: 100, max: 3000, default: 500, first: 1000, last: 3000 });

const THEME = Object.freeze({
    page: [0x26, 0x24, 0x21],
    text: [0xee, 0xea, 0xe2],
    dim: [0xa8, 0xa1, 0x96],
    accent: [0xe8, 0xb0, 0x40],
    swatchWhite: [0xfa, 0xf8, 0xf2],
    swatchBlack: [0x12, 0x11, 0x10],
    swatchEdge: [0x78, 0x72, 0x6a],
    light: [0xf0, 0xd9, 0xb5],
    dark: [0xb5, 0x88, 0x63],
    lightHi: [0xcd, 0xd2, 0x6a],
    darkHi: [0xaa, 0xa2, 0x3a],
    checkCore: [0xff, 0x00, 0x00],
    checkMid: [0xe7, 0x00, 0x00],
});

// Square kinds: base colour (light / dark) + state.
const K_LIGHT = 0, K_DARK = 1, K_LIGHT_HI = 2, K_DARK_HI = 3, K_LIGHT_CHECK = 4, K_DARK_CHECK = 5;

const mix = (a, b, t) => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];

// The red glow of a king in check at a distance t from the square's centre (1 = a corner):
// opaque red in the middle fading out (as lichess draws it). Returns [r, g, b, alpha].
function glowAt(t) {
    if (t <= 0.25) {
        const c = mix(THEME.checkCore, THEME.checkMid, t / 0.25);
        return [c[0], c[1], c[2], 1];
    }
    if (t >= 0.89) return [0, 0, 0, 0];
    const u = (t - 0.25) / 0.64;
    return [THEME.checkMid[0], THEME.checkMid[1], THEME.checkMid[2], 1 - u];
}

/** The palette: entries, named indices, and a nearest-colour lookup. */
class Palette {
    constructor() {
        this.rgb = [];
        this._keys = new Map();
        this._cache = new Map();
        // Index 0: the transparent colour of the frames after the first (never drawn).
        this.rgb.push(THEME.page.slice());
        this.idx = {};
        for (const name of ['page', 'text', 'dim', 'accent', 'swatchWhite', 'swatchBlack', 'swatchEdge']) {
            this.idx[name] = this._add(THEME[name]);
        }
        const bases = [THEME.light, THEME.dark, THEME.lightHi, THEME.darkHi];
        this.square = bases.map((b) => this._add(b));
        // Piece outlines (black) over each square colour.
        for (const b of bases) for (let k = 1; k < 16; k++) this._add(mix(b, [0, 0, 0], k / 16));
        // Grey ramp: black lines on white bodies, #ececec lines on black bodies.
        for (let k = 0; k <= 32; k++) this._add([k * 255 / 32, k * 255 / 32, k * 255 / 32]);
        this._add([0xec, 0xec, 0xec]);
        // The check glow over both square colours, and black outlines over the red.
        for (let k = 0; k < 4; k++) this._add(mix(THEME.checkCore, THEME.checkMid, k / 4));
        for (const b of [THEME.light, THEME.dark]) for (let k = 0; k < 32; k++) this._add(mix(THEME.checkMid, b, k / 32));
        for (let k = 1; k < 8; k++) this._add(mix(THEME.checkMid, [0, 0, 0], k / 8));
        /** Number of colours used (the rest of the 256 is free). */
        this.used = this.rgb.length;
        if (this.rgb.length > 256) throw new Error(`GIF palette too large (${this.rgb.length})`);
        this.flat = new Uint8Array(this.rgb.length * 3);
        this.rgb.forEach((c, i) => this.flat.set(c, i * 3));
    }

    _add(c) {
        const r = Math.round(c[0]), g = Math.round(c[1]), b = Math.round(c[2]);
        const k = (r << 16) | (g << 8) | b;
        let i = this._keys.get(k);
        if (i === undefined) {
            i = this.rgb.length;
            this.rgb.push([r, g, b]);
            this._keys.set(k, i);
        }
        return i;
    }

    /** Index of the nearest colour (never the transparent index 0). */
    nearest(r, g, b) {
        r = Math.round(r);
        g = Math.round(g);
        b = Math.round(b);
        const k = (r << 16) | (g << 8) | b;
        let best = this._keys.get(k);
        if (best !== undefined) return best;
        best = this._cache.get(k);
        if (best !== undefined) return best;
        let bd = Infinity;
        for (let i = 1; i < this.rgb.length; i++) {
            const c = this.rgb[i];
            const dr = c[0] - r, dg = c[1] - g, db = c[2] - b;
            const d = 3 * dr * dr + 4 * dg * dg + 2 * db * db;
            if (d < bd) {
                bd = d;
                best = i;
            }
        }
        this._cache.set(k, best);
        return best;
    }
}

let palette = null;
const getPalette = () => (palette ??= new Palette());

// Square pictures per size: kind * 16 + piece code -> palette indices (square * square).
const tileCache = new Map();

function tile(size, kind, code) {
    const key = size * 256 + kind * 16 + code;
    let t = tileCache.get(key);
    if (t) return t;
    const pal = getPalette();
    const S = size;
    t = new Uint8Array(S * S);
    const base = (kind === K_LIGHT || kind === K_LIGHT_HI || kind === K_LIGHT_CHECK) ? THEME.light : THEME.dark;
    const flat = kind === K_LIGHT_HI ? THEME.lightHi : kind === K_DARK_HI ? THEME.darkHi : base;
    const check = kind === K_LIGHT_CHECK || kind === K_DARK_CHECK;
    const sprite = code ? pieceSprite(code, S) : null;
    const flatIndex = pal.nearest(flat[0], flat[1], flat[2]);
    const half = S / 2, corner = S / Math.SQRT2;
    for (let y = 0; y < S; y++) {
        for (let x = 0; x < S; x++) {
            const i = y * S + x;
            let r = flat[0], g = flat[1], b = flat[2];
            if (check) {
                const gl = glowAt(Math.hypot(x + 0.5 - half, y + 0.5 - half) / corner);
                r = gl[0] * gl[3] + r * (1 - gl[3]);
                g = gl[1] * gl[3] + g * (1 - gl[3]);
                b = gl[2] * gl[3] + b * (1 - gl[3]);
            }
            if (sprite) {
                const a = sprite[i * 4 + 3];
                if (a > 0) {
                    r = sprite[i * 4] * 255 + r * (1 - a);
                    g = sprite[i * 4 + 1] * 255 + g * (1 - a);
                    b = sprite[i * 4 + 2] * 255 + b * (1 - a);
                } else if (!check) {
                    t[i] = flatIndex;
                    continue;
                }
            }
            t[i] = pal.nearest(r, g, b);
        }
    }
    tileCache.set(key, t);
    return t;
}

/** Drops the cached square pictures (tests, memory). */
export function clearRenderCache() {
    tileCache.clear();
}

// Printable text of a name: characters of the font only.
function cleanText(s, maxLen = 64) {
    let out = '';
    for (const ch of String(s ?? '')) {
        const c = ch.codePointAt(0);
        out += (c >= 0x20 && c < 0x7f) || c === 0xbd || c === 0xb7 ? ch : '?';
        if (out.length >= maxLen) break;
    }
    return out.trim();
}

function ratingText(r) {
    if (r === null || r === undefined || r === '') return '';
    if (typeof r === 'number') return Number.isFinite(r) ? String(Math.round(r)) : '';
    return cleanText(r, 12);
}

const RESULT_TEXT = { '1-0': '1-0', '0-1': '0-1', '1/2-1/2': '½-½', '*': '*' };
const SCORE = { '1-0': ['1', '0'], '0-1': ['0', '1'], '1/2-1/2': ['½', '½'] };

// The result of a job: '1-0', '0-1', '1/2-1/2' (also written with the one-half sign), else '*'.
function normalResult(r) {
    if (r === '½-½') return '1/2-1/2';
    return typeof r === 'string' && Object.hasOwn(RESULT_TEXT, r) ? r : '*';
}

/**
 * Replays the moves and records what each frame shows.
 * @returns {Array<{ board: Uint8Array, from: number, to: number, check: number, side: number, text: string }>}
 */
function replay(startFen, moves) {
    let pos;
    if (startFen === null || startFen === undefined || startFen === '') {
        pos = Position.start();
    } else {
        pos = Position.fromFEN(String(startFen));
        if (!pos) throw new RangeError('invalid start position (FEN)');
    }
    const boardOf = (p) => {
        const b = new Uint8Array(64);
        for (let sq = 0; sq < 64; sq++) b[sq] = p.pieceAt(sq);
        return b;
    };
    const checkOf = (p) => (p.inCheck() ? p.kingSquare(p.side) : -1);
    const states = [{ board: boardOf(pos), from: -1, to: -1, check: checkOf(pos), side: pos.side, number: '', san: '', text: '' }];
    for (let i = 0; i < moves.length; i++) {
        const m = moves[i];
        if (!pos.isLegal(m)) throw new RangeError(`illegal move at ply ${i + 1}`);
        const san = pos.san(m);
        const number = pos.side === WHITE ? `${pos.fullmove}.` : `${pos.fullmove}...`;
        pos.play(m);
        states.push({ board: boardOf(pos), from: m & 63, to: (m >> 6) & 63, check: checkOf(pos), side: pos.side, number, san, text: `${number} ${san}` });
    }
    return { states, final: pos };
}

// What the end of a game was, when the caller does not say: what the final position shows.
function derivedEnding(pos, result) {
    if (pos.isCheckmate()) return 'Checkmate';
    if (pos.isStalemate()) return 'Stalemate';
    if (result === '1/2-1/2' && pos.hasInsufficientMaterial()) return 'Insufficient material';
    return '';
}

/**
 * Renders a game as an animated GIF.
 * @param {{ startFen?: string|null, moves?: ArrayLike<number>, white?: { name?: string, rating?: number|string|null },
 *   black?: { name?: string, rating?: number|string|null }, result?: string, footer?: string,
 *   options?: { size?: 'small'|'medium'|'large', orientation?: 'white'|'black', delayMs?: number, coords?: boolean } }} job
 * @param {{ onFrame?: (f: { ply: number, width: number, height: number, pixels: Uint8Array, palette: Uint8Array, delayCs: number }) => void }} [hooks]
 *   onFrame: called with the whole picture of every frame (tools, tests)
 * @returns {Buffer} the GIF file
 * @throws {RangeError|TypeError} on an invalid job (bad FEN, illegal move, too many moves).
 */
export function renderGame(job, hooks = {}) {
    if (!job || typeof job !== 'object') throw new TypeError('renderGame: job object expected');
    const o = job.options ?? {};
    const sizeName = Object.hasOwn(SIZES, o.size ?? '') ? o.size : 'medium';
    const spec = SIZES[sizeName];
    const flip = o.orientation === 'black';
    const coords = o.coords !== false;
    let delayMs = Number(o.delayMs ?? DELAY.default);
    if (!Number.isFinite(delayMs)) delayMs = DELAY.default;
    delayMs = Math.min(DELAY.max, Math.max(DELAY.min, delayMs));
    const moves = Array.from(job.moves ?? [], Number);
    if (moves.length > MAX_PLIES) throw new RangeError(`too many moves (${moves.length} plies, at most ${MAX_PLIES})`);
    const result = normalResult(job.result);

    const { states, final } = replay(job.startFen, moves);
    const ending = job.footer !== undefined && job.footer !== null && String(job.footer).trim() !== ''
        ? cleanText(job.footer, 120)
        : derivedEnding(final, result);

    const pal = getPalette();
    const P = pal.idx;
    const fontB = gifFont(spec.bold), fontN = gifFont(spec.regular), fontC = gifFont(spec.coord);

    // Layout.
    const S = spec.square, boardPx = 8 * S;
    const margin = coords ? spec.margin : spec.pad;
    const width = boardPx + 2 * margin;
    const rowW = boardPx, rowX = margin;
    const row1Y = spec.pad, row2Y = row1Y + spec.rowH + spec.rowGap;
    const boardX = margin, boardY = row2Y + spec.rowH + spec.pad;
    const footerY = boardY + boardPx + (coords ? margin : spec.pad);
    const height = footerY + spec.rowH + spec.pad;

    const canvas = { width, height, data: new Uint8Array(width * height).fill(P.page) };
    const data = canvas.data;
    // Rectangles redrawn for the frame being drawn: only they are compared with the previous frame.
    const dirtyRects = [];
    const dirty = (x, y, w, h) => { dirtyRects.push(x, y, w, h); };
    const fillRect = (x, y, w, h, c) => {
        for (let yy = y; yy < y + h; yy++) data.fill(c, yy * width + x, yy * width + x + w);
    };
    // Text vertically centred on the capital letters in a row of height h at y.
    const textTop = (font, y, h) => y + Math.round((h - font.capHeight) / 2) - font.capTop;

    // Coordinates (drawn once).
    if (coords) {
        for (let i = 0; i < 8; i++) {
            const file = String.fromCharCode(97 + (flip ? 7 - i : i));
            const rank = String(flip ? i + 1 : 8 - i);
            const fx = boardX + i * S + Math.round((S - fontC.measure(file)) / 2);
            fontC.draw(canvas, fx, textTop(fontC, boardY + boardPx, margin) - 1, file, P.dim);
            const rx = Math.round((margin - fontC.measure(rank)) / 2);
            fontC.draw(canvas, rx, textTop(fontC, boardY + i * S, S), rank, P.dim);
        }
    }

    // Player rows.
    const players = [
        { color: WHITE, y: row1Y, name: cleanText(job.white?.name) || 'White', rating: ratingText(job.white?.rating) },
        { color: BLACK, y: row2Y, name: cleanText(job.black?.name) || 'Black', rating: ratingText(job.black?.rating) },
    ];
    const swatch = fontB.capHeight + 2;
    const drawRow = (pl, active, score) => {
        const { y } = pl;
        fillRect(rowX, y, rowW, spec.rowH, P.page);
        dirty(rowX, y, rowW, spec.rowH);
        if (active) fillRect(rowX, y, spec.bar, spec.rowH, P.accent);
        let x = rowX + spec.bar + spec.pad;
        const sy = y + Math.round((spec.rowH - swatch) / 2);
        fillRect(x, sy, swatch, swatch, P.swatchEdge);
        fillRect(x + 1, sy + 1, swatch - 2, swatch - 2, pl.color === WHITE ? P.swatchWhite : P.swatchBlack);
        x += swatch + spec.pad;
        const right = rowX + rowW - spec.pad;
        const scoreW = score ? fontB.measure(score) + spec.pad : 0;
        const ratingW = pl.rating ? fontN.measure(pl.rating) + fontN.space * 2 : 0;
        const name = fontB.fit(pl.name, Math.max(0, right - scoreW - ratingW - x));
        const ty = textTop(fontB, y, spec.rowH);
        x = fontB.draw(canvas, x, ty, name, P.text);
        if (pl.rating) fontN.draw(canvas, x + fontN.space * 2 - fontB.gap, textTop(fontN, y, spec.rowH), pl.rating, P.dim);
        if (score) {
            fontB.draw(canvas, right - fontB.measure(score), ty, score, P.text);
        } else if (active) {
            // The side to move: a dot at the end of the row.
            const d = swatch - 2, cx = right - d / 2, cy = sy + swatch / 2;
            for (let yy = 0; yy < d; yy++) {
                for (let xx = 0; xx < d; xx++) {
                    const ddx = xx + 0.5 - d / 2, ddy = yy + 0.5 - d / 2;
                    if (ddx * ddx + ddy * ddy <= (d / 2) * (d / 2) + 0.25) data[(Math.round(cy - d / 2) + yy) * width + Math.round(cx - d / 2) + xx] = P.accent;
                }
            }
        }
    };

    const drawFooter = (left, leftColor, right) => {
        fillRect(rowX, footerY, rowW, spec.rowH, P.page);
        dirty(rowX, footerY, rowW, spec.rowH);
        const ty = textTop(fontB, footerY, spec.rowH);
        let x = rowX;
        for (const [text, font, color] of left) {
            if (!text) continue;
            const fitted = font.fit(text, rowX + rowW - x);
            x = font.draw(canvas, x, font === fontB ? ty : textTop(font, footerY, spec.rowH), fitted, color ?? leftColor) + font.space;
        }
        if (right) {
            const w = fontN.measure(right);
            if (x + w <= rowX + rowW) fontN.draw(canvas, rowX + rowW - w, textTop(fontN, footerY, spec.rowH), right, P.dim);
        }
    };

    // Squares.
    const shownKind = new Int16Array(64).fill(-1), shownPiece = new Int16Array(64).fill(-1);
    const drawSquares = (st) => {
        for (let sq = 0; sq < 64; sq++) {
            const file = sq & 7, rank = sq >> 3;
            const light = ((file + rank) & 1) === 1;
            let kind = light ? K_LIGHT : K_DARK;
            if (sq === st.check) kind = light ? K_LIGHT_CHECK : K_DARK_CHECK;
            else if (sq === st.from || sq === st.to) kind = light ? K_LIGHT_HI : K_DARK_HI;
            const piece = st.board[sq];
            if (shownKind[sq] === kind && shownPiece[sq] === piece) continue;
            shownKind[sq] = kind;
            shownPiece[sq] = piece;
            const col = flip ? 7 - file : file, row = flip ? rank : 7 - rank;
            const x = boardX + col * S, y = boardY + row * S;
            const t = tile(S, kind, piece);
            for (let yy = 0; yy < S; yy++) data.set(t.subarray(yy * S, yy * S + S), (y + yy) * width + x);
            dirty(x, y, S, S);
        }
    };

    const enc = new GifEncoder({ width, height, palette: pal.flat, loop: 0, background: P.page });
    const prev = new Uint8Array(width * height);
    // Pixels of the changed box, reused by every frame: addFrame encodes them at once and keeps
    // no reference to them.
    const scratch = new Uint8Array(width * height);
    const last = states.length - 1;
    const finished = result !== '*';
    const resultText = RESULT_TEXT[result];

    for (let ply = 0; ply <= last; ply++) {
        const st = states[ply];
        const end = ply === last;
        drawSquares(st);
        const showEnd = end && (finished || ending !== '');
        const score = showEnd ? SCORE[result] : null;
        drawRow(players[0], !showEnd && st.side === WHITE, score ? score[0] : '');
        drawRow(players[1], !showEnd && st.side === BLACK, score ? score[1] : '');
        if (showEnd) drawFooter([[resultText, fontB, P.accent], [ending, fontN, P.text]], P.text, st.text);
        else drawFooter([[st.number, fontN, P.dim], [st.san, fontB, P.text]], P.text, '');

        const delayMs2 = ply === 0 ? (last === 0 ? DELAY.last : Math.max(DELAY.first, delayMs)) : end ? DELAY.last : delayMs;
        const delayCs = Math.round(delayMs2 / 10);
        if (ply === 0) {
            enc.addFrame({ x: 0, y: 0, width, height, pixels: data, delayCs, disposal: Disposal.Keep });
            prev.set(data);
        } else {
            // Exact box of the changed pixels (they are all inside the redrawn rectangles).
            let bx0 = width, by0 = height, bx1 = -1, by1 = -1;
            for (let r = 0; r < dirtyRects.length; r += 4) {
                const rx = dirtyRects[r], ry = dirtyRects[r + 1], rw = dirtyRects[r + 2], rh = dirtyRects[r + 3];
                for (let y = ry; y < ry + rh; y++) {
                    const o = y * width;
                    for (let x = rx; x < rx + rw; x++) {
                        if (data[o + x] !== prev[o + x]) {
                            if (x < bx0) bx0 = x;
                            if (x > bx1) bx1 = x;
                            if (y < by0) by0 = y;
                            if (y > by1) by1 = y;
                        }
                    }
                }
            }
            if (bx1 < 0) {
                // Nothing changed: a one-pixel transparent frame keeps the timing.
                enc.addFrame({ x: 0, y: 0, width: 1, height: 1, pixels: new Uint8Array(1), delayCs, disposal: Disposal.Keep, transparentIndex: 0 });
            } else {
                // The changed pixels; everything else is the transparent index 0.
                const w = bx1 - bx0 + 1, h = by1 - by0 + 1;
                const sub = scratch.subarray(0, w * h);
                sub.fill(0);
                for (let r = 0; r < dirtyRects.length; r += 4) {
                    const rx = dirtyRects[r], ry = dirtyRects[r + 1], rw = dirtyRects[r + 2], rh = dirtyRects[r + 3];
                    for (let y = ry; y < ry + rh; y++) {
                        const o = y * width, so = (y - by0) * w - bx0;
                        for (let x = rx; x < rx + rw; x++) {
                            const v = data[o + x];
                            if (v !== prev[o + x]) sub[so + x] = v;
                        }
                    }
                }
                for (let r = 0; r < dirtyRects.length; r += 4) {
                    const rx = dirtyRects[r], ry = dirtyRects[r + 1], rw = dirtyRects[r + 2], rh = dirtyRects[r + 3];
                    for (let y = ry; y < ry + rh; y++) prev.set(data.subarray(y * width + rx, y * width + rx + rw), y * width + rx);
                }
                enc.addFrame({ x: bx0, y: by0, width: w, height: h, pixels: sub, delayCs, disposal: Disposal.Keep, transparentIndex: 0 });
            }
        }
        if (hooks.onFrame) hooks.onFrame({ ply, width, height, pixels: data.slice(), palette: pal.flat, delayCs });
        dirtyRects.length = 0;
    }
    return enc.finish();
}

/**
 * Picture size of a preset (pixels), without rendering.
 * @param {'small'|'medium'|'large'} size
 * @param {boolean} [coords]
 * @returns {{ width: number, height: number }}
 */
export function imageSize(size, coords = true) {
    const spec = SIZES[size];
    if (!spec) throw new RangeError(`unknown size ${size}`);
    const margin = coords ? spec.margin : spec.pad;
    const boardPx = 8 * spec.square;
    const boardY = spec.pad + 2 * spec.rowH + spec.rowGap + spec.pad;
    return { width: boardPx + 2 * margin, height: boardY + boardPx + (coords ? margin : spec.pad) + spec.rowH + spec.pad };
}
