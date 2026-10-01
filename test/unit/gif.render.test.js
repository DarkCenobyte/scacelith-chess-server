// Game GIFs (src/gif/render.js): determinism, frames and delays, picture sizes, what the frames
// show (squares, last move, check, coordinates, orientation), invalid jobs. The GIFs are decoded
// with the tests' own decoder and compared with the renderer's pictures (onFrame).
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { renderGame, imageSize, SIZES, DELAY, MAX_PLIES } from '../../src/gif/render.js';
import { Position, parseSquare } from '../../src/chess/index.js';
import { decodeGif, composeFrames } from './helpers/gif-decode.js';

const uciMoves = (list, fen) => {
    const p = fen ? Position.fromFEN(fen) : Position.start();
    return list.split(' ').map((u) => {
        const m = p.parseUCI(u);
        assert.notEqual(m, -1, u);
        p.play(m);
        return m;
    });
};

// Morphy's Opera game (1858), 17. Rd8# at the end.
const OPERA = uciMoves('e2e4 e7e5 g1f3 d7d6 d2d4 c8g4 d4e5 g4f3 d1f3 d6e5 f1c4 g8f6 f3b3 d8e7 b1c3 c7c6 c1g5 b7b5 c3b5 c6b5 c4b5 b8d7 e1c1 a8d8 d1d7 d8d7 h1d1 e7e6 b5d7 f6d7 b3b8 d7b8 d1d8');

const job = (extra = {}) => ({
    moves: OPERA, white: { name: 'Morphy', rating: 2690 }, black: { name: 'Duke_Karl', rating: null }, result: '1-0',
    footer: 'Checkmate', options: { size: 'small' }, ...extra,
});

function rgbAt(gif, screen, x, y) {
    const i = screen[y * gif.width + x] * 3;
    return [gif.palette[i], gif.palette[i + 1], gif.palette[i + 2]];
}

test('deterministic: the same job gives the same bytes', () => {
    const a = renderGame(job());
    const b = renderGame(job());
    assert.ok(a.equals(b));
    assert.equal(a.subarray(0, 6).toString('latin1'), 'GIF89a');
});

test('one frame per position, delays: start held, moves at delayMs, end held 3 s, loops forever', () => {
    const gif = decodeGif(renderGame(job({ options: { size: 'small', delayMs: 400 } })));
    assert.equal(gif.frames.length, OPERA.length + 1);
    assert.equal(gif.loop, 0);
    assert.equal(gif.frames[0].delayCs, DELAY.first / 10);
    for (let i = 1; i < OPERA.length; i++) assert.equal(gif.frames[i].delayCs, 40);
    assert.equal(gif.frames[OPERA.length].delayCs, DELAY.last / 10);
    // The first frame is the whole picture; later frames are transparent sub-rectangles.
    assert.deepEqual([gif.frames[0].x, gif.frames[0].y, gif.frames[0].width, gif.frames[0].height], [0, 0, gif.width, gif.height]);
    assert.equal(gif.frames[0].transparentIndex, null);
    for (const f of gif.frames.slice(1)) {
        assert.equal(f.transparentIndex, 0);
        assert.ok(f.width * f.height < gif.width * gif.height);
    }
    // Index 0 (transparent) is never drawn.
    for (const screen of composeFrames(gif)) assert.ok(!screen.includes(0));
    // Delay clamped to 100..3000 ms; a long delay also holds the start that long.
    const fast = decodeGif(renderGame(job({ options: { size: 'small', delayMs: 1 } })));
    assert.equal(fast.frames[1].delayCs, DELAY.min / 10);
    const slow = decodeGif(renderGame(job({ options: { size: 'small', delayMs: 1e9 } })));
    assert.equal(slow.frames[1].delayCs, DELAY.max / 10);
    assert.equal(slow.frames[0].delayCs, DELAY.max / 10);
    const nan = decodeGif(renderGame(job({ options: { size: 'small', delayMs: 'fast' } })));
    assert.equal(nan.frames[1].delayCs, DELAY.default / 10);
    // No move: one frame.
    const none = decodeGif(renderGame(job({ moves: [], result: '*', footer: '' })));
    assert.equal(none.frames.length, 1);
});

test('picture sizes of the presets; the GIF decodes to exactly the rendered frames', () => {
    for (const size of Object.keys(SIZES)) {
        for (const coords of [true, false]) {
            const frames = [];
            const buf = renderGame(job({ moves: OPERA.slice(0, 12), options: { size, coords } }), { onFrame: (f) => frames.push(f) });
            const gif = decodeGif(buf);
            const want = imageSize(size, coords);
            assert.equal(gif.width, want.width, `${size} width`);
            assert.equal(gif.height, want.height, `${size} height`);
            assert.ok(gif.width >= 8 * SIZES[size].square);
            const screens = composeFrames(gif);
            assert.equal(screens.length, frames.length);
            screens.forEach((s, k) => assert.deepEqual(s, frames[k].pixels, `${size} frame ${k}`));
            assert.deepEqual(Uint8Array.from(gif.palette.subarray(0, frames[0].palette.length)), frames[0].palette);
        }
    }
    assert.throws(() => imageSize('huge'), RangeError);
});

test('board squares, last move highlight, check glow, orientation', () => {
    const size = 'medium';
    const S = SIZES[size].square, m = SIZES[size].margin;
    const boardY = SIZES[size].pad + 2 * SIZES[size].rowH + SIZES[size].rowGap + SIZES[size].pad;
    const centre = (sq, flip) => {
        const file = sq & 7, rank = sq >> 3;
        const col = flip ? 7 - file : file, row = flip ? rank : 7 - rank;
        return [m + col * S + (S >> 1), boardY + row * S + (S >> 1)];
    };
    const corner = (sq, flip) => {
        const [x, y] = centre(sq, flip);
        return [x - (S >> 1) + 1, y - (S >> 1) + 1];
    };
    for (const flip of [false, true]) {
        const gif = decodeGif(renderGame(job({ options: { size, orientation: flip ? 'black' : 'white' } })));
        const screens = composeFrames(gif);
        const start = screens[0], end = screens[screens.length - 1];
        // a1 is dark, h1 light; their corners show the plain square colours at the start.
        const dark = rgbAt(gif, start, ...corner(parseSquare('a1'), flip));
        const light = rgbAt(gif, start, ...corner(parseSquare('h1'), flip));
        assert.ok(light[0] + light[1] + light[2] > dark[0] + dark[1] + dark[2], 'light squares are lighter');
        // Last move Rd1-d8#: both squares highlighted (yellow-green), the mated king's square glows red.
        for (const sq of ['d1', 'd8']) {
            const c = rgbAt(gif, end, ...corner(parseSquare(sq), flip));
            assert.ok(c[1] > c[2] + 40, `${sq} highlighted: ${c}`);
        }
        const e8 = corner(parseSquare('e8'), flip);
        const near = rgbAt(gif, end, e8[0] - 1 + Math.round(S * 0.25), e8[1] - 1 + Math.round(S * 0.22));
        assert.ok(near[0] > 200 && near[1] < 120, `e8 glows red: ${near}`);
        // At the start e8 holds the black king: the bulb under its cross is black.
        const king = rgbAt(gif, start, e8[0] - 1 + Math.round(S * 0.5), e8[1] - 1 + Math.round(S * 0.4));
        assert.ok(king[0] + king[1] + king[2] < 200, `black king on e8: ${king}`);
    }
});

test('header and footer: names, ratings, turn marker and the result change the picture', () => {
    const base = renderGame(job());
    assert.ok(!base.equals(renderGame(job({ white: { name: 'Anderssen', rating: 2690 } }))));
    assert.ok(!base.equals(renderGame(job({ black: { name: 'Duke_Karl', rating: 1800 } }))));
    assert.ok(!base.equals(renderGame(job({ footer: 'White wins' }))));
    assert.ok(!base.equals(renderGame(job({ result: '*', footer: '' }))));
    // Names keep only printable characters; long names are cut, never overflow.
    const long = 'x'.repeat(200);
    const gif = decodeGif(renderGame(job({ white: { name: long, rating: 'abc\u0001' }, black: { name: 'é中' } })));
    assert.equal(gif.width, imageSize('small').width);
    // Unknown sizes and orientations fall back to medium / white.
    const fallback = decodeGif(renderGame(job({ options: { size: 'enormous', orientation: 'sideways' } })));
    assert.equal(fallback.width, imageSize('medium').width);
});

test('custom start position (FEN) and promotion', () => {
    const fen = '8/P7/8/8/8/8/k7/4K3 w - - 0 1';
    const moves = uciMoves('a7a8q a2b2 a8b8', fen);
    const gif = decodeGif(renderGame({ startFen: fen, moves, white: { name: 'w' }, black: { name: 'b' }, result: '*', options: { size: 'small' } }));
    assert.equal(gif.frames.length, 4);
});

test('invalid jobs are refused with RangeError / TypeError', () => {
    assert.throws(() => renderGame(null), TypeError);
    assert.throws(() => renderGame(job({ startFen: 'not a fen' })), /invalid start position/);
    assert.throws(() => renderGame(job({ moves: [...OPERA.slice(0, 5), 0] })), /illegal move at ply 6/);
    assert.throws(() => renderGame(job({ moves: [12 | (28 << 6), 0x10000] })), /illegal move at ply 2/);
    assert.throws(() => renderGame(job({ moves: new Array(MAX_PLIES + 1).fill(0) })), /too many moves/);
});

test('render time and size stay reasonable (300 plies, small)', () => {
    const p = Position.start();
    const moves = [];
    let x = 7;
    while (moves.length < 300) {
        const legal = p.legalMoves();
        if (!legal.length) break;
        x = (x * 1103515245 + 12345) >>> 0;
        const m = legal[x % legal.length];
        p.play(m);
        moves.push(m);
    }
    const t0 = performance.now();
    const buf = renderGame({ moves, white: { name: 'a' }, black: { name: 'b' }, result: '*', options: { size: 'small' } });
    const ms = performance.now() - t0;
    assert.equal(decodeGif(buf).frames.length, moves.length + 1);
    assert.ok(buf.length < 1.5 * 1024 * 1024, `${buf.length} bytes`);
    assert.ok(ms < 20000, `${ms} ms`);
});
