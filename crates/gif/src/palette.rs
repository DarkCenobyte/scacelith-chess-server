//! The palette of the GIFs and the square pictures ("tiles") drawn with it.
//!
//! One global palette of 180 colours, built so that anti-aliased pieces stay smooth on every
//! square colour: each square colour (light, dark, both highlighted) blended with black in 16
//! steps (the pieces' outlines), a 32-step grey ramp, the red glow of a check over both square
//! colours in 32 steps, and the interface colours. Index 0 is the transparent colour of the
//! frames after the first and is never drawn.
//!
//! Every pixel of a square (piece over square colour, maybe over the red glow of a check) is
//! composed in true colour, then mapped to the nearest palette entry. The 6 x 13 tiles of a square
//! size (light / dark, highlighted, in check; empty or one of 12 pieces) are made once and cached
//! for the process.

use std::collections::HashMap;
use std::f64::consts::SQRT_2;
use std::sync::{Arc, LazyLock, OnceLock};

use crate::jsmath;
use crate::pieces::{self, PIECE_CODES};

type Rgb = [f64; 3];

const PAGE: Rgb = [38.0, 36.0, 33.0];
const TEXT: Rgb = [238.0, 234.0, 226.0];
const DIM: Rgb = [168.0, 161.0, 150.0];
const ACCENT: Rgb = [232.0, 176.0, 64.0];
const SWATCH_WHITE: Rgb = [250.0, 248.0, 242.0];
const SWATCH_BLACK: Rgb = [18.0, 17.0, 16.0];
const SWATCH_EDGE: Rgb = [120.0, 114.0, 106.0];
const LIGHT: Rgb = [240.0, 217.0, 181.0];
const DARK: Rgb = [181.0, 136.0, 99.0];
const LIGHT_HI: Rgb = [205.0, 210.0, 106.0];
const DARK_HI: Rgb = [170.0, 162.0, 58.0];
const CHECK_CORE: Rgb = [255.0, 0.0, 0.0];
const CHECK_MID: Rgb = [231.0, 0.0, 0.0];
const BLACK: Rgb = [0.0, 0.0, 0.0];

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Palette indices of the interface colours.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ui {
    pub page: u8,
    pub text: u8,
    pub dim: u8,
    pub accent: u8,
    pub swatch_white: u8,
    pub swatch_black: u8,
    pub swatch_edge: u8,
}

/// The palette: entries, interface indices and the nearest-colour lookup.
#[derive(Debug)]
pub(crate) struct Palette {
    rgb: Vec<[u8; 3]>,
    /// Index of the first registration of each colour (`r << 16 | g << 8 | b`).
    keys: HashMap<i64, u8>,
    pub ui: Ui,
    /// The palette as r, g, b bytes.
    pub flat: Vec<u8>,
}

fn key(r: i64, g: i64, b: i64) -> i64 {
    (r << 16) | (g << 8) | b
}

impl Palette {
    fn build() -> Palette {
        let mut p = Palette {
            // Index 0: the transparent colour of the frames after the first (never drawn).
            rgb: vec![[PAGE[0] as u8, PAGE[1] as u8, PAGE[2] as u8]],
            keys: HashMap::new(),
            ui: Ui { page: 0, text: 0, dim: 0, accent: 0, swatch_white: 0, swatch_black: 0, swatch_edge: 0 },
            flat: Vec::new(),
        };
        p.ui = Ui {
            page: p.add(PAGE),
            text: p.add(TEXT),
            dim: p.add(DIM),
            accent: p.add(ACCENT),
            swatch_white: p.add(SWATCH_WHITE),
            swatch_black: p.add(SWATCH_BLACK),
            swatch_edge: p.add(SWATCH_EDGE),
        };
        let bases = [LIGHT, DARK, LIGHT_HI, DARK_HI];
        for b in bases {
            p.add(b);
        }
        // Piece outlines (black) over each square colour.
        for b in bases {
            for k in 1..16 {
                p.add(mix(b, BLACK, f64::from(k) / 16.0));
            }
        }
        // Grey ramp: black lines on white bodies, #ececec lines on black bodies.
        for k in 0..=32 {
            let v = f64::from(k) * 255.0 / 32.0;
            p.add([v, v, v]);
        }
        p.add([236.0, 236.0, 236.0]);
        // The check glow over both square colours, and black outlines over the red.
        for k in 0..4 {
            p.add(mix(CHECK_CORE, CHECK_MID, f64::from(k) / 4.0));
        }
        for b in [LIGHT, DARK] {
            for k in 0..32 {
                p.add(mix(CHECK_MID, b, f64::from(k) / 32.0));
            }
        }
        for k in 1..8 {
            p.add(mix(CHECK_MID, BLACK, f64::from(k) / 8.0));
        }
        assert!(p.rgb.len() <= 256, "GIF palette too large ({})", p.rgb.len());
        p.flat = p.rgb.iter().flatten().copied().collect();
        p
    }

    /// Registers a colour (rounded); returns the index of its first registration.
    fn add(&mut self, c: Rgb) -> u8 {
        let [r, g, b] = c.map(|v| jsmath::round(v) as i64);
        let k = key(r, g, b);
        if let Some(&i) = self.keys.get(&k) {
            return i;
        }
        let i = self.rgb.len() as u8;
        self.rgb.push([r as u8, g as u8, b as u8]);
        self.keys.insert(k, i);
        i
    }

    /// Number of colours used.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.rgb.len()
    }

    /// Index of the nearest colour (never the transparent index 0): an exact entry, else the
    /// lowest index at the smallest weighted distance 3 dr^2 + 4 dg^2 + 2 db^2.
    pub fn nearest(&self, c: Rgb) -> u8 {
        self.nearest_memo(c, &mut HashMap::new())
    }

    /// [`Palette::nearest`] with a memo of the colours already searched.
    fn nearest_memo(&self, c: Rgb, memo: &mut HashMap<i64, u8>) -> u8 {
        let [r, g, b] = c.map(|v| jsmath::round(v) as i64);
        let k = key(r, g, b);
        if let Some(&i) = self.keys.get(&k).or_else(|| memo.get(&k)) {
            return i;
        }
        let mut best = 0;
        let mut best_d = i64::MAX;
        for (i, e) in self.rgb.iter().enumerate().skip(1) {
            let (dr, dg, db) = (i64::from(e[0]) - r, i64::from(e[1]) - g, i64::from(e[2]) - b);
            let d = 3 * dr * dr + 4 * dg * dg + 2 * db * db;
            if d < best_d {
                best_d = d;
                best = i as u8;
            }
        }
        memo.insert(k, best);
        best
    }
}

static PALETTE: LazyLock<Palette> = LazyLock::new(Palette::build);

/// The palette (built once).
pub(crate) fn palette() -> &'static Palette {
    &PALETTE
}

/// Kind of a square: its colour and state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SquareKind {
    Light = 0,
    Dark = 1,
    LightHighlight = 2,
    DarkHighlight = 3,
    LightCheck = 4,
    DarkCheck = 5,
}

/// The red glow of a king in check at a distance t from the square's centre (1 = a corner):
/// opaque red in the middle fading out. Returns the colour and its alpha.
fn glow_at(t: f64) -> (Rgb, f64) {
    if t <= 0.25 {
        return (mix(CHECK_CORE, CHECK_MID, t / 0.25), 1.0);
    }
    if t >= 0.89 {
        return (BLACK, 0.0);
    }
    let u = (t - 0.25) / 0.64;
    (CHECK_MID, 1.0 - u)
}

/// The picture of one square: palette indices, `size` x `size`, row-major.
fn make_tile(pal: &Palette, memo: &mut HashMap<i64, u8>, size: u32, kind: SquareKind, code: u8) -> Vec<u8> {
    use SquareKind::*;
    let s = size as usize;
    let base = if matches!(kind, Light | LightHighlight | LightCheck) { LIGHT } else { DARK };
    let flat = match kind {
        LightHighlight => LIGHT_HI,
        DarkHighlight => DARK_HI,
        _ => base,
    };
    let check = matches!(kind, LightCheck | DarkCheck);
    let sprite = if code == 0 {
        None
    } else {
        Some(pieces::piece_sprite(code, size).expect("piece codes and tile sizes are valid"))
    };
    let flat_index = pal.nearest(flat);
    let half = f64::from(size) / 2.0;
    let corner = f64::from(size) / SQRT_2;
    let mut t = vec![0u8; s * s];
    for y in 0..s {
        for x in 0..s {
            let i = y * s + x;
            let [mut r, mut g, mut b] = flat;
            if check {
                let (gl, a) = glow_at(jsmath::hypot(x as f64 + 0.5 - half, y as f64 + 0.5 - half) / corner);
                r = gl[0] * a + r * (1.0 - a);
                g = gl[1] * a + g * (1.0 - a);
                b = gl[2] * a + b * (1.0 - a);
            }
            if let Some(sp) = &sprite {
                let a = f64::from(sp[i * 4 + 3]);
                if a > 0.0 {
                    r = f64::from(sp[i * 4]) * 255.0 + r * (1.0 - a);
                    g = f64::from(sp[i * 4 + 1]) * 255.0 + g * (1.0 - a);
                    b = f64::from(sp[i * 4 + 2]) * 255.0 + b * (1.0 - a);
                } else if !check {
                    t[i] = flat_index;
                    continue;
                }
            }
            t[i] = pal.nearest_memo([r, g, b], memo);
        }
    }
    t
}

/// The 6 x 13 tiles of one square size.
#[derive(Debug)]
pub(crate) struct Tiles {
    tiles: Vec<Arc<[u8]>>,
}

impl Tiles {
    fn build(size: u32) -> Tiles {
        use SquareKind::*;
        let pal = palette();
        let mut memo = HashMap::new();
        let mut tiles = Vec::with_capacity(6 * 16);
        for kind in [Light, Dark, LightHighlight, DarkHighlight, LightCheck, DarkCheck] {
            for code in 0..16u8 {
                let used = code == 0 || PIECE_CODES.contains(&code);
                let tile = if used { make_tile(pal, &mut memo, size, kind, code) } else { Vec::new() };
                tiles.push(tile.into());
            }
        }
        Tiles { tiles }
    }

    /// The picture of a square of `kind` holding the piece `code` (0: empty).
    pub fn get(&self, kind: SquareKind, code: u8) -> &[u8] {
        &self.tiles[kind as usize * 16 + usize::from(code & 15)]
    }
}

/// The tiles of a square size, made on first use. Sizes other than the presets' are made on
/// every call.
pub(crate) fn tiles(size: u32) -> Arc<Tiles> {
    static CACHE: [OnceLock<Arc<Tiles>>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let slot = match size {
        32 => &CACHE[0],
        48 => &CACHE[1],
        72 => &CACHE[2],
        _ => return Arc::new(Tiles::build(size)),
    };
    slot.get_or_init(|| Arc::new(Tiles::build(size))).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_order_and_lookup() {
        let p = palette();
        assert_eq!(p.len(), 180);
        assert_eq!(&p.flat[..3], &[0x26, 0x24, 0x21]);
        assert_eq!((p.ui.page, p.ui.text, p.ui.dim, p.ui.accent), (1, 2, 3, 4));
        assert_eq!((p.ui.swatch_white, p.ui.swatch_black, p.ui.swatch_edge), (5, 6, 7));
        assert_eq!(&p.flat[179 * 3..], &[0x1d, 0, 0]);
        // An exact colour maps to its first registration, never to 0.
        assert_eq!(p.nearest(PAGE), 1);
        assert_eq!(p.nearest(LIGHT), 8);
        assert_eq!(p.nearest([239.6, 217.4, 180.5]), 8);
        assert_ne!(p.nearest([12.0, 200.0, 30.0]), 0);
    }
}
