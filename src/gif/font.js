// Bitmap fonts of the game GIFs: crisp one-bit glyphs, no anti-aliasing (every text pixel is the
// text colour, so text costs no palette entries and stays sharp at every size). The glyphs are
// subsets of Terminus Font by Dimitar Toshkov Zhekov (SIL Open Font License 1.1), renamed
// "Scacelith GIF" as the OFL asks of modified fonts: assets/fonts/scacelith-gif/*.bdf with
// OFL.txt, written by tools/gen-gif-font.js. Read once per font (BDF), on first use.
//
// Terminus is a monospaced font; text is set proportionally here (each glyph advances by its ink
// width plus a gap), which reads better in names and sentences. Characters the font lacks print
// as '?'.
//
//   const f = gifFont('16b');                  // 12n 12b 16n 16b 24n 24b
//   const w = f.measure('Carlsen (2830)');
//   f.draw(canvas, x, top, 'Carlsen (2830)', colorIndex);   // canvas: { width, height, data }

import { readFileSync } from 'node:fs';

const FONT_DIR = new URL('../../assets/fonts/scacelith-gif/', import.meta.url);

/** The font names available. */
export const FONT_NAMES = Object.freeze(['12n', '12b', '16n', '16b', '24n', '24b']);

/**
 * @typedef {{ width: number, height: number, data: Uint8Array }} Canvas  an indexed image
 */

/**
 * Parses a BDF font (the subset of BDF 2.1 that bitmap fonts like Terminus use).
 * @param {string} text
 * @returns {{ ascent: number, descent: number, glyphs: Map<number, {w: number, h: number, xoff: number, yoff: number, dwidth: number, rows: number[]}> }}
 */
export function parseBdf(text) {
    const ascent = Number(/^FONT_ASCENT (-?\d+)$/m.exec(text)?.[1]);
    const descent = Number(/^FONT_DESCENT (-?\d+)$/m.exec(text)?.[1]);
    if (!Number.isFinite(ascent) || !Number.isFinite(descent)) throw new Error('BDF: FONT_ASCENT / FONT_DESCENT missing');
    const glyphs = new Map();
    for (const m of text.matchAll(/STARTCHAR[^\n]*\n([\s\S]*?)ENDCHAR/g)) {
        const body = m[1];
        const enc = Number(/^ENCODING (-?\d+)$/m.exec(body)?.[1]);
        const bbx = /^BBX (\d+) (\d+) (-?\d+) (-?\d+)$/m.exec(body);
        const dw = /^DWIDTH (-?\d+) (-?\d+)$/m.exec(body);
        const bi = body.indexOf('BITMAP\n');
        if (!(enc >= 0) || !bbx || !dw || bi < 0) continue;
        const [w, h, xoff, yoff] = bbx.slice(1, 5).map(Number);
        const hex = body.slice(bi + 7).trim().split('\n').filter((l) => l.length > 0);
        if (hex.length !== h) throw new Error(`BDF: glyph ${enc} has ${hex.length} rows, ${h} expected`);
        const rows = hex.map((l) => {
            const v = parseInt(l, 16);
            const bits = l.length * 4;
            return bits >= w ? Math.floor(v / 2 ** (bits - w)) : v;     // keep the w leftmost bits
        });
        glyphs.set(enc, { w, h, xoff, yoff, dwidth: Number(dw[1]), rows });
    }
    return { ascent, descent, glyphs };
}

/** A bitmap font with proportional text setting. */
export class BitmapFont {
    /**
     * @param {{ ascent: number, descent: number, glyphs: Map<number, object> }} bdf parseBdf's result
     * @param {{ gap?: number, space?: number }} [o] gap: pixels between glyphs; space: advance of a space
     */
    constructor(bdf, { gap, space } = {}) {
        this.ascent = bdf.ascent;
        this.descent = bdf.descent;
        /** Line height in pixels. */
        this.height = bdf.ascent + bdf.descent;
        this.gap = gap ?? Math.max(1, Math.round(this.height / 12));
        this._glyphs = new Map();
        const cell = bdf.glyphs.get(0x30)?.dwidth ?? Math.round(this.height / 2);
        for (const [cp, g] of bdf.glyphs) {
            // Ink columns of the glyph (relative to the glyph origin).
            let left = Infinity, right = -Infinity;
            const px = [];
            for (let y = 0; y < g.h; y++) {
                const row = g.rows[y];
                for (let x = 0; x < g.w; x++) {
                    if (Math.floor(row / 2 ** (g.w - 1 - x)) & 1) {
                        const gx = g.xoff + x;
                        // Top of the glyph box is ascent - (yoff + h) below the line top.
                        px.push(gx, this.ascent - (g.yoff + g.h) + y);
                        if (gx < left) left = gx;
                        if (gx > right) right = gx;
                    }
                }
            }
            const blank = px.length === 0;
            this._glyphs.set(cp, {
                px: Int16Array.from(blank ? [] : px.map((v, i) => (i % 2 === 0 ? v - left : v))),
                ink: blank ? 0 : right - left + 1,
                blank,
            });
        }
        /** Advance of a space. */
        this.space = space ?? Math.max(2, Math.round(cell / 2));
        // Rows of the capital letters (from the line top): text is centred on them.
        const H = this._glyphs.get(0x48);
        let capTop = 0, capBottom = this.ascent - 1;
        if (H && !H.blank) {
            capTop = Infinity;
            capBottom = -Infinity;
            for (let i = 1; i < H.px.length; i += 2) {
                capTop = Math.min(capTop, H.px[i]);
                capBottom = Math.max(capBottom, H.px[i]);
            }
        }
        /** First row of a capital letter, from the top of the line box. */
        this.capTop = capTop;
        /** Height of a capital letter. */
        this.capHeight = capBottom - capTop + 1;
        /** Advance of a digit (digits are set at a fixed width: numbers keep their alignment). */
        this.digit = 0;
        for (let d = 0x30; d <= 0x39; d++) this.digit = Math.max(this.digit, this._glyphs.get(d)?.ink ?? 0);
    }

    _glyph(cp) {
        return this._glyphs.get(cp) ?? this._glyphs.get(0x3f);
    }

    _advance(cp, g) {
        if (cp === 0x20 || g.blank) return this.space;
        if (cp >= 0x30 && cp <= 0x39) return this.digit + this.gap;
        return g.ink + this.gap;
    }

    /**
     * Width of a text in pixels (no trailing gap).
     * @param {string} text
     */
    measure(text) {
        let w = 0;
        let last = 0;
        for (const ch of String(text)) {
            const cp = ch.codePointAt(0);
            const g = this._glyph(cp);
            const a = this._advance(cp, g);
            w += a;
            last = (cp === 0x20 || g.blank) ? 0 : this.gap;
        }
        return Math.max(0, w - last);
    }

    /**
     * The longest prefix of a text that fits in maxWidth, with an ellipsis when cut.
     * @param {string} text
     * @param {number} maxWidth
     * @returns {string}
     */
    fit(text, maxWidth) {
        const s = String(text);
        if (this.measure(s) <= maxWidth) return s;
        const chars = [...s];
        for (let n = chars.length - 1; n > 0; n--) {
            const t = chars.slice(0, n).join('').trimEnd() + '…';
            if (this.measure(t) <= maxWidth) return t;
        }
        return '';
    }

    /**
     * Draws a text (one line) into an indexed canvas, clipped to it.
     * @param {Canvas} canvas
     * @param {number} x left edge
     * @param {number} top top of the line box (the baseline is top + ascent)
     * @param {string} text
     * @param {number} color palette index
     * @returns {number} the x after the text
     */
    draw(canvas, x, top, text, color) {
        const { width, height, data } = canvas;
        let cx = x | 0;
        for (const ch of String(text)) {
            const cp = ch.codePointAt(0);
            const g = this._glyph(cp);
            if (!g.blank && cp !== 0x20) {
                // Digits are centred in their fixed cell.
                const ox = (cp >= 0x30 && cp <= 0x39) ? cx + ((this.digit - g.ink) >> 1) : cx;
                const p = g.px;
                for (let i = 0; i < p.length; i += 2) {
                    const px = ox + p[i], py = (top | 0) + p[i + 1];
                    if (px >= 0 && px < width && py >= 0 && py < height) data[py * width + px] = color;
                }
            }
            cx += this._advance(cp, g);
        }
        return cx;
    }
}

const fonts = new Map();

/**
 * One of the GIF fonts (loaded on first use, then cached).
 * @param {string} name one of FONT_NAMES
 * @returns {BitmapFont}
 */
export function gifFont(name) {
    if (!FONT_NAMES.includes(name)) throw new RangeError(`unknown GIF font ${name}`);
    let f = fonts.get(name);
    if (!f) {
        f = new BitmapFont(parseBdf(readFileSync(new URL(`sg-${name}.bdf`, FONT_DIR), 'latin1')));
        fonts.set(name, f);
    }
    return f;
}
