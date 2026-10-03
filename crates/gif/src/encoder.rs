//! GIF89a writer: one global colour table (2 to 256 colours), the NETSCAPE2.0 application
//! extension for looping, and per frame a graphic control extension (delay, disposal method,
//! optional transparent index) followed by an image of a sub-rectangle of the logical screen,
//! LZW-compressed (variable code size from the minimum code size + 1 up to 12 bits, a clear code
//! when the table is full). The bytes are those of the Node server's encoder.
//!
//! ```
//! use scacelith_gif::encoder::{Disposal, Frame, GifEncoder};
//!
//! let palette = [0, 0, 0, 255, 255, 255];
//! let mut enc = GifEncoder::new(2, 1, &palette, Some(0), 0).unwrap();
//! enc.add_frame(&Frame { x: 0, y: 0, width: 2, height: 1, pixels: &[0, 1], delay_cs: 100,
//!     disposal: Disposal::Keep, transparent: None }).unwrap();
//! let gif: Vec<u8> = enc.finish();
//! assert_eq!(&gif[..6], b"GIF89a");
//! ```

use std::cell::RefCell;
use std::fmt;

/// Disposal method of a frame (what happens to it before the next one is drawn).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Disposal {
    /// No disposal specified.
    None = 0,
    /// The frame stays (the renderer's frames build on each other).
    #[default]
    Keep = 1,
    /// The frame's rectangle is cleared to the background.
    Background = 2,
    /// The screen is restored to what it was before the frame.
    Previous = 3,
}

/// A frame: an image of a sub-rectangle of the logical screen.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    /// Left edge on the logical screen.
    pub x: u16,
    /// Top edge on the logical screen.
    pub y: u16,
    /// Width of the image.
    pub width: u16,
    /// Height of the image.
    pub height: u16,
    /// `width * height` palette indices, row-major.
    pub pixels: &'a [u8],
    /// How long the frame shows, in centiseconds.
    pub delay_cs: u16,
    /// What happens to the frame before the next one.
    pub disposal: Disposal,
    /// The palette index shown as transparent, if any.
    pub transparent: Option<u8>,
}

/// An invalid argument of the encoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The logical screen is empty.
    BadSize,
    /// The palette is not 2..256 RGB triples.
    BadPalette,
    /// The frame rectangle is empty or leaves the logical screen.
    BadRectangle,
    /// The pixel count is not width * height.
    PixelCount,
    /// A pixel index is beyond the colour table.
    PixelIndex(u8),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::BadSize => f.write_str("bad GIF size"),
            EncodeError::BadPalette => f.write_str("the palette needs 2..256 RGB colours"),
            EncodeError::BadRectangle => f.write_str("bad frame rectangle"),
            EncodeError::PixelCount => f.write_str("frame pixels do not match its size"),
            EncodeError::PixelIndex(i) => write!(f, "pixel index {i} out of the palette"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Writes a GIF89a animation frame by frame.
#[derive(Debug)]
pub struct GifEncoder {
    out: Vec<u8>,
    width: u16,
    height: u16,
    min_code_size: u8,
    frames: usize,
}

impl GifEncoder {
    /// Starts a GIF: the header, the logical screen, the global colour table (the palette padded
    /// to a power of two) and, when `repeat` is given, the loop extension (0 = forever).
    pub fn new(
        width: u16,
        height: u16,
        palette: &[u8],
        repeat: Option<u16>,
        background: u8,
    ) -> Result<Self, EncodeError> {
        if width == 0 || height == 0 {
            return Err(EncodeError::BadSize);
        }
        let colors = palette.len() / 3;
        if !(2..=256).contains(&colors) || palette.len() != colors * 3 {
            return Err(EncodeError::BadPalette);
        }
        let mut bits_per_color = 1u8;
        while (1usize << bits_per_color) < colors {
            bits_per_color += 1;
        }
        let mut out = Vec::with_capacity((usize::from(width) * usize::from(height) / 4).max(1 << 16));
        out.extend_from_slice(b"GIF89a");
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        // Global colour table present, colour resolution 8 bits, not sorted, table size.
        out.push(0x80 | (7 << 4) | (bits_per_color - 1));
        out.push(background);
        out.push(0); // pixel aspect ratio: unspecified (square)
        out.extend_from_slice(palette);
        out.resize(out.len() + (3usize << bits_per_color) - palette.len(), 0);
        if let Some(repeat) = repeat {
            out.extend_from_slice(&[0x21, 0xFF, 11]);
            out.extend_from_slice(b"NETSCAPE2.0");
            out.extend_from_slice(&[3, 1]);
            out.extend_from_slice(&repeat.to_le_bytes());
            out.push(0);
        }
        Ok(GifEncoder { out, width, height, min_code_size: bits_per_color.max(2), frames: 0 })
    }

    /// Number of frames added.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.out.len()
    }

    /// Whether nothing was written (never: the header is written at once).
    pub fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    /// Adds a frame.
    pub fn add_frame(&mut self, f: &Frame<'_>) -> Result<(), EncodeError> {
        if f.width == 0
            || f.height == 0
            || u32::from(f.x) + u32::from(f.width) > u32::from(self.width)
            || u32::from(f.y) + u32::from(f.height) > u32::from(self.height)
        {
            return Err(EncodeError::BadRectangle);
        }
        if f.pixels.len() != usize::from(f.width) * usize::from(f.height) {
            return Err(EncodeError::PixelCount);
        }
        let max = 1u16 << self.min_code_size;
        if max < 256
            && let Some(&bad) = f.pixels.iter().find(|&&p| u16::from(p) >= max)
        {
            return Err(EncodeError::PixelIndex(bad));
        }
        let out = &mut self.out;
        out.extend_from_slice(&[
            0x21,
            0xF9,
            4,
            ((f.disposal as u8 & 7) << 2) | u8::from(f.transparent.is_some()),
        ]);
        out.extend_from_slice(&f.delay_cs.to_le_bytes());
        out.extend_from_slice(&[f.transparent.unwrap_or(0), 0, 0x2C]);
        for v in [f.x, f.y, f.width, f.height] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.push(0); // no local colour table, not interlaced
        lzw(out, f.pixels, self.min_code_size, f.transparent);
        self.frames += 1;
        Ok(())
    }

    /// Ends the GIF (the trailer) and returns its bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.out.push(0x3B);
        self.out
    }
}

const MAX_CODE: u16 = 4096;

/// The LZW dictionary: per (prefix code << 8 | pixel), `generation << 12 | code`. A new
/// generation empties it at no cost (a clear code, a new frame).
struct LzwTable {
    entries: Box<[u32]>,
    generation: u32,
    /// `chain[len]`: the code of the string of `len` run symbols (see [`lzw`]).
    chain: Box<[u16]>,
}

impl LzwTable {
    fn new() -> LzwTable {
        LzwTable {
            entries: vec![0; usize::from(MAX_CODE) << 8].into_boxed_slice(),
            generation: 0,
            chain: vec![0; usize::from(MAX_CODE) + 1].into_boxed_slice(),
        }
    }

    /// Empties the table; returns the tag of the new generation.
    fn clear(&mut self) -> u32 {
        self.generation += 1;
        if self.generation >= 1 << 20 {
            self.entries.fill(0);
            self.generation = 1;
        }
        self.generation << 12
    }
}

thread_local! {
    /// One dictionary per thread (4 MiB), reused by every frame encoded on it.
    static TABLE: RefCell<Option<LzwTable>> = const { RefCell::new(None) };
}

/// Packs codes LSB first into data sub-blocks of up to 255 bytes.
struct CodeWriter<'a> {
    out: &'a mut Vec<u8>,
    block: [u8; 255],
    block_len: usize,
    bits: u32,
    nbits: u32,
}

impl CodeWriter<'_> {
    fn emit(&mut self, code: u16, size: u32) {
        self.bits |= u32::from(code) << self.nbits;
        self.nbits += size;
        while self.nbits >= 8 {
            self.push(self.bits as u8);
            self.bits >>= 8;
            self.nbits -= 8;
        }
    }

    fn push(&mut self, byte: u8) {
        self.block[self.block_len] = byte;
        self.block_len += 1;
        if self.block_len == 255 {
            self.flush_block();
        }
    }

    fn flush_block(&mut self) {
        if self.block_len > 0 {
            self.out.push(self.block_len as u8);
            self.out.extend_from_slice(&self.block[..self.block_len]);
            self.block_len = 0;
        }
    }

    /// Writes the last partial byte, the last sub-block and the block terminator.
    fn finish(mut self) {
        if self.nbits > 0 {
            self.push(self.bits as u8);
        }
        self.flush_block();
        self.out.push(0);
    }
}

/// How many bytes at the start of `s` equal `k` (16 at a time, which the compiler vectorizes).
fn run_length(s: &[u8], k: u8) -> usize {
    let full = s.as_chunks::<16>().0.iter().take_while(|c| **c == [k; 16]).count() * 16;
    full + s[full..].iter().take_while(|&&p| p == k).count()
}

/// LZW-compresses palette indices (each below 2 ** min_code_size) into GIF image data: the
/// minimum code size, then the data sub-blocks. Greedy LZW (longest match), with the code-size
/// rules of GIF decoders: the width grows before a code that needs it is assigned, and the end
/// code takes the width a decoder expects after it added the entry of the last data code.
///
/// Fast path for long runs of `run_symbol` (the transparent index of a frame that changed
/// little): the codes of the strings of 1, 2, ... L run symbols form a chain in the dictionary,
/// kept in `chain`, so inside a run the longest match is found by counting symbols instead of one
/// (cache-missing) table lookup per pixel. The output is that of plain greedy LZW.
fn lzw(out: &mut Vec<u8>, pixels: &[u8], min_code_size: u8, run_symbol: Option<u8>) {
    TABLE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let table = slot.get_or_insert_with(LzwTable::new);
        let clear_code: u16 = 1 << min_code_size;
        let eoi = clear_code + 1;
        let first_size = u32::from(min_code_size) + 1;
        let mut code_size = first_size;
        let mut next = eoi + 1;
        let mut generation = table.clear();
        // chain[1..=chain_len] are the codes of 1..=chain_len run symbols.
        let mut chain_len = 1;
        if let Some(r) = run_symbol {
            table.chain[1] = u16::from(r);
        }
        out.push(min_code_size);
        let mut w = CodeWriter { out, block: [0; 255], block_len: 0, bits: 0, nbits: 0 };
        w.emit(clear_code, code_size);
        if let Some(&first) = pixels.first() {
            let mut prefix = u16::from(first);
            // The prefix is chain[run] when it is a string of run symbols, else run is 0.
            let mut run = usize::from(Some(first) == run_symbol);
            let mut i = 1;
            while let Some(&k) = pixels.get(i) {
                if run > 0 && run < chain_len && Some(k) == run_symbol {
                    // Inside a run: jump along the chain.
                    let lim = pixels.len().min(i + (chain_len - run));
                    let len = 1 + run_length(&pixels[i + 1..lim], k);
                    run += len;
                    prefix = table.chain[run];
                    i += len;
                    continue;
                }
                i += 1;
                let key = usize::from(prefix) << 8 | usize::from(k);
                let v = table.entries[key];
                if v & !0xFFF == generation {
                    prefix = (v & 0xFFF) as u16;
                    run = 0;
                    continue;
                }
                w.emit(prefix, code_size);
                if next == MAX_CODE {
                    w.emit(clear_code, code_size);
                    next = eoi + 1;
                    code_size = first_size;
                    generation = table.clear();
                    chain_len = 1;
                } else {
                    if u32::from(next) >= 1 << code_size {
                        code_size += 1;
                    }
                    if run > 0 && run == chain_len && Some(k) == run_symbol {
                        chain_len += 1;
                        table.chain[chain_len] = next;
                    }
                    table.entries[key] = generation | u32::from(next);
                    next += 1;
                }
                prefix = u16::from(k);
                run = usize::from(Some(k) == run_symbol);
            }
            w.emit(prefix, code_size);
            if u32::from(next) == 1 << code_size && code_size < 12 {
                code_size += 1;
            }
        }
        w.emit(eoi, code_size);
        w.finish();
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A frame decoded by [`decode`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct Decoded {
        pub x: u16,
        pub y: u16,
        pub width: u16,
        pub height: u16,
        pub pixels: Vec<u8>,
        pub delay_cs: u16,
        pub disposal: u8,
        pub transparent: Option<u8>,
        pub min_code_size: u8,
        pub clear_codes: usize,
    }

    /// A decoded GIF.
    #[derive(Debug)]
    pub struct Gif {
        pub width: u16,
        pub height: u16,
        pub palette: Vec<u8>,
        pub background: u8,
        pub repeat: Option<u16>,
        pub frames: Vec<Decoded>,
    }

    /// A strict GIF decoder written from the specification (the Node tests' gif-decode.js): the
    /// end code must come at the width a decoder expects, followed only by zero padding bits.
    pub fn decode(buf: &[u8]) -> Result<Gif, String> {
        // Declared before the reading macros, which use it; set once the signature is checked.
        let mut p: usize;
        let need = |p: usize, n: usize| {
            if p + n > buf.len() { Err(format!("truncated GIF at {p}")) } else { Ok(()) }
        };
        macro_rules! u8_ {
            () => {{
                need(p, 1)?;
                p += 1;
                buf[p - 1]
            }};
        }
        macro_rules! u16_ {
            () => {{
                need(p, 2)?;
                p += 2;
                u16::from_le_bytes([buf[p - 2], buf[p - 1]])
            }};
        }
        let sub_blocks = |p: &mut usize| -> Result<Vec<u8>, String> {
            let mut out = Vec::new();
            loop {
                need(*p, 1)?;
                let n = usize::from(buf[*p]);
                *p += 1;
                if n == 0 {
                    return Ok(out);
                }
                need(*p, n)?;
                out.extend_from_slice(&buf[*p..*p + n]);
                *p += n;
            }
        };
        need(0, 6)?;
        if &buf[..6] != b"GIF89a" {
            return Err("not a GIF89a file".into());
        }
        p = 6;
        let width = u16_!();
        let height = u16_!();
        let flags = u8_!();
        let background = u8_!();
        let _aspect = u8_!();
        let mut palette = Vec::new();
        if flags & 0x80 != 0 {
            let n = 3usize << ((flags & 7) + 1);
            need(p, n)?;
            palette = buf[p..p + n].to_vec();
            p += n;
        }
        let mut repeat = None;
        let mut gce: Option<(u8, u16, Option<u8>)> = None;
        let mut frames = Vec::new();
        loop {
            match u8_!() {
                0x3B => break,
                0x21 => {
                    let label = u8_!();
                    if label == 0xF9 {
                        if u8_!() != 4 {
                            return Err("bad graphic control extension".into());
                        }
                        let f = u8_!();
                        let delay = u16_!();
                        let ti = u8_!();
                        if u8_!() != 0 {
                            return Err("graphic control extension not terminated".into());
                        }
                        gce = Some(((f >> 2) & 7, delay, if f & 1 != 0 { Some(ti) } else { None }));
                    } else if label == 0xFF {
                        let n = usize::from(u8_!());
                        need(p, n)?;
                        let id = buf[p..p + n].to_vec();
                        p += n;
                        let data = sub_blocks(&mut p)?;
                        if id == b"NETSCAPE2.0" && data.len() == 3 && data[0] == 1 {
                            repeat = Some(u16::from_le_bytes([data[1], data[2]]));
                        }
                    } else {
                        sub_blocks(&mut p)?;
                    }
                }
                0x2C => {
                    let (x, y, w, h) = (u16_!(), u16_!(), u16_!(), u16_!());
                    let f = u8_!();
                    if f & 0xC0 != 0 {
                        return Err("local colour tables and interlacing are not expected".into());
                    }
                    if u32::from(x) + u32::from(w) > u32::from(width)
                        || u32::from(y) + u32::from(h) > u32::from(height)
                    {
                        return Err("frame outside the logical screen".into());
                    }
                    let min_code_size = u8_!();
                    let data = sub_blocks(&mut p)?;
                    let (pixels, clear_codes) =
                        lzw_decode(&data, min_code_size, usize::from(w) * usize::from(h))?;
                    let (disposal, delay_cs, transparent) = gce.take().unwrap_or((0, 0, None));
                    frames.push(Decoded {
                        x,
                        y,
                        width: w,
                        height: h,
                        pixels,
                        delay_cs,
                        disposal,
                        transparent,
                        min_code_size,
                        clear_codes,
                    });
                }
                b => return Err(format!("unexpected block 0x{b:x} at {}", p - 1)),
            }
        }
        if p != buf.len() {
            return Err("bytes after the trailer".into());
        }
        Ok(Gif { width, height, palette, background, repeat, frames })
    }

    /// The string of `code` appended to `out` at `o`.
    fn write_string(
        out: &mut [u8],
        o: &mut usize,
        table: (&[i32], &[u8], &[usize]),
        code: usize,
    ) -> Result<(), String> {
        let (prefix, suffix, length) = table;
        let n = length[code];
        if *o + n > out.len() {
            return Err("LZW data longer than the image".into());
        }
        let mut c = code;
        for i in (0..n).rev() {
            out[*o + i] = suffix[c];
            c = prefix[c].max(0) as usize;
        }
        *o += n;
        Ok(())
    }

    fn lzw_decode(data: &[u8], min_code_size: u8, expected: usize) -> Result<(Vec<u8>, usize), String> {
        if !(2..=8).contains(&min_code_size) {
            return Err(format!("bad minimum code size {min_code_size}"));
        }
        let clear = 1usize << min_code_size;
        let eoi = clear + 1;
        let mut prefix = vec![-1i32; 4096];
        let mut suffix = vec![0u8; 4096];
        let mut length = vec![0usize; 4096];
        for i in 0..clear {
            suffix[i] = i as u8;
            length[i] = 1;
        }
        let (mut size, mut next, mut prev) = (usize::from(min_code_size) + 1, eoi + 1, None::<usize>);
        let mut bit_pos = 0usize;
        let mut out = vec![0u8; expected];
        let mut o = 0usize;
        let mut clear_codes = 0;
        let read = |bit_pos: &mut usize, size: usize| -> Result<usize, String> {
            let mut v = 0;
            for i in 0..size {
                let byte = *bit_pos >> 3;
                if byte >= data.len() {
                    return Err("LZW data ended before the end-of-information code".into());
                }
                v |= ((usize::from(data[byte]) >> (*bit_pos & 7)) & 1) << i;
                *bit_pos += 1;
            }
            Ok(v)
        };
        let first = |prefix: &[i32], suffix: &[u8], code: usize| {
            let mut c = code;
            while prefix[c] >= 0 {
                c = prefix[c] as usize;
            }
            suffix[c]
        };
        loop {
            let code = read(&mut bit_pos, size)?;
            if code == clear {
                clear_codes += 1;
                size = usize::from(min_code_size) + 1;
                next = eoi + 1;
                prev = None;
                continue;
            }
            if code == eoi {
                if bit_pos.div_ceil(8) != data.len() {
                    return Err("LZW data goes on after the end-of-information code".into());
                }
                if bit_pos & 7 != 0 && data[data.len() - 1] >> (bit_pos & 7) != 0 {
                    return Err("non-zero padding after the end-of-information code".into());
                }
                break;
            }
            let Some(pv) = prev else {
                if code >= clear {
                    return Err(format!("first code after a clear is {code}"));
                }
                write_string(&mut out, &mut o, (&prefix, &suffix, &length), code)?;
                prev = Some(code);
                continue;
            };
            let k = if code < next {
                write_string(&mut out, &mut o, (&prefix, &suffix, &length), code)?;
                first(&prefix, &suffix, code)
            } else if code == next {
                let k = first(&prefix, &suffix, pv);
                if next < 4096 {
                    prefix[next] = pv as i32;
                    suffix[next] = k;
                    length[next] = length[pv] + 1;
                }
                write_string(&mut out, &mut o, (&prefix, &suffix, &length), next)?;
                if next < 4096 {
                    next += 1;
                }
                prev = Some(code);
                if next == 1 << size && size < 12 {
                    size += 1;
                }
                continue;
            } else {
                return Err(format!("LZW code {code} beyond the table ({next})"));
            };
            if next < 4096 {
                prefix[next] = pv as i32;
                suffix[next] = k;
                length[next] = length[pv] + 1;
                next += 1;
            }
            if next == 1 << size && size < 12 {
                size += 1;
            }
            prev = Some(code);
        }
        if o != expected {
            return Err(format!("LZW data gives {o} pixels, {expected} expected"));
        }
        Ok((out, clear_codes))
    }

    /// The palette indices of the whole screen after each frame (disposal keep, transparency).
    pub fn compose(gif: &Gif) -> Vec<Vec<u8>> {
        let w = usize::from(gif.width);
        let mut screen = vec![gif.background; w * usize::from(gif.height)];
        gif.frames
            .iter()
            .map(|f| {
                assert!(f.disposal <= 1, "disposal {} not expected", f.disposal);
                for y in 0..usize::from(f.height) {
                    for x in 0..usize::from(f.width) {
                        let v = f.pixels[y * usize::from(f.width) + x];
                        if Some(v) != f.transparent {
                            screen[(usize::from(f.y) + y) * w + usize::from(f.x) + x] = v;
                        }
                    }
                }
                screen.clone()
            })
            .collect()
    }

    /// xorshift32 in 0..1, as the Node tests' `rng`.
    pub struct Rng(u32);

    impl Rng {
        pub fn new(seed: u32) -> Rng {
            Rng(if seed == 0 { 1 } else { seed })
        }

        pub fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            f64::from(self.0) / 4_294_967_296.0
        }

        pub fn below(&mut self, n: usize) -> usize {
            (self.next() * n as f64) as usize
        }
    }

    fn palette(n: usize) -> Vec<u8> {
        (0..n).flat_map(|i| [i as u8, (i * 7) as u8, (255 - i) as u8]).collect()
    }

    fn frame(width: u16, height: u16, pixels: &[u8]) -> Frame<'_> {
        Frame { x: 0, y: 0, width, height, pixels, delay_cs: 0, disposal: Disposal::Keep, transparent: None }
    }

    fn encode_one(w: u16, h: u16, colors: usize, pixels: &[u8], transparent: Option<u8>) -> Vec<u8> {
        let mut enc = GifEncoder::new(w, h, &palette(colors), Some(0), 0).unwrap();
        enc.add_frame(&Frame { transparent, ..frame(w, h, pixels) }).unwrap();
        enc.finish()
    }

    #[test]
    fn header_logical_screen_global_palette_and_loop_extension() {
        let mut enc = GifEncoder::new(5, 3, &palette(3), Some(0), 2).unwrap();
        enc.add_frame(&Frame { delay_cs: 42, ..frame(5, 3, &[1; 15]) }).unwrap();
        let buf = enc.finish();
        assert_eq!(&buf[..6], b"GIF89a");
        assert_eq!(buf[buf.len() - 1], 0x3B);
        let gif = decode(&buf).unwrap();
        assert_eq!((gif.width, gif.height, gif.background, gif.repeat), (5, 3, 2, Some(0)));
        // 3 colours are stored as a table of 4 (power of two), padded with black.
        let mut want = palette(3);
        want.extend([0, 0, 0]);
        assert_eq!(gif.palette, want);
        assert_eq!(gif.frames.len(), 1);
        let f = &gif.frames[0];
        assert_eq!((f.delay_cs, f.disposal, f.transparent, f.min_code_size), (42, 1, None, 2));
        assert_eq!(f.pixels, vec![1; 15]);

        let mut no_loop = GifEncoder::new(1, 1, &palette(2), None, 0).unwrap();
        no_loop.add_frame(&frame(1, 1, &[0])).unwrap();
        assert_eq!(decode(&no_loop.finish()).unwrap().repeat, None);
    }

    #[test]
    fn lzw_round_trip_every_palette_size_random_and_repetitive_pictures_clear_codes() {
        let mut r = Rng::new(12345);
        for colors in [2usize, 3, 4, 5, 16, 17, 100, 128, 129, 256] {
            let noise: Vec<u8> = (0..64 * 64).map(|_| r.below(colors) as u8).collect();
            let stripes: Vec<u8> = (0..300 * 50).map(|i| ((i >> 5) % colors) as u8).collect();
            let run = vec![(colors - 1) as u8; 200 * 200];
            let single = vec![(colors - 1) as u8];
            for (w, pixels) in [(64u16, noise), (300, stripes), (200, run), (1, single)] {
                let h = (pixels.len() / usize::from(w)) as u16;
                let gif = decode(&encode_one(w, h, colors, &pixels, None)).unwrap();
                assert_eq!(gif.frames[0].pixels, pixels, "{colors} colours, {} pixels", pixels.len());
            }
        }
        // Noise fills the 4096-entry table several times: the decoder sees the clear codes.
        let noise: Vec<u8> = (0..256 * 256).map(|_| r.below(256) as u8).collect();
        let gif = decode(&encode_one(256, 256, 256, &noise, None)).unwrap();
        assert_eq!(gif.frames[0].pixels, noise);
        assert!(gif.frames[0].clear_codes > 5, "clear codes: {}", gif.frames[0].clear_codes);
    }

    #[test]
    fn lzw_round_trip_of_transparent_runs_mixed_with_changes() {
        let mut r = Rng::new(99);
        for trial in 0..40 {
            let w = 1 + r.below(400);
            let h = 1 + r.below(300);
            let mut pixels = vec![0u8; w * h];
            let blobs = r.below(12);
            for _ in 0..blobs {
                let (bx, by) = (r.below(w), r.below(h));
                let (bw, bh) = (1 + r.below(60), 1 + r.below(60));
                let flat = r.next() < 0.5;
                for y in by..h.min(by + bh) {
                    for x in bx..w.min(bx + bw) {
                        pixels[y * w + x] = if flat { 7 } else { r.below(256) as u8 };
                    }
                }
            }
            let gif = decode(&encode_one(w as u16, h as u16, 256, &pixels, Some(0))).unwrap();
            assert_eq!(gif.frames[0].pixels, pixels, "trial {trial}: {w}x{h}");
            assert_eq!(gif.frames[0].transparent, Some(0));
            // The run fast path changes nothing in the output (whichever symbol it follows).
            let lzw_of = |run: Option<u8>| {
                let mut out = Vec::new();
                lzw(&mut out, &pixels, 8, run);
                out
            };
            let plain = lzw_of(None);
            assert!(lzw_of(Some(0)) == plain && lzw_of(Some(7)) == plain, "trial {trial}");
        }
        // A long run costs about sqrt(2n) codes.
        let mut enc = GifEncoder::new(600, 600, &palette(256), Some(0), 0).unwrap();
        let before = enc.len();
        enc.add_frame(&Frame { transparent: Some(0), ..frame(600, 600, &vec![0; 600 * 600]) }).unwrap();
        assert!(enc.len() - before < 1500, "{} bytes for 360000 transparent pixels", enc.len() - before);
    }

    #[test]
    fn the_end_code_takes_the_width_the_decoder_expects_after_the_last_code() {
        // The last data code fills the table to 2 ** codeSize: a decoder widens its codes when it
        // adds that entry, so the end code takes one more bit (49 bits, 7 bytes).
        let pixels = [1, 0, 3, 1, 3, 0, 1, 2, 2, 0, 2];
        let buf = encode_one(11, 1, 4, &pixels, None);
        assert_eq!(decode(&buf).unwrap().frames[0].pixels, pixels);
        let at = buf.len() - 1 - 1 - 7 - 1 - 1;
        assert_eq!(&buf[at..], &[2, 7, 0x0C, 0x16, 0x03, 0x21, 0x02, 0x52, 0x00, 0, 0x3B]);
        // Random small pictures of few colours reach that case often: every one decodes strictly.
        let mut r = Rng::new(2024);
        for _ in 0..3000 {
            let colors = 2 + r.below(7);
            let (w, h) = (1 + r.below(40), 1 + r.below(3));
            let px: Vec<u8> = (0..w * h).map(|_| r.below(colors) as u8).collect();
            let gif = decode(&encode_one(w as u16, h as u16, colors, &px, None)).unwrap();
            assert_eq!(gif.frames[0].pixels, px, "{colors} colours, {w}x{h}: {px:?}");
        }
    }

    #[test]
    fn frames_sub_rectangles_delays_disposal_transparency_compose_like_a_viewer() {
        let mut enc = GifEncoder::new(4, 3, &palette(4), Some(0), 0).unwrap();
        enc.add_frame(&Frame { delay_cs: 100, ..frame(4, 3, &[1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3]) })
            .unwrap();
        enc.add_frame(&Frame {
            x: 1,
            y: 1,
            delay_cs: 50,
            transparent: Some(0),
            ..frame(2, 2, &[0, 3, 2, 0])
        })
        .unwrap();
        enc.add_frame(&Frame { x: 3, y: 0, delay_cs: 300, ..frame(1, 1, &[2]) }).unwrap();
        assert_eq!(enc.frames(), 3);
        let gif = decode(&enc.finish()).unwrap();
        let rects: Vec<_> =
            gif.frames.iter().map(|f| (f.x, f.y, f.width, f.height, f.delay_cs, f.transparent)).collect();
        assert_eq!(rects, [(0, 0, 4, 3, 100, None), (1, 1, 2, 2, 50, Some(0)), (3, 0, 1, 1, 300, None)]);
        let screens = compose(&gif);
        assert_eq!(screens[1], [1, 1, 1, 1, 2, 2, 3, 2, 3, 2, 3, 3]);
        assert_eq!(screens[2], [1, 1, 1, 2, 2, 2, 3, 2, 3, 2, 3, 3]);
    }

    #[test]
    fn invalid_arguments_are_refused() {
        assert_eq!(GifEncoder::new(0, 1, &palette(2), Some(0), 0).unwrap_err(), EncodeError::BadSize);
        assert_eq!(GifEncoder::new(1, 1, &palette(1), Some(0), 0).unwrap_err(), EncodeError::BadPalette);
        assert_eq!(GifEncoder::new(1, 1, &[0; 257 * 3], Some(0), 0).unwrap_err(), EncodeError::BadPalette);
        assert_eq!(GifEncoder::new(1, 1, &[0; 7], Some(0), 0).unwrap_err(), EncodeError::BadPalette);
        let mut enc = GifEncoder::new(4, 4, &palette(4), Some(0), 0).unwrap();
        assert_eq!(enc.add_frame(&Frame { x: 3, ..frame(2, 1, &[0, 0]) }), Err(EncodeError::BadRectangle));
        assert_eq!(enc.add_frame(&frame(2, 2, &[0; 3])), Err(EncodeError::PixelCount));
        assert_eq!(enc.add_frame(&frame(1, 1, &[4])), Err(EncodeError::PixelIndex(4)));
        assert_eq!(enc.add_frame(&frame(0, 1, &[])), Err(EncodeError::BadRectangle));
        enc.add_frame(&frame(1, 1, &[3])).unwrap();
        assert_eq!(enc.frames(), 1);
    }
}
