// GIF89a writer (src/gif/encoder.js), checked with the tests' own decoder (helpers/gif-decode.js):
// header, palette, loop extension, frames and their control extensions, LZW round trips (code
// size growth, clear codes, long runs through the fast path), transparency.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { GifEncoder, Disposal } from '../../src/gif/encoder.js';
import { decodeGif, composeFrames } from './helpers/gif-decode.js';

function rng(seed) {
    let x = seed >>> 0 || 1;
    return () => {
        x ^= x << 13; x >>>= 0;
        x ^= x >>> 17;
        x ^= x << 5; x >>>= 0;
        return x / 4294967296;
    };
}

function palette(n) {
    const p = new Uint8Array(n * 3);
    for (let i = 0; i < n; i++) p.set([i, (i * 7) & 255, 255 - i], i * 3);
    return p;
}

test('header, logical screen, global palette and loop extension', () => {
    const enc = new GifEncoder({ width: 5, height: 3, palette: palette(3), loop: 0, background: 2 });
    enc.addFrame({ width: 5, height: 3, pixels: new Uint8Array(15).fill(1), delayCs: 42 });
    const buf = enc.finish();
    assert.equal(buf.subarray(0, 6).toString('latin1'), 'GIF89a');
    assert.equal(buf[buf.length - 1], 0x3b);
    const gif = decodeGif(buf);
    assert.equal(gif.width, 5);
    assert.equal(gif.height, 3);
    assert.equal(gif.background, 2);
    assert.equal(gif.loop, 0);
    // 3 colours are stored as a table of 4 (power of two), padded with black.
    assert.deepEqual([...gif.palette], [...palette(3), 0, 0, 0]);
    assert.equal(gif.frames.length, 1);
    assert.equal(gif.frames[0].delayCs, 42);
    assert.equal(gif.frames[0].disposal, Disposal.Keep);
    assert.equal(gif.frames[0].transparentIndex, null);
    assert.equal(gif.frames[0].minCodeSize, 2);
    assert.deepEqual([...gif.frames[0].pixels], new Array(15).fill(1));
    // The result owns its ArrayBuffer (transferable between threads).
    assert.equal(buf.byteOffset, 0);
    assert.equal(buf.buffer.byteLength, buf.length);

    const noLoop = new GifEncoder({ width: 1, height: 1, palette: palette(2), loop: null });
    noLoop.addFrame({ width: 1, height: 1, pixels: new Uint8Array(1) });
    assert.equal(decodeGif(noLoop.finish()).loop, null);
});

test('LZW round trip: every palette size, random and repetitive pictures, clear codes', () => {
    const r = rng(12345);
    for (const colors of [2, 3, 4, 5, 16, 17, 100, 128, 129, 256]) {
        const pictures = [
            Uint8Array.from({ length: 64 * 64 }, () => Math.floor(r() * colors)),         // noise: many clear codes
            Uint8Array.from({ length: 300 * 50 }, (_, i) => (i >> 5) % colors),           // stripes
            new Uint8Array(200 * 200).fill(colors - 1),                                   // one long run
            Uint8Array.from({ length: 1 }, () => colors - 1),
        ];
        for (const pixels of pictures) {
            const w = pixels.length === 1 ? 1 : pixels.length === 64 * 64 ? 64 : pixels.length === 300 * 50 ? 300 : 200;
            const enc = new GifEncoder({ width: w, height: pixels.length / w, palette: palette(colors) });
            enc.addFrame({ width: w, height: pixels.length / w, pixels });
            const gif = decodeGif(enc.finish());
            assert.deepEqual(gif.frames[0].pixels, pixels, `${colors} colours, ${pixels.length} pixels`);
        }
    }
    // Noise fills the 4096-entry table several times: the decoder sees the clear codes.
    const noise = Uint8Array.from({ length: 256 * 256 }, () => Math.floor(r() * 256));
    const enc = new GifEncoder({ width: 256, height: 256, palette: palette(256) });
    enc.addFrame({ width: 256, height: 256, pixels: noise });
    const gif = decodeGif(enc.finish());
    assert.deepEqual(gif.frames[0].pixels, noise);
    assert.ok(gif.frames[0].clearCodes > 5, `clear codes: ${gif.frames[0].clearCodes}`);
});

test('LZW round trip of transparent runs (the fast path) mixed with changes', () => {
    const r = rng(99);
    for (let trial = 0; trial < 40; trial++) {
        const w = 1 + Math.floor(r() * 400), h = 1 + Math.floor(r() * 300);
        const pixels = new Uint8Array(w * h);   // transparent index 0 everywhere...
        const blobs = Math.floor(r() * 12);
        for (let b = 0; b < blobs; b++) {        // ...but for a few rectangles of content
            const bx = Math.floor(r() * w), by = Math.floor(r() * h);
            const bw = 1 + Math.floor(r() * 60), bh = 1 + Math.floor(r() * 60);
            const flat = r() < 0.5;
            for (let y = by; y < Math.min(h, by + bh); y++) {
                for (let x = bx; x < Math.min(w, bx + bw); x++) pixels[y * w + x] = flat ? 7 : Math.floor(r() * 256);
            }
        }
        const enc = new GifEncoder({ width: w, height: h, palette: palette(256) });
        enc.addFrame({ width: w, height: h, pixels, transparentIndex: 0 });
        const gif = decodeGif(enc.finish());
        assert.deepEqual(gif.frames[0].pixels, pixels, `trial ${trial}: ${w}x${h}`);
        assert.equal(gif.frames[0].transparentIndex, 0);
    }
    // A long run costs about sqrt(2n) codes.
    const enc = new GifEncoder({ width: 600, height: 600, palette: palette(256) });
    const before = enc._w.len;
    enc.addFrame({ width: 600, height: 600, pixels: new Uint8Array(600 * 600), transparentIndex: 0 });
    assert.ok(enc._w.len - before < 1500, `${enc._w.len - before} bytes for 360000 transparent pixels`);
});

test('frames: sub-rectangles, delays, disposal, transparency compose like a viewer', () => {
    const pal = palette(4);
    const enc = new GifEncoder({ width: 4, height: 3, palette: pal });
    enc.addFrame({ width: 4, height: 3, pixels: Uint8Array.from([1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3]), delayCs: 100 });
    enc.addFrame({ x: 1, y: 1, width: 2, height: 2, pixels: Uint8Array.from([0, 3, 2, 0]), delayCs: 50, transparentIndex: 0 });
    enc.addFrame({ x: 3, y: 0, width: 1, height: 1, pixels: Uint8Array.from([2]), delayCs: 300, disposal: Disposal.Keep });
    assert.equal(enc.frames, 3);
    const gif = decodeGif(enc.finish());
    assert.deepEqual(gif.frames.map((f) => [f.x, f.y, f.width, f.height, f.delayCs, f.transparentIndex]),
        [[0, 0, 4, 3, 100, null], [1, 1, 2, 2, 50, 0], [3, 0, 1, 1, 300, null]]);
    const screens = composeFrames(gif);
    assert.deepEqual([...screens[1]], [1, 1, 1, 1, 2, 2, 3, 2, 3, 2, 3, 3]);
    assert.deepEqual([...screens[2]], [1, 1, 1, 2, 2, 2, 3, 2, 3, 2, 3, 3]);
});

test('invalid arguments are refused', () => {
    assert.throws(() => new GifEncoder({ width: 0, height: 1, palette: palette(2) }), RangeError);
    assert.throws(() => new GifEncoder({ width: 1, height: 70000, palette: palette(2) }), RangeError);
    assert.throws(() => new GifEncoder({ width: 1, height: 1, palette: palette(1) }), RangeError);
    assert.throws(() => new GifEncoder({ width: 1, height: 1, palette: new Uint8Array(257 * 3) }), RangeError);
    assert.throws(() => new GifEncoder({ width: 1, height: 1, palette: new Uint8Array(7) }), RangeError);
    const enc = new GifEncoder({ width: 4, height: 4, palette: palette(4) });
    assert.throws(() => enc.addFrame({ x: 3, y: 0, width: 2, height: 1, pixels: new Uint8Array(2) }), RangeError);
    assert.throws(() => enc.addFrame({ width: 2, height: 2, pixels: new Uint8Array(3) }), RangeError);
    assert.throws(() => enc.addFrame({ width: 1, height: 1, pixels: Uint8Array.from([4]) }), RangeError);
    enc.addFrame({ width: 1, height: 1, pixels: Uint8Array.from([3]) });
    enc.finish();
    assert.throws(() => enc.finish(), /finished/);
    assert.throws(() => enc.addFrame({ width: 1, height: 1, pixels: new Uint8Array(1) }), /finished/);
});
