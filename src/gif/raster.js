// Anti-aliased vector rasterizer of the game GIFs (src/gif/pieces.js draws the cburnett pieces with
// it): SVG path data (M L H V C S Q T A Z, absolute and relative) flattened to polygons, strokes
// turned into polygons (butt / round / square caps, miter / round / bevel joins with the miter
// limit), and coverage computed per pixel.
//
// Coverage: every pixel row is cut into SUB sub-scanlines; on each one the crossings of the
// polygon edges are sorted and walked with the winding number (nonzero or evenodd rule), and the
// inside spans are added with their exact horizontal extent (fractional at both ends). The result
// is exact horizontally and has SUB + 1 levels vertically: smooth edges at any angle, no
// sampling noise. A stroke is the union of its segment rectangles, join wedges and caps, all
// oriented the same way, filled with the nonzero rule (overlaps never cancel).
//
//   const paths = flattenPath('M9 36c3.4-1 ...z', { scale: 2, tol: 0.05 });
//   const fillCov = rasterize(polygonsOf(paths), 'evenodd', 90, 90);
//   const strokeCov = rasterize(strokePolygons(paths, 3, 'round', 'round', 4), 'nonzero', 90, 90);
//
// Pure and synchronous; no allocation is retained between calls.

/** Sub-scanlines per pixel row (vertical anti-aliasing levels). */
export const SUB = 16;

const CMD_ARGS = { M: 2, L: 2, H: 1, V: 1, C: 6, S: 4, Q: 4, T: 2, A: 7, Z: 0 };
const NUMBER = /[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?/y;

function isSeparator(c) {
    return c === ' ' || c === ',' || c === '\t' || c === '\n' || c === '\r' || c === '\f';
}

/**
 * Parses SVG path data.
 * @param {string} d
 * @returns {Array<{c: string, a: number[]}>} commands as written (case kept), arguments as numbers
 *   (arc flags as 0 / 1); an implicit command after M / m is L / l.
 * @throws {SyntaxError} on malformed data.
 */
export function parsePathData(d) {
    const out = [];
    const n = d.length;
    let i = 0;
    let cmd = null;
    const skip = () => { while (i < n && isSeparator(d[i])) i++; };
    const num = () => {
        skip();
        NUMBER.lastIndex = i;
        const m = NUMBER.exec(d);
        if (!m) throw new SyntaxError(`path data: number expected at ${i}`);
        i = NUMBER.lastIndex;
        return Number(m[0]);
    };
    const flag = () => {
        skip();
        const c = d[i];
        if (c !== '0' && c !== '1') throw new SyntaxError(`path data: arc flag expected at ${i}`);
        i++;
        return c === '1' ? 1 : 0;
    };
    for (;;) {
        skip();
        if (i >= n) break;
        const c = d[i];
        const upper = c.toUpperCase();
        if (Object.hasOwn(CMD_ARGS, upper) && c !== 'e' && c !== 'E') {
            cmd = c;
            i++;
            if (upper === 'Z') {
                out.push({ c, a: [] });
                continue;
            }
        } else if (cmd === null || cmd === 'Z' || cmd === 'z') {
            throw new SyntaxError(`path data: command expected at ${i}`);
        }
        const count = CMD_ARGS[cmd.toUpperCase()];
        const a = new Array(count);
        for (let k = 0; k < count; k++) a[k] = (cmd === 'A' || cmd === 'a') && (k === 3 || k === 4) ? flag() : num();
        out.push({ c: cmd, a });
        if (cmd === 'M') cmd = 'L';
        else if (cmd === 'm') cmd = 'l';
    }
    return out;
}

/**
 * A path flattened to polylines, in pixel coordinates.
 * @typedef {{ pts: number[], closed: boolean }} Subpath  pts: x0, y0, x1, y1, ...
 */

/**
 * Flattens SVG path data (or parsed commands) to polylines.
 * @param {string|Array<{c: string, a: number[]}>} d
 * @param {{ scale?: number, dx?: number, dy?: number, tol?: number }} [t] pixel = user * scale + d;
 *   tol: maximum distance in pixels between a curve and its polyline (default 0.05).
 * @returns {Subpath[]}
 */
export function flattenPath(d, { scale = 1, dx = 0, dy = 0, tol = 0.05 } = {}) {
    const cmds = typeof d === 'string' ? parsePathData(d) : d;
    const subpaths = [];
    let cur = null;
    let x = 0, y = 0, sx = 0, sy = 0;        // current point, subpath start (user units)
    let cx2 = 0, cy2 = 0, prev = '';         // last control point (S / T reflection)
    const tolUser = tol / scale;
    const emit = (px, py) => { cur.pts.push(px * scale + dx, py * scale + dy); };
    const begin = (px, py) => {
        cur = { pts: [], closed: false };
        subpaths.push(cur);
        emit(px, py);
    };
    const ensure = () => { if (!cur) begin(x, y); };
    const cubic = (x1, y1, x2, y2, x3, y3) => {
        const ddx = Math.max(Math.abs(x - 2 * x1 + x2), Math.abs(x1 - 2 * x2 + x3));
        const ddy = Math.max(Math.abs(y - 2 * y1 + y2), Math.abs(y1 - 2 * y2 + y3));
        const steps = Math.max(1, Math.ceil(Math.sqrt(0.75 * Math.hypot(ddx, ddy) / tolUser)));
        for (let k = 1; k <= steps; k++) {
            const t = k / steps, u = 1 - t;
            const a = u * u * u, b = 3 * u * u * t, c = 3 * u * t * t, e = t * t * t;
            emit(a * x + b * x1 + c * x2 + e * x3, a * y + b * y1 + c * y2 + e * y3);
        }
    };
    const quad = (x1, y1, x2, y2) => {
        const dd = Math.hypot(x - 2 * x1 + x2, y - 2 * y1 + y2);
        const steps = Math.max(1, Math.ceil(Math.sqrt(0.25 * dd / tolUser)));
        for (let k = 1; k <= steps; k++) {
            const t = k / steps, u = 1 - t;
            emit(u * u * x + 2 * u * t * x1 + t * t * x2, u * u * y + 2 * u * t * y1 + t * t * y2);
        }
    };
    for (const { c, a } of cmds) {
        const rel = c >= 'a';
        const ox = rel ? x : 0, oy = rel ? y : 0;
        switch (c.toUpperCase()) {
        case 'M':
            x = a[0] + ox; y = a[1] + oy;
            sx = x; sy = y;
            begin(x, y);
            break;
        case 'L':
            ensure();
            x = a[0] + ox; y = a[1] + oy;
            emit(x, y);
            break;
        case 'H':
            ensure();
            x = a[0] + ox;
            emit(x, y);
            break;
        case 'V':
            ensure();
            y = a[0] + oy;
            emit(x, y);
            break;
        case 'C': {
            ensure();
            const x1 = a[0] + ox, y1 = a[1] + oy, x2 = a[2] + ox, y2 = a[3] + oy, x3 = a[4] + ox, y3 = a[5] + oy;
            cubic(x1, y1, x2, y2, x3, y3);
            cx2 = x2; cy2 = y2;
            x = x3; y = y3;
            break;
        }
        case 'S': {
            ensure();
            const p = prev.toUpperCase();
            const x1 = (p === 'C' || p === 'S') ? 2 * x - cx2 : x;
            const y1 = (p === 'C' || p === 'S') ? 2 * y - cy2 : y;
            const x2 = a[0] + ox, y2 = a[1] + oy, x3 = a[2] + ox, y3 = a[3] + oy;
            cubic(x1, y1, x2, y2, x3, y3);
            cx2 = x2; cy2 = y2;
            x = x3; y = y3;
            break;
        }
        case 'Q': {
            ensure();
            const x1 = a[0] + ox, y1 = a[1] + oy, x2 = a[2] + ox, y2 = a[3] + oy;
            quad(x1, y1, x2, y2);
            cx2 = x1; cy2 = y1;
            x = x2; y = y2;
            break;
        }
        case 'T': {
            ensure();
            const p = prev.toUpperCase();
            const x1 = (p === 'Q' || p === 'T') ? 2 * x - cx2 : x;
            const y1 = (p === 'Q' || p === 'T') ? 2 * y - cy2 : y;
            const x2 = a[0] + ox, y2 = a[1] + oy;
            quad(x1, y1, x2, y2);
            cx2 = x1; cy2 = y1;
            x = x2; y = y2;
            break;
        }
        case 'A': {
            ensure();
            const x2 = a[5] + ox, y2 = a[6] + oy;
            arc(x, y, a[0], a[1], a[2], a[3], a[4], x2, y2, emit, tolUser);
            x = x2; y = y2;
            break;
        }
        case 'Z':
            if (cur) {
                cur.closed = true;
                x = sx; y = sy;
                cur = null;     // a command after Z starts a new subpath at the start point
            }
            break;
        default:
            break;
        }
        prev = c;
    }
    return subpaths;
}

// SVG 1.1 F.6.5: endpoint to center parameterization, then points every few degrees.
function arc(x1, y1, rx, ry, phiDeg, fa, fs, x2, y2, emit, tol) {
    if (x1 === x2 && y1 === y2) return;
    rx = Math.abs(rx);
    ry = Math.abs(ry);
    if (rx === 0 || ry === 0) {
        emit(x2, y2);
        return;
    }
    const phi = (phiDeg % 360) * Math.PI / 180;
    const cos = Math.cos(phi), sin = Math.sin(phi);
    const hx = (x1 - x2) / 2, hy = (y1 - y2) / 2;
    const x1p = cos * hx + sin * hy, y1p = -sin * hx + cos * hy;
    const lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if (lambda > 1) {
        const s = Math.sqrt(lambda);
        rx *= s;
        ry *= s;
    }
    const num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    const den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    const coef = (fa === fs ? -1 : 1) * Math.sqrt(Math.max(0, num / den));
    const cxp = coef * rx * y1p / ry, cyp = -coef * ry * x1p / rx;
    const cx = cos * cxp - sin * cyp + (x1 + x2) / 2, cy = sin * cxp + cos * cyp + (y1 + y2) / 2;
    const ang = (ux, uy, vx, vy) => Math.atan2(ux * vy - uy * vx, ux * vx + uy * vy);
    const ux = (x1p - cxp) / rx, uy = (y1p - cyp) / ry;
    const t1 = ang(1, 0, ux, uy);
    let dt = ang(ux, uy, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if (!fs && dt > 0) dt -= 2 * Math.PI;
    else if (fs && dt < 0) dt += 2 * Math.PI;
    const r = Math.max(rx, ry);
    const step = r > tol ? 2 * Math.acos(Math.max(-1, 1 - tol / r)) : Math.PI / 2;
    const n = Math.max(2, Math.ceil(Math.abs(dt) / step));
    for (let k = 1; k < n; k++) {
        const t = t1 + dt * k / n;
        const ex = rx * Math.cos(t), ey = ry * Math.sin(t);
        emit(cx + cos * ex - sin * ey, cy + sin * ex + cos * ey);
    }
    emit(x2, y2);
}

/**
 * The closed polygons of a fill (every subpath, closed or not, is filled as if closed).
 * @param {Subpath[]} subpaths
 * @returns {number[][]}
 */
export function polygonsOf(subpaths) {
    return subpaths.filter((s) => s.pts.length >= 6).map((s) => s.pts);
}

/**
 * A circle as a polygon (positive orientation).
 * @param {number} cx @param {number} cy @param {number} r pixels
 * @param {number} [tol] pixels
 * @returns {number[]}
 */
export function circlePolygon(cx, cy, r, tol = 0.05) {
    const step = r > tol ? 2 * Math.acos(Math.max(-1, 1 - tol / r)) : Math.PI / 2;
    const n = Math.max(8, Math.ceil(2 * Math.PI / step));
    const pts = new Array(2 * n);
    for (let k = 0; k < n; k++) {
        const t = 2 * Math.PI * k / n;
        pts[2 * k] = cx + r * Math.cos(t);
        pts[2 * k + 1] = cy + r * Math.sin(t);
    }
    return pts;
}

function signedArea(p) {
    let s = 0;
    const n = p.length;
    for (let i = 0; i < n; i += 2) {
        const j = (i + 2) % n;
        s += p[i] * p[j + 1] - p[j] * p[i + 1];
    }
    return s / 2;
}

function oriented(p) {
    if (signedArea(p) >= 0) return p;
    const q = new Array(p.length);
    for (let i = 0, n = p.length / 2; i < n; i++) {
        q[2 * i] = p[2 * (n - 1 - i)];
        q[2 * i + 1] = p[2 * (n - 1 - i) + 1];
    }
    return q;
}

/**
 * The polygons of a stroke: their nonzero union is the stroked area.
 * @param {Subpath[]} subpaths pixel coordinates
 * @param {number} width stroke width in pixels
 * @param {'butt'|'round'|'square'} cap
 * @param {'miter'|'round'|'bevel'} join
 * @param {number} [miterLimit] SVG stroke-miterlimit (default 4)
 * @param {number} [tol] pixels
 * @returns {number[][]}
 */
export function strokePolygons(subpaths, width, cap, join, miterLimit = 4, tol = 0.05) {
    const hw = width / 2;
    const out = [];
    if (!(hw > 0)) return out;
    const add = (p) => { if (p.length >= 6 && Math.abs(signedArea(p)) > 1e-12) out.push(oriented(p)); };
    for (const sp of subpaths) {
        // Points without consecutive duplicates (and without the closing duplicate of a closed path).
        const src = sp.pts;
        const xs = [], ys = [];
        for (let i = 0; i < src.length; i += 2) {
            const px = src[i], py = src[i + 1];
            const k = xs.length - 1;
            if (k >= 0 && Math.abs(xs[k] - px) < 1e-9 && Math.abs(ys[k] - py) < 1e-9) continue;
            xs.push(px);
            ys.push(py);
        }
        let n = xs.length;
        const closed = sp.closed;
        if (closed && n > 1 && Math.abs(xs[0] - xs[n - 1]) < 1e-9 && Math.abs(ys[0] - ys[n - 1]) < 1e-9) {
            xs.pop();
            ys.pop();
            n--;
        }
        if (n === 1) {
            // A zero-length subpath: a dot for round and square caps (SVG 1.1 11.4).
            if (cap === 'round') add(circlePolygon(xs[0], ys[0], hw, tol));
            else if (cap === 'square') add([xs[0] - hw, ys[0] - hw, xs[0] + hw, ys[0] - hw, xs[0] + hw, ys[0] + hw, xs[0] - hw, ys[0] + hw]);
            continue;
        }
        if (n === 0) continue;
        const segs = closed ? n : n - 1;
        const dxs = new Array(segs), dys = new Array(segs);
        for (let i = 0; i < segs; i++) {
            const j = (i + 1) % n;
            const ddx = xs[j] - xs[i], ddy = ys[j] - ys[i];
            const len = Math.hypot(ddx, ddy);
            dxs[i] = ddx / len;
            dys[i] = ddy / len;
            const nx = -dys[i] * hw, ny = dxs[i] * hw;
            add([xs[i] + nx, ys[i] + ny, xs[j] + nx, ys[j] + ny, xs[j] - nx, ys[j] - ny, xs[i] - nx, ys[i] - ny]);
        }
        // Joins: every vertex of a closed path, the inner vertices of an open one.
        for (let v = closed ? 0 : 1; v < (closed ? n : n - 1); v++) {
            const a = (v - 1 + segs) % segs, b = v % segs;
            const d0x = dxs[a], d0y = dys[a], d1x = dxs[b], d1y = dys[b];
            const cross = d0x * d1y - d0y * d1x, dot = d0x * d1x + d0y * d1y;
            if (Math.abs(cross) < 1e-12 && dot > 0) continue;     // straight on
            const vx = xs[v], vy = ys[v];
            if (join === 'round') {
                add(circlePolygon(vx, vy, hw, tol));
                continue;
            }
            const s = cross > 0 ? -1 : 1;                           // the outer side of the turn
            const n0x = -d0y, n0y = d0x, n1x = -d1y, n1y = d1x;
            const p0x = vx + s * hw * n0x, p0y = vy + s * hw * n0y;
            const p1x = vx + s * hw * n1x, p1y = vy + s * hw * n1y;
            const nd = n0x * n1x + n0y * n1y;
            const sum = Math.hypot(n0x + n1x, n0y + n1y);
            if (join === 'miter' && sum > 1e-9 && 2 / sum <= miterLimit) {
                const tx = vx + s * hw * (n0x + n1x) / (1 + nd), ty = vy + s * hw * (n0y + n1y) / (1 + nd);
                add([vx, vy, p0x, p0y, tx, ty, p1x, p1y]);
            } else {
                add([vx, vy, p0x, p0y, p1x, p1y]);
            }
        }
        if (!closed) {
            const ends = [[0, -dxs[0], -dys[0]], [n - 1, dxs[segs - 1], dys[segs - 1]]];
            for (const [k, ox, oy] of ends) {
                if (cap === 'round') {
                    add(circlePolygon(xs[k], ys[k], hw, tol));
                } else if (cap === 'square') {
                    const nx = -oy * hw, ny = ox * hw;
                    const ex = xs[k] + ox * hw, ey = ys[k] + oy * hw;
                    add([xs[k] + nx, ys[k] + ny, ex + nx, ey + ny, ex - nx, ey - ny, xs[k] - nx, ys[k] - ny]);
                }
            }
        }
    }
    return out;
}

/**
 * Coverage of polygons on a width x height pixel grid (pixel (x, y) is the square
 * [x, x+1) x [y, y+1)).
 * @param {number[][]} polygons closed polygons, pixel coordinates
 * @param {'nonzero'|'evenodd'} rule
 * @param {number} width
 * @param {number} height
 * @param {Float32Array} [out] width * height values in 0..1 (overwritten); allocated when missing
 * @returns {Float32Array}
 */
export function rasterize(polygons, rule, width, height, out = new Float32Array(width * height)) {
    out.fill(0);
    // Edges: x at the top, slope dx/dy, y range [y0, y1), winding direction.
    let count = 0;
    for (const p of polygons) count += p.length / 2;
    const ex = new Float64Array(count), ey0 = new Float64Array(count), ey1 = new Float64Array(count);
    const eslope = new Float64Array(count);
    const edir = new Int8Array(count);
    let m = 0;
    let minY = Infinity, maxY = -Infinity;
    for (const p of polygons) {
        const n = p.length;
        for (let i = 0; i < n; i += 2) {
            const j = (i + 2) % n;
            let x0 = p[i], y0 = p[i + 1], x1 = p[j], y1 = p[j + 1];
            if (y0 === y1) continue;
            let dir = 1;
            if (y0 > y1) {
                [x0, x1] = [x1, x0];
                [y0, y1] = [y1, y0];
                dir = -1;
            }
            ex[m] = x0;
            ey0[m] = y0;
            ey1[m] = y1;
            eslope[m] = (x1 - x0) / (y1 - y0);
            edir[m] = dir;
            m++;
            if (y0 < minY) minY = y0;
            if (y1 > maxY) maxY = y1;
        }
    }
    if (m === 0) return out;
    // Edges sorted by their top, walked with an active list.
    const order = Array.from({ length: m }, (_, k) => k).sort((a, b) => ey0[a] - ey0[b]);
    const active = [];
    let nextEdge = 0;
    const xs = new Float64Array(m), ds = new Int8Array(m);
    const acc = new Float32Array(width + 1);
    const evenOdd = rule === 'evenodd';
    const rowStart = Math.max(0, Math.floor(minY)), rowEnd = Math.min(height, Math.ceil(maxY));
    const w1 = 1 / SUB;
    for (let row = rowStart; row < rowEnd; row++) {
        acc.fill(0);
        let any = false;
        for (let s = 0; s < SUB; s++) {
            const sy = row + (s + 0.5) * w1;
            while (nextEdge < m && ey0[order[nextEdge]] <= sy) active.push(order[nextEdge++]);
            let k = 0;
            for (let a = 0; a < active.length; a++) {
                const e = active[a];
                if (ey1[e] <= sy) {
                    active[a] = active[active.length - 1];
                    active.pop();
                    a--;
                    continue;
                }
                // Insertion sort by x as the crossings are collected.
                const x = ex[e] + (sy - ey0[e]) * eslope[e];
                let j = k++;
                while (j > 0 && xs[j - 1] > x) {
                    xs[j] = xs[j - 1];
                    ds[j] = ds[j - 1];
                    j--;
                }
                xs[j] = x;
                ds[j] = edir[e];
            }
            if (k < 2) continue;
            let wind = 0;
            for (let j = 0; j < k - 1; j++) {
                wind += ds[j];
                const inside = evenOdd ? (wind & 1) !== 0 : wind !== 0;
                if (!inside) continue;
                let a = xs[j], b = xs[j + 1];
                if (a < 0) a = 0;
                if (b > width) b = width;
                if (b <= a) continue;
                any = true;
                const ia = Math.floor(a), ib = Math.floor(b);
                if (ia === ib) {
                    acc[ia] += (b - a) * w1;
                } else {
                    acc[ia] += (ia + 1 - a) * w1;
                    for (let q = ia + 1; q < ib; q++) acc[q] += w1;
                    if (ib < width) acc[ib] += (b - ib) * w1;
                }
            }
        }
        if (!any) continue;
        const base = row * width;
        for (let x = 0; x < width; x++) {
            const v = acc[x];
            if (v > 0) out[base + x] = v > 1 ? 1 : v;
        }
    }
    return out;
}
