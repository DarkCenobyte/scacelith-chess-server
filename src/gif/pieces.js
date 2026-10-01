// The chess pieces of the game GIFs: the "cburnett" set by Colin M.L. Burnett (GPLv2+, see
// assets/pieces/cburnett/LICENSE.md), read once from the committed SVG files and rasterized
// anti-aliased in JavaScript (src/gif/raster.js) at any square size, cached per size.
//
// The SVG reader covers what these files use, and a little more: <svg viewBox>, nested <g>
// with inherited presentation attributes (fill, fill-rule, fill-opacity, stroke, stroke-width,
// stroke-linecap, stroke-linejoin, stroke-miterlimit, stroke-opacity, opacity) given as
// attributes or in a style attribute, and the shapes <path>, <circle>, <ellipse>, <rect>,
// <line>, <polyline>, <polygon>. Colours: #rgb, #rrggbb, black, white, none. Defaults are SVG's:
// fill black, stroke none, width 1, butt caps, miter joins, miter limit 4. Shapes are painted
// in document order, the fill then the stroke of each, with source-over compositing.
//
//   const sprite = pieceSprite(code, 72);   // code = type | colour << 3 (Position.pieceAt)
//   // sprite: Float32Array(72 * 72 * 4), premultiplied RGBA in 0..1, row-major
//
// Each sprite fills the whole square (the SVG's 45 x 45 view box scaled to the square), as the
// pieces are drawn on lichess and in most chess programs.

import { readFileSync } from 'node:fs';
import { flattenPath, polygonsOf, strokePolygons, rasterize } from './raster.js';

const ASSET_DIR = new URL('../../assets/pieces/cburnett/', import.meta.url);

/** File names by piece code (type | colour << 3; type 1 pawn .. 6 king, colour 0 White, 1 Black). */
export const PIECE_FILES = Object.freeze({
    1: 'wP.svg', 2: 'wN.svg', 3: 'wB.svg', 4: 'wR.svg', 5: 'wQ.svg', 6: 'wK.svg',
    9: 'bP.svg', 10: 'bN.svg', 11: 'bB.svg', 12: 'bR.svg', 13: 'bQ.svg', 14: 'bK.svg',
});

/** The piece codes, in a fixed order. */
export const PIECE_CODES = Object.freeze([1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14]);

/** Largest square size rasterized (pixels). */
export const MAX_SPRITE_SIZE = 256;

const INHERITED = ['fill', 'fill-rule', 'fill-opacity', 'stroke', 'stroke-width', 'stroke-linecap',
    'stroke-linejoin', 'stroke-miterlimit', 'stroke-opacity'];

const DEFAULT_STYLE = Object.freeze({
    'fill': 'black', 'fill-rule': 'nonzero', 'fill-opacity': '1', 'stroke': 'none', 'stroke-width': '1',
    'stroke-linecap': 'butt', 'stroke-linejoin': 'miter', 'stroke-miterlimit': '4', 'stroke-opacity': '1',
});

/**
 * Parses an SVG colour.
 * @param {string} s
 * @returns {number[]|null} [r, g, b] in 0..1, null for none.
 * @throws {Error} for an unsupported colour.
 */
export function parseColor(s) {
    const v = String(s).trim().toLowerCase();
    if (v === 'none' || v === 'transparent') return null;
    if (v === 'black') return [0, 0, 0];
    if (v === 'white') return [1, 1, 1];
    let m = /^#([0-9a-f]{6})$/.exec(v);
    if (m) {
        const n = parseInt(m[1], 16);
        return [(n >> 16) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255];
    }
    m = /^#([0-9a-f]{3})$/.exec(v);
    if (m) return [...m[1]].map((c) => parseInt(c + c, 16) / 255);
    throw new Error(`unsupported colour ${s}`);
}

function attributesOf(text) {
    const attrs = {};
    for (const m of text.matchAll(/([\w:-]+)\s*=\s*"([^"]*)"/g)) attrs[m[1]] = m[2];
    if (attrs.style) {
        for (const decl of attrs.style.split(';')) {
            const k = decl.indexOf(':');
            if (k > 0) attrs[decl.slice(0, k).trim()] = decl.slice(k + 1).trim();
        }
    }
    return attrs;
}

/**
 * A drawing: the view box and the shapes in paint order.
 * @typedef {{ viewBox: number[], shapes: Array<{ kind: string, attrs: object, style: object, opacity: number }> }} Drawing
 */

/**
 * Reads the subset of SVG described above.
 * @param {string} text
 * @returns {Drawing}
 */
export function parseSvg(text) {
    const shapes = [];
    const stack = [{ style: { ...DEFAULT_STYLE }, opacity: 1 }];
    let viewBox = null;
    const tags = /<(\/?)([a-zA-Z][\w:-]*)([^>]*?)(\/?)>/g;
    for (const m of text.replace(/<!--[\s\S]*?-->/g, '').matchAll(tags)) {
        const [, closing, name, rest, selfClosing] = m;
        if (closing) {
            if ((name === 'g' || name === 'svg') && stack.length > 1) stack.pop();
            continue;
        }
        const attrs = attributesOf(rest);
        const parent = stack[stack.length - 1];
        const style = { ...parent.style };
        for (const k of INHERITED) if (attrs[k] !== undefined && attrs[k] !== 'inherit') style[k] = attrs[k];
        const opacity = parent.opacity * (attrs.opacity !== undefined ? Number(attrs.opacity) : 1);
        if (name === 'svg') {
            const vb = attrs.viewBox ? attrs.viewBox.trim().split(/[\s,]+/).map(Number) : null;
            viewBox = vb && vb.length === 4 && vb.every(Number.isFinite)
                ? vb
                : [0, 0, Number(attrs.width) || 45, Number(attrs.height) || 45];
            if (!selfClosing) stack.push({ style, opacity });
        } else if (name === 'g') {
            if (!selfClosing) stack.push({ style, opacity });
        } else if (['path', 'circle', 'ellipse', 'rect', 'line', 'polyline', 'polygon'].includes(name)) {
            shapes.push({ kind: name, attrs, style, opacity });
        }
    }
    if (!viewBox) throw new Error('not an SVG document');
    return { viewBox, shapes };
}

function num(v, d = 0) {
    const n = Number(v);
    return Number.isFinite(n) ? n : d;
}

// Shape geometry as path data (user units).
function shapePath(shape) {
    const a = shape.attrs;
    switch (shape.kind) {
    case 'path': return a.d || '';
    case 'circle': {
        const cx = num(a.cx), cy = num(a.cy), r = num(a.r);
        return r > 0 ? `M${cx + r} ${cy}A${r} ${r} 0 1 1 ${cx - r} ${cy}A${r} ${r} 0 1 1 ${cx + r} ${cy}Z` : '';
    }
    case 'ellipse': {
        const cx = num(a.cx), cy = num(a.cy), rx = num(a.rx), ry = num(a.ry);
        return rx > 0 && ry > 0 ? `M${cx + rx} ${cy}A${rx} ${ry} 0 1 1 ${cx - rx} ${cy}A${rx} ${ry} 0 1 1 ${cx + rx} ${cy}Z` : '';
    }
    case 'rect': {
        const x = num(a.x), y = num(a.y), w = num(a.width), h = num(a.height);
        return w > 0 && h > 0 ? `M${x} ${y}h${w}v${h}h${-w}Z` : '';
    }
    case 'line': return `M${num(a.x1)} ${num(a.y1)}L${num(a.x2)} ${num(a.y2)}`;
    case 'polyline':
    case 'polygon': {
        const p = String(a.points || '').trim().split(/[\s,]+/).map(Number);
        if (p.length < 4) return '';
        return `M${p.join(' ')}${shape.kind === 'polygon' ? 'Z' : ''}`;
    }
    default: return '';
    }
}

/**
 * Paints a drawing into a premultiplied RGBA buffer.
 * @param {Drawing} drawing
 * @param {number} size output width and height in pixels (the view box is scaled to it)
 * @returns {Float32Array} size * size * 4
 */
export function renderDrawing(drawing, size) {
    const [vx, vy, vw, vh] = drawing.viewBox;
    const scale = Math.min(size / vw, size / vh);
    const dx = -vx * scale + (size - vw * scale) / 2, dy = -vy * scale + (size - vh * scale) / 2;
    const out = new Float32Array(size * size * 4);
    const cov = new Float32Array(size * size);
    const paint = (color, alpha) => {
        const [r, g, b] = color;
        for (let i = 0, j = 0; i < cov.length; i++, j += 4) {
            const a = cov[i] * alpha;
            if (a <= 0) continue;
            const k = 1 - a;
            out[j] = r * a + out[j] * k;
            out[j + 1] = g * a + out[j + 1] * k;
            out[j + 2] = b * a + out[j + 2] * k;
            out[j + 3] = a + out[j + 3] * k;
        }
    };
    for (const shape of drawing.shapes) {
        const d = shapePath(shape);
        if (!d) continue;
        const st = shape.style;
        const subpaths = flattenPath(d, { scale, dx, dy, tol: 0.03 });
        const fill = parseColor(st.fill);
        if (fill) {
            rasterize(polygonsOf(subpaths), st['fill-rule'] === 'evenodd' ? 'evenodd' : 'nonzero', size, size, cov);
            paint(fill, shape.opacity * num(st['fill-opacity'], 1));
        }
        const stroke = parseColor(st.stroke);
        const width = num(st['stroke-width'], 1) * scale;
        if (stroke && width > 0) {
            const polys = strokePolygons(subpaths, width, st['stroke-linecap'], st['stroke-linejoin'],
                num(st['stroke-miterlimit'], 4), 0.03);
            rasterize(polys, 'nonzero', size, size, cov);
            paint(stroke, shape.opacity * num(st['stroke-opacity'], 1));
        }
    }
    return out;
}

let drawings = null;

function loadDrawings() {
    if (drawings) return drawings;
    const map = new Map();
    for (const code of PIECE_CODES) {
        map.set(code, parseSvg(readFileSync(new URL(PIECE_FILES[code], ASSET_DIR), 'utf8')));
    }
    drawings = map;
    return map;
}

const sprites = new Map();

/**
 * The anti-aliased sprite of a piece at a square size (cached).
 * @param {number} code piece code: type | colour << 3
 * @param {number} size square size in pixels, 8..MAX_SPRITE_SIZE
 * @returns {Float32Array} size * size * 4 premultiplied RGBA in 0..1
 */
export function pieceSprite(code, size) {
    if (!Object.hasOwn(PIECE_FILES, code)) throw new RangeError(`bad piece code ${code}`);
    if (!Number.isInteger(size) || size < 8 || size > MAX_SPRITE_SIZE) throw new RangeError(`bad sprite size ${size}`);
    const key = code * 1024 + size;
    let s = sprites.get(key);
    if (!s) {
        s = renderDrawing(loadDrawings().get(code), size);
        sprites.set(key, s);
    }
    return s;
}

/** Drops the cached sprites (tests, memory). */
export function clearPieceCache() {
    sprites.clear();
}

