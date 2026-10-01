// Vector rasterizer (src/gif/raster.js), cburnett piece sprites (src/gif/pieces.js) and bitmap
// fonts (src/gif/font.js) of the game GIFs.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { parsePathData, flattenPath, polygonsOf, strokePolygons, rasterize, circlePolygon } from '../../src/gif/raster.js';
import { pieceSprite, parseSvg, parseColor, renderDrawing, PIECE_CODES, clearPieceCache } from '../../src/gif/pieces.js';
import { gifFont, FONT_NAMES, parseBdf, BitmapFont } from '../../src/gif/font.js';

const sum = (a) => a.reduce((x, y) => x + y, 0);

test('path data: commands, implicit repeats, compact numbers and arc flags', () => {
    assert.deepEqual(parsePathData('M1 2L3 4'), [{ c: 'M', a: [1, 2] }, { c: 'L', a: [3, 4] }]);
    assert.deepEqual(parsePathData('m1,2 3,4'), [{ c: 'm', a: [1, 2] }, { c: 'l', a: [3, 4] }]);
    assert.deepEqual(parsePathData('M1.5.5-1e1 2z'), [{ c: 'M', a: [1.5, 0.5] }, { c: 'L', a: [-10, 2] }, { c: 'z', a: [] }]);
    // cburnett writes arcs as "a.5.5 0 1 1-1 0": flags are single characters.
    assert.deepEqual(parsePathData('a.5.5 0 1 1-1 0'), [{ c: 'a', a: [0.5, 0.5, 0, 1, 1, -1, 0] }]);
    assert.deepEqual(parsePathData('A35 35 1 0 0 23 0')[0].a, [35, 35, 1, 0, 0, 23, 0]);
    assert.equal(parsePathData('h1 2 3').length, 3);
    for (const bad of ['1 2', 'M1', 'M1 2 L', 'a1 1 0 2 0 1 1', 'M1 2 Z 3']) assert.throws(() => parsePathData(bad), SyntaxError, bad);
});

test('fill coverage: exact areas, sub-pixel edges, nonzero and evenodd', () => {
    // A 10 x 10 square at (2.5, 2.5): area 100, half-covered edge pixels.
    const sq = flattenPath('M2.5 2.5h10v10h-10z');
    const cov = rasterize(polygonsOf(sq), 'nonzero', 16, 16);
    assert.ok(Math.abs(sum(cov) - 100) < 1e-3, `area ${sum(cov)}`);
    assert.ok(Math.abs(cov[2 * 16 + 5] - 0.5) < 1e-6);      // top edge pixel
    assert.ok(Math.abs(cov[2 * 16 + 2] - 0.25) < 1e-6);     // corner pixel
    assert.equal(cov[5 * 16 + 5], 1);
    assert.equal(cov[0], 0);
    // A circle of radius 20: area ~ pi r^2.
    const circle = rasterize([circlePolygon(32, 32, 20, 0.01)], 'nonzero', 64, 64);
    assert.ok(Math.abs(sum(circle) - Math.PI * 400) < 2, `circle ${sum(circle)}`);
    // A ring: two concentric circles in the same direction; evenodd makes a hole, nonzero does not.
    const ring = [circlePolygon(32, 32, 20, 0.01), circlePolygon(32, 32, 10, 0.01)];
    assert.ok(Math.abs(sum(rasterize(ring, 'evenodd', 64, 64)) - Math.PI * 300) < 2);
    assert.ok(Math.abs(sum(rasterize(ring, 'nonzero', 64, 64)) - Math.PI * 400) < 2);
    // Shapes partly outside the grid are clipped.
    assert.ok(Math.abs(sum(rasterize(polygonsOf(flattenPath('M-5 -5h10v10h-10z')), 'nonzero', 8, 8)) - 25) < 1e-3);
    // Curves and arcs: the path of a circle with two arcs has the circle's area.
    const arcs = flattenPath('M52 32A20 20 0 1 1 12 32A20 20 0 1 1 52 32Z', { tol: 0.01 });
    assert.ok(Math.abs(sum(rasterize(polygonsOf(arcs), 'nonzero', 64, 64)) - Math.PI * 400) < 2);
    const quad = flattenPath('M0 0Q10 20 20 0T40 0', { tol: 0.01 });
    assert.ok(quad[0].pts.length > 10);
    assert.equal(quad[0].pts.at(-2), 40);
});

test('strokes: width, caps and joins', () => {
    const line = flattenPath('M10 10H50');
    const area = (polys) => sum(rasterize(polys, 'nonzero', 64, 64));
    assert.ok(Math.abs(area(strokePolygons(line, 4, 'butt', 'miter')) - 160) < 1e-3);
    assert.ok(Math.abs(area(strokePolygons(line, 4, 'square', 'miter')) - 176) < 1e-3);
    assert.ok(Math.abs(area(strokePolygons(line, 4, 'round', 'miter', 4, 0.002)) - (160 + Math.PI * 4)) < 0.05);
    // A right-angle corner: a miter join fills the corner square, a bevel half of it, round a quarter circle.
    const corner = flattenPath('M10 10H40V40');
    const miter = area(strokePolygons(corner, 6, 'butt', 'miter'));
    const bevel = area(strokePolygons(corner, 6, 'butt', 'bevel'));
    const round = area(strokePolygons(corner, 6, 'butt', 'round', 4, 0.002));
    assert.ok(Math.abs(miter - bevel - 4.5) < 0.05, `${miter} ${bevel}`);
    assert.ok(Math.abs(miter - round - (9 - Math.PI * 9 / 4)) < 0.2, `${miter} ${round}`);
    // A very sharp angle exceeds the miter limit: beveled.
    const sharp = flattenPath('M10 10L50 12L10 14');
    assert.ok(Math.abs(area(strokePolygons(sharp, 2, 'butt', 'miter', 4)) - area(strokePolygons(sharp, 2, 'butt', 'bevel'))) < 0.01);
    // A closed rectangle stroked: outer minus inner.
    assert.ok(Math.abs(area(strokePolygons(flattenPath('M10 10h20v20h-20z'), 2, 'butt', 'miter')) - (22 * 22 - 18 * 18)) < 1e-3);
    // A zero-length subpath with round caps is a dot.
    assert.ok(Math.abs(area(strokePolygons(flattenPath('M20 20z'), 4, 'round', 'round', 4, 0.002)) - Math.PI * 4) < 0.05);
    assert.equal(strokePolygons(flattenPath('M20 20z'), 4, 'butt', 'round').length, 0);
});

test('SVG subset: inheritance, colours, shapes', () => {
    assert.deepEqual(parseColor('#fff'), [1, 1, 1]);
    assert.deepEqual(parseColor('#000000'), [0, 0, 0]);
    assert.equal(parseColor('none'), null);
    assert.ok(Math.abs(parseColor('#ececec')[0] - 236 / 255) < 1e-9);
    assert.throws(() => parseColor('red'));
    const d = parseSvg('<svg viewBox="0 0 10 10"><g fill="#fff" stroke="#000" stroke-width="2"><g style="stroke-width:1"><path d="M0 0h1"/></g>'
        + '<circle cx="5" cy="5" r="2" fill="none"/></g><rect x="1" y="1" width="2" height="2"/></svg>');
    assert.deepEqual(d.viewBox, [0, 0, 10, 10]);
    assert.equal(d.shapes.length, 3);
    assert.equal(d.shapes[0].style.fill, '#fff');
    assert.equal(d.shapes[0].style['stroke-width'], '1');
    assert.equal(d.shapes[1].style.fill, 'none');
    assert.equal(d.shapes[1].style['stroke-width'], '2');
    assert.equal(d.shapes[2].style.fill, 'black');        // SVG default
    assert.equal(d.shapes[2].style.stroke, 'none');
    const img = renderDrawing(d, 20);
    assert.equal(img.length, 20 * 20 * 4);
    // The black rect (1..3 in a 10 box, scaled 2x) is opaque black at (4, 4).
    const i = (4 * 20 + 4) * 4;
    assert.deepEqual([img[i], img[i + 1], img[i + 2], img[i + 3]], [0, 0, 0, 1]);
    assert.throws(() => parseSvg('<p>no svg</p>'));
});

test('cburnett sprites: every piece, sane coverage, white pieces lighter, cached per size', () => {
    clearPieceCache();
    for (const size of [16, 32, 45, 72, 100]) {
        for (const code of PIECE_CODES) {
            const s = pieceSprite(code, size);
            assert.equal(s.length, size * size * 4);
            let alpha = 0, light = 0;
            let minX = size, maxX = -1;
            for (let y = 0; y < size; y++) {
                for (let x = 0; x < size; x++) {
                    const j = (y * size + x) * 4;
                    const a = s[j + 3];
                    assert.ok(a >= 0 && a <= 1 + 1e-6);
                    assert.ok(s[j] <= a + 1e-6, 'premultiplied');
                    alpha += a;
                    light += s[j];
                    if (a > 0.5) {
                        minX = Math.min(minX, x);
                        maxX = Math.max(maxX, x);
                    }
                }
            }
            const fill = alpha / (size * size);
            assert.ok(fill > 0.1 && fill < 0.6, `piece ${code} at ${size}: coverage ${fill}`);
            const lightness = light / alpha;
            if (code < 8) assert.ok(lightness > 0.35, `white piece ${code} lightness ${lightness}`);
            else assert.ok(lightness < 0.3, `black piece ${code} lightness ${lightness}`);
            // Roughly centred horizontally.
            assert.ok(Math.abs((minX + maxX) / 2 - (size - 1) / 2) < size * 0.08, `piece ${code} centred`);
            // Anti-aliased: partial coverage exists on the edges.
            let partial = 0;
            for (let k = 3; k < s.length; k += 4) if (s[k] > 0.05 && s[k] < 0.95) partial++;
            assert.ok(partial > size / 2, `piece ${code} at ${size} anti-aliased`);
        }
    }
    // The pawn is left-right symmetric.
    const p = pieceSprite(1, 45);
    let asym = 0;
    for (let y = 0; y < 45; y++) for (let x = 0; x < 45; x++) asym += Math.abs(p[(y * 45 + x) * 4 + 3] - p[(y * 45 + 44 - x) * 4 + 3]);
    assert.ok(asym < 3, `pawn asymmetry ${asym}`);
    assert.equal(pieceSprite(6, 48), pieceSprite(6, 48));
    assert.throws(() => pieceSprite(7, 48), RangeError);
    assert.throws(() => pieceSprite(1, 4), RangeError);
    assert.throws(() => pieceSprite(1, 47.5), RangeError);
});

test('bitmap fonts: the characters of the GIFs, crisp drawing, measure and fit', () => {
    const needed = [];
    for (let c = 32; c < 127; c++) needed.push(c);
    needed.push(0xbd, 0x2026, 0xb7);
    for (const name of FONT_NAMES) {
        const f = gifFont(name);
        assert.equal(f.height, parseInt(name, 10));
        for (const c of needed) assert.ok(f._glyphs.has(c), `${name}: U+${c.toString(16)}`);
        assert.ok(f.capHeight > f.height / 3 && f.capHeight < f.height);
        // Drawing writes only the colour asked for, inside the canvas.
        const canvas = { width: 300, height: 40, data: new Uint8Array(300 * 40) };
        const end = f.draw(canvas, 2, 4, 'Carlsen 2830 ½-½', 7);
        assert.ok(end > 2 && end <= 300);
        const used = new Set(canvas.data);
        assert.deepEqual([...used].sort(), [0, 7]);
        assert.ok(canvas.data.filter((v) => v === 7).length > 50);
        // Text is set proportionally: "il" is narrower than "MW"; digits share one width.
        assert.ok(f.measure('il') < f.measure('MW'));
        assert.equal(f.measure('1111'), f.measure('8888'));
        assert.equal(f.measure(''), 0);
        // fit() cuts with an ellipsis.
        const long = 'abcdefghijklmnopqrstuvwxyz';
        const cut = f.fit(long, f.measure('abcdefgh'));
        assert.ok(cut.endsWith('…') && f.measure(cut) <= f.measure('abcdefgh'));
        assert.equal(f.fit('abc', 1000), 'abc');
        // Unknown characters print as '?'; drawing clips at the canvas edges.
        assert.equal(f.measure('中'), f.measure('?'));
        f.draw(canvas, -50, -10, 'clipped', 3);
        f.draw(canvas, 290, 30, 'clipped', 3);
    }
    assert.throws(() => gifFont('13n'), RangeError);
    assert.throws(() => parseBdf('STARTFONT 2.1\n'), /FONT_ASCENT/);
    const tiny = new BitmapFont(parseBdf('FONT_ASCENT 2\nFONT_DESCENT 0\nSTARTCHAR A\nENCODING 65\nDWIDTH 2 0\nBBX 2 2 0 0\nBITMAP\n80\n40\nENDCHAR\n'));
    const c = { width: 4, height: 2, data: new Uint8Array(8) };
    tiny.draw(c, 0, 0, 'A', 1);
    assert.deepEqual([...c.data], [1, 0, 0, 0, 0, 1, 0, 0]);
});
