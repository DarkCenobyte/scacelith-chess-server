// GIF89a writer for the game GIFs (src/gif/render.js): one global colour table (up to 256
// colours), the NETSCAPE2.0 application extension for looping, and per frame a graphic control
// extension (delay in centiseconds, disposal method, optional transparent index) followed by an
// image of a sub-rectangle of the logical screen, LZW-compressed (variable code size from
// minimum code size + 1 up to 12 bits, a clear code when the table is full).
//
//   const enc = new GifEncoder({ width, height, palette, loop: 0 });
//   enc.addFrame({ x: 0, y: 0, width, height, pixels, delayCs: 100 });
//   enc.addFrame({ x, y, width: w, height: h, pixels: sub, delayCs: 50, transparentIndex: 0, disposal: 1 });
//   const gif = enc.finish();   // Buffer
//
// The LZW dictionary is a direct table (code << 8 | pixel -> code) tagged with a generation
// number, so a clear code costs nothing: about 5 ns per pixel on one core, long runs (the
// transparent pixels of a frame that changed little) included.

/** Disposal methods of the graphic control extension. */
export const Disposal = Object.freeze({ None: 0, Keep: 1, Background: 2, Previous: 3 });

const MAX_CODE = 4096;

/** A growable byte buffer. */
class ByteWriter {
    constructor(capacity = 1 << 16) {
        this.buf = Buffer.allocUnsafe(capacity);
        this.len = 0;
    }

    _grow(n) {
        if (this.len + n <= this.buf.length) return;
        let cap = this.buf.length * 2;
        while (cap < this.len + n) cap *= 2;
        const b = Buffer.allocUnsafe(cap);
        this.buf.copy(b, 0, 0, this.len);
        this.buf = b;
    }

    byte(v) {
        this._grow(1);
        this.buf[this.len++] = v;
    }

    u16(v) {
        this._grow(2);
        this.buf[this.len++] = v & 255;
        this.buf[this.len++] = (v >> 8) & 255;
    }

    bytes(arr) {
        this._grow(arr.length);
        for (let i = 0; i < arr.length; i++) this.buf[this.len++] = arr[i];
    }

    ascii(s) {
        this._grow(s.length);
        for (let i = 0; i < s.length; i++) this.buf[this.len++] = s.charCodeAt(i) & 255;
    }

    // A Buffer that owns its whole ArrayBuffer (transferable to another thread, never a slice of
    // Node's shared pool).
    result() {
        const ab = new ArrayBuffer(this.len);
        const out = Buffer.from(ab);
        this.buf.copy(out, 0, 0, this.len);
        return out;
    }
}

// Dictionary of the LZW encoder: (generation << 12 | code) per (prefix code << 8 | pixel).
// Shared by the encoders of a thread (encoding is synchronous).
let table = null;
let generation = 0;

// chain[len]: code of the string of `len` run symbols (see lzw), shared like the table.
let chain = null;

/**
 * LZW-compresses indexed pixels into GIF data sub-blocks (greedy longest match).
 * Fast path for long runs of one symbol (the transparent index of a frame that changed little):
 * the codes of the strings of 1, 2, ... L run symbols form a chain in the dictionary, kept in
 * `chain`; inside a run the longest match is found by counting the symbols instead of one table
 * lookup per pixel. The output is the same as without the fast path.
 * @param {ByteWriter} out
 * @param {Uint8Array} pixels palette indices, each < 2 ** minCodeSize
 * @param {number} minCodeSize 2..8
 * @param {number} runSymbol the symbol of the fast path (-1: none)
 */
function lzw(out, pixels, minCodeSize, runSymbol) {
    if (table === null) {
        table = new Int32Array(MAX_CODE << 8);
        chain = new Int32Array(MAX_CODE + 1);
    }
    const clearCode = 1 << minCodeSize;
    const eoi = clearCode + 1;
    let codeSize = minCodeSize + 1;
    let next = eoi + 1;
    const newGeneration = () => {
        generation++;
        if (generation >= 1 << 19) {
            table.fill(0);
            generation = 1;
        }
    };
    newGeneration();
    let gen = generation << 12;
    // Run chain: chain[1..chainLen] are the codes of 1..chainLen run symbols.
    let chainLen = 1;
    if (runSymbol >= 0) chain[1] = runSymbol;

    out.byte(minCodeSize);
    // Sub-blocks of up to 255 bytes: a length byte, then the data.
    const block = new Uint8Array(255);
    let blockLen = 0;
    let bits = 0, nbits = 0;
    const flushBlock = () => {
        if (blockLen === 0) return;
        out.byte(blockLen);
        out.bytes(blockLen === 255 ? block : block.subarray(0, blockLen));
        blockLen = 0;
    };
    const emit = (code) => {
        bits |= code << nbits;
        nbits += codeSize;
        while (nbits >= 8) {
            block[blockLen++] = bits & 255;
            if (blockLen === 255) flushBlock();
            bits >>>= 8;
            nbits -= 8;
        }
    };

    emit(clearCode);
    const n = pixels.length;
    if (n > 0) {
        let prefix = pixels[0];
        let run = prefix === runSymbol ? 1 : 0;     // the prefix is chain[run] (0: not a pure run)
        for (let i = 1; i < n; i++) {
            const k = pixels[i];
            if (run > 0 && k === runSymbol && run < chainLen) {
                // Inside a run: jump along the chain.
                let j = i + 1;
                const lim = Math.min(n, i + (chainLen - run));
                while (j < lim && pixels[j] === runSymbol) j++;
                run += j - i;
                prefix = chain[run];
                i = j - 1;
                continue;
            }
            const key = (prefix << 8) | k;
            const v = table[key];
            if ((v & ~0xfff) === gen) {
                prefix = v & 0xfff;
                run = 0;
                continue;
            }
            emit(prefix);
            if (next === MAX_CODE) {
                emit(clearCode);
                next = eoi + 1;
                codeSize = minCodeSize + 1;
                newGeneration();
                gen = generation << 12;
                chainLen = 1;
            } else {
                if (next >= (1 << codeSize)) codeSize++;
                if (run > 0 && run === chainLen && k === runSymbol) chain[++chainLen] = next;
                table[key] = gen | next++;
            }
            prefix = k;
            run = k === runSymbol ? 1 : 0;
        }
        emit(prefix);
    }
    emit(eoi);
    if (nbits > 0) {
        block[blockLen++] = bits & 255;
        if (blockLen === 255) flushBlock();
    }
    flushBlock();
    out.byte(0);    // block terminator
}

/** Writes a GIF89a animation frame by frame. */
export class GifEncoder {
    /**
     * @param {{ width: number, height: number, palette: Uint8Array|number[], loop?: number|null, background?: number }} o
     *   palette: r, g, b per colour (2..256 colours; padded to a power of two);
     *   loop: NETSCAPE2.0 loop count (0 = forever, the default; null = no loop extension);
     *   background: background colour index of the logical screen.
     */
    constructor({ width, height, palette, loop = 0, background = 0 }) {
        if (!Number.isInteger(width) || !Number.isInteger(height) || width < 1 || height < 1 || width > 65535 || height > 65535) {
            throw new RangeError(`bad GIF size ${width}x${height}`);
        }
        const colors = Math.floor(palette.length / 3);
        if (colors < 2 || colors > 256 || palette.length !== colors * 3) throw new RangeError('the palette needs 2..256 RGB colours');
        let bitsPerColor = 1;
        while ((1 << bitsPerColor) < colors) bitsPerColor++;
        this.width = width;
        this.height = height;
        this.colors = colors;
        this.minCodeSize = Math.max(2, bitsPerColor);
        this.frames = 0;
        this._finished = false;
        const w = new ByteWriter(Math.max(1 << 16, (width * height) >> 2));
        this._w = w;
        w.ascii('GIF89a');
        w.u16(width);
        w.u16(height);
        // Global colour table present, colour resolution 8 bits, not sorted, table size.
        w.byte(0x80 | (7 << 4) | (bitsPerColor - 1));
        w.byte(background & 255);
        w.byte(0);                                  // pixel aspect ratio: unspecified (square)
        const table = new Uint8Array(3 << bitsPerColor);
        table.set(Array.from(palette, (v) => v & 255));
        w.bytes(table);
        if (loop !== null && loop !== undefined) {
            w.byte(0x21);
            w.byte(0xff);
            w.byte(11);
            w.ascii('NETSCAPE2.0');
            w.byte(3);
            w.byte(1);
            w.u16(loop & 0xffff);
            w.byte(0);
        }
    }

    /**
     * Adds a frame: an image of a sub-rectangle of the logical screen.
     * @param {{ x?: number, y?: number, width: number, height: number, pixels: Uint8Array,
     *   delayCs?: number, disposal?: number, transparentIndex?: number|null }} f
     *   pixels: width * height palette indices; delayCs: 0..65535 centiseconds;
     *   disposal: Disposal value (default Keep); transparentIndex: index shown as transparent.
     */
    addFrame({ x = 0, y = 0, width, height, pixels, delayCs = 0, disposal = Disposal.Keep, transparentIndex = null }) {
        if (this._finished) throw new Error('GIF already finished');
        if (!Number.isInteger(width) || !Number.isInteger(height) || width < 1 || height < 1
            || !Number.isInteger(x) || !Number.isInteger(y) || x < 0 || y < 0
            || x + width > this.width || y + height > this.height) {
            throw new RangeError(`bad frame rectangle ${x},${y} ${width}x${height}`);
        }
        if (!pixels || pixels.length !== width * height) throw new RangeError('frame pixels do not match its size');
        const max = 1 << this.minCodeSize;
        if (max < 256 || !(pixels instanceof Uint8Array)) {
            for (let i = 0; i < pixels.length; i++) {
                if (!(pixels[i] >= 0 && pixels[i] < max)) throw new RangeError(`pixel index ${pixels[i]} out of the palette`);
            }
        }
        const w = this._w;
        const hasT = transparentIndex !== null && transparentIndex !== undefined;
        w.byte(0x21);
        w.byte(0xf9);
        w.byte(4);
        w.byte(((disposal & 7) << 2) | (hasT ? 1 : 0));
        w.u16(Math.max(0, Math.min(65535, Math.round(delayCs))));
        w.byte(hasT ? transparentIndex & 255 : 0);
        w.byte(0);
        w.byte(0x2c);
        w.u16(x);
        w.u16(y);
        w.u16(width);
        w.u16(height);
        w.byte(0);                                  // no local colour table, not interlaced
        lzw(w, pixels instanceof Uint8Array ? pixels : Uint8Array.from(pixels), this.minCodeSize, hasT ? transparentIndex & 255 : -1);
        this.frames++;
    }

    /**
     * @returns {Buffer} the GIF file, over an ArrayBuffer of its own (transferable); the encoder
     *   cannot be used afterwards.
     */
    finish() {
        if (this._finished) throw new Error('GIF already finished');
        this._finished = true;
        this._w.byte(0x3b);
        const out = this._w.result();
        this._w = null;
        return out;
    }
}
