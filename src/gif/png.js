// Minimal PNG writer (truecolour RGB or RGBA, 8 bits, zlib through node:zlib) for the visual
// review of the game GIFs (tools/gif-sample.js) and the tests: the server itself only sends GIFs.

import { deflateSync } from 'node:zlib';

const CRC_TABLE = (() => {
    const t = new Uint32Array(256);
    for (let n = 0; n < 256; n++) {
        let c = n;
        for (let k = 0; k < 8; k++) c = (c & 1) ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
        t[n] = c >>> 0;
    }
    return t;
})();

function crc32(buf, start, end) {
    let c = 0xffffffff;
    for (let i = start; i < end; i++) c = CRC_TABLE[(c ^ buf[i]) & 255] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
    const b = Buffer.alloc(12 + data.length);
    b.writeUInt32BE(data.length, 0);
    b.write(type, 4, 'latin1');
    data.copy(b, 8);
    b.writeUInt32BE(crc32(b, 4, 8 + data.length), 8 + data.length);
    return b;
}

/**
 * Encodes an image as PNG.
 * @param {number} width
 * @param {number} height
 * @param {Uint8Array} pixels width * height * channels bytes, row-major
 * @param {3|4} [channels] 3 RGB, 4 RGBA (straight alpha)
 * @returns {Buffer}
 */
export function encodePng(width, height, pixels, channels = 3) {
    const stride = width * channels;
    const raw = Buffer.alloc((stride + 1) * height);
    for (let y = 0; y < height; y++) {
        raw[y * (stride + 1)] = 0;
        raw.set(pixels.subarray(y * stride, (y + 1) * stride), y * (stride + 1) + 1);
    }
    const ihdr = Buffer.alloc(13);
    ihdr.writeUInt32BE(width, 0);
    ihdr.writeUInt32BE(height, 4);
    ihdr[8] = 8;
    ihdr[9] = channels === 4 ? 6 : 2;
    return Buffer.concat([
        Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
        chunk('IHDR', ihdr),
        chunk('IDAT', deflateSync(raw, { level: 9 })),
        chunk('IEND', Buffer.alloc(0)),
    ]);
}

/**
 * An indexed image (palette indices) as PNG, scaled by an integer factor (nearest neighbour).
 * @param {number} width
 * @param {number} height
 * @param {Uint8Array} indices width * height
 * @param {Uint8Array} palette r, g, b per entry
 * @param {number} [zoom] integer >= 1
 * @returns {Buffer}
 */
export function indexedToPng(width, height, indices, palette, zoom = 1) {
    const w = width * zoom, h = height * zoom;
    const px = new Uint8Array(w * h * 3);
    for (let y = 0; y < h; y++) {
        const sy = (y / zoom) | 0;
        for (let x = 0; x < w; x++) {
            const p = indices[sy * width + ((x / zoom) | 0)] * 3;
            const o = (y * w + x) * 3;
            px[o] = palette[p];
            px[o + 1] = palette[p + 1];
            px[o + 2] = palette[p + 2];
        }
    }
    return encodePng(w, h, px, 3);
}
