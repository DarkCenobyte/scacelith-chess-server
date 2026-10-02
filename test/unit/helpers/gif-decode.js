// A small GIF decoder for the tests of src/gif/ (written from the GIF89a specification, sharing
// nothing with the encoder): header, logical screen, global colour table, extensions (graphic
// control, NETSCAPE2.0 loop), image descriptors, LZW, and the composition of the frames on the
// logical screen (disposal "keep" and transparency), strict about the format (the end-of-information
// code must come at the code width a decoder expects, followed only by the zero padding bits).

/**
 * @param {Uint8Array} buf
 * @returns {{ width: number, height: number, palette: Uint8Array, background: number, loop: number|null,
 *   frames: Array<{ x: number, y: number, width: number, height: number, pixels: Uint8Array, delayCs: number,
 *   disposal: number, transparentIndex: number|null, minCodeSize: number, clearCodes: number }> }}
 */
export function decodeGif(buf) {
    let p = 0;
    const need = (n) => {
        if (p + n > buf.length) throw new Error(`truncated GIF at ${p}`);
    };
    const u8 = () => { need(1); return buf[p++]; };
    const u16 = () => { need(2); const v = buf[p] | (buf[p + 1] << 8); p += 2; return v; };
    const ascii = (n) => { need(n); const s = String.fromCharCode(...buf.subarray(p, p + n)); p += n; return s; };
    const subBlocks = () => {
        const parts = [];
        for (;;) {
            const n = u8();
            if (n === 0) break;
            need(n);
            parts.push(buf.subarray(p, p + n));
            p += n;
        }
        const out = new Uint8Array(parts.reduce((a, b) => a + b.length, 0));
        let o = 0;
        for (const x of parts) { out.set(x, o); o += x.length; }
        return out;
    };

    if (ascii(6) !== 'GIF89a') throw new Error('not a GIF89a file');
    const width = u16(), height = u16();
    const flags = u8();
    const background = u8();
    u8();   // aspect
    let palette = new Uint8Array(0);
    if (flags & 0x80) {
        const n = 3 << ((flags & 7) + 1);
        need(n);
        palette = buf.slice(p, p + n);
        p += n;
    }
    let loop = null;
    let gce = null;
    const frames = [];
    for (;;) {
        const b = u8();
        if (b === 0x3b) break;
        if (b === 0x21) {
            const label = u8();
            if (label === 0xf9) {
                if (u8() !== 4) throw new Error('bad graphic control extension');
                const f = u8();
                const delayCs = u16();
                const ti = u8();
                if (u8() !== 0) throw new Error('graphic control extension not terminated');
                gce = { disposal: (f >> 2) & 7, delayCs, transparentIndex: f & 1 ? ti : null };
            } else if (label === 0xff) {
                const n = u8();
                const id = ascii(n);
                const data = subBlocks();
                if (id === 'NETSCAPE2.0' && data.length === 3 && data[0] === 1) loop = data[1] | (data[2] << 8);
            } else {
                subBlocks();
            }
            continue;
        }
        if (b !== 0x2c) throw new Error(`unexpected block 0x${b.toString(16)} at ${p - 1}`);
        const x = u16(), y = u16(), w = u16(), h = u16();
        const f = u8();
        if (f & 0x80) throw new Error('local colour tables are not expected');
        if (f & 0x40) throw new Error('interlacing is not expected');
        if (x + w > width || y + h > height) throw new Error('frame outside the logical screen');
        const minCodeSize = u8();
        const { pixels, clearCodes } = lzwDecode(subBlocks(), minCodeSize, w * h);
        frames.push({ x, y, width: w, height: h, pixels, minCodeSize, clearCodes,
            delayCs: gce ? gce.delayCs : 0, disposal: gce ? gce.disposal : 0, transparentIndex: gce ? gce.transparentIndex : null });
        gce = null;
    }
    if (p !== buf.length) throw new Error('bytes after the trailer');
    return { width, height, palette, background, loop, frames };
}

/** GIF LZW decoding (variable-length codes, clear and end-of-information codes). */
export function lzwDecode(data, minCodeSize, expected) {
    if (minCodeSize < 2 || minCodeSize > 8) throw new Error(`bad minimum code size ${minCodeSize}`);
    const clear = 1 << minCodeSize, eoi = clear + 1;
    const prefix = new Int32Array(4096), suffix = new Uint8Array(4096), length = new Int32Array(4096);
    for (let i = 0; i < clear; i++) { prefix[i] = -1; suffix[i] = i; length[i] = 1; }
    let size = minCodeSize + 1, next = eoi + 1, prev = -1;
    let bitPos = 0;
    const out = new Uint8Array(expected);
    let o = 0;
    let clearCodes = 0;
    const read = () => {
        let v = 0;
        for (let i = 0; i < size; i++, bitPos++) {
            const byte = bitPos >> 3;
            if (byte >= data.length) throw new Error('LZW data ended before the end-of-information code');
            v |= ((data[byte] >> (bitPos & 7)) & 1) << i;
        }
        return v;
    };
    const write = (code) => {
        const n = length[code];
        if (o + n > expected) throw new Error('LZW data longer than the image');
        let c = code;
        for (let i = n - 1; i >= 0; i--) { out[o + i] = suffix[c]; c = prefix[c]; }
        o += n;
    };
    const first = (code) => { let c = code; while (prefix[c] >= 0) c = prefix[c]; return suffix[c]; };
    for (;;) {
        const code = read();
        if (code === clear) {
            clearCodes++;
            size = minCodeSize + 1;
            next = eoi + 1;
            prev = -1;
            continue;
        }
        if (code === eoi) {
            // Strict end: the end-of-information code was read at the width the decoder expects,
            // and only the zero bits padding its last byte follow it.
            if (Math.ceil(bitPos / 8) !== data.length) throw new Error('LZW data goes on after the end-of-information code');
            if ((bitPos & 7) && (data[data.length - 1] >> (bitPos & 7)) !== 0) throw new Error('non-zero padding after the end-of-information code');
            break;
        }
        if (prev === -1) {
            if (code >= clear) throw new Error(`first code after a clear is ${code}`);
            write(code);
            prev = code;
            continue;
        }
        let k;
        if (code < next) {
            write(code);
            k = first(code);
        } else if (code === next) {
            k = first(prev);
            // KwKwK: the new string is prev + its first symbol.
            if (next < 4096) { prefix[next] = prev; suffix[next] = k; length[next] = length[prev] + 1; }
            write(next);
            if (next < 4096) next++;
            prev = code;
            if (next === (1 << size) && size < 12) size++;
            continue;
        } else {
            throw new Error(`LZW code ${code} beyond the table (${next})`);
        }
        if (next < 4096) {
            prefix[next] = prev;
            suffix[next] = k;
            length[next] = length[prev] + 1;
            next++;
        }
        if (next === (1 << size) && size < 12) size++;
        prev = code;
    }
    if (o !== expected) throw new Error(`LZW data gives ${o} pixels, ${expected} expected`);
    return { pixels: out, clearCodes };
}

/**
 * Composes the frames on the logical screen (disposal 0/1: keep; transparency).
 * @returns {Uint8Array[]} the palette indices of the whole screen after each frame
 */
export function composeFrames(gif) {
    const screen = new Uint8Array(gif.width * gif.height).fill(gif.background);
    const out = [];
    for (const f of gif.frames) {
        if (f.disposal > 1) throw new Error(`disposal ${f.disposal} not expected`);
        for (let y = 0; y < f.height; y++) {
            for (let x = 0; x < f.width; x++) {
                const v = f.pixels[y * f.width + x];
                if (v !== f.transparentIndex) screen[(f.y + y) * gif.width + f.x + x] = v;
            }
        }
        out.push(screen.slice());
    }
    return out;
}
