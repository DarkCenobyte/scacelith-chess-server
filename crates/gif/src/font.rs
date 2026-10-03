//! Bitmap fonts: crisp one-bit glyphs, no anti-aliasing (every text pixel is the text colour, so
//! text costs no palette entries). The glyphs are subsets of Terminus Font by Dimitar Toshkov
//! Zhekov (SIL Open Font License 1.1), renamed "Scacelith GIF" as the OFL asks of modified fonts:
//! `assets/fonts/scacelith-gif/*.bdf`, embedded and parsed once per font on first use (font.js of
//! the Node server).
//!
//! Terminus is monospaced; text is set proportionally here (each glyph advances by its ink width
//! plus a gap, digits by the widest digit). Characters the font lacks print as '?'.

use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;

use crate::jsmath;
use crate::text::{js_trim, js_trim_end};

/// The GIF fonts: pixel height and weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FontName {
    N12,
    B12,
    N16,
    B16,
    N24,
    B24,
}

impl FontName {
    fn source(self) -> &'static str {
        match self {
            FontName::N12 => include_str!("../../../assets/fonts/scacelith-gif/sg-12n.bdf"),
            FontName::B12 => include_str!("../../../assets/fonts/scacelith-gif/sg-12b.bdf"),
            FontName::N16 => include_str!("../../../assets/fonts/scacelith-gif/sg-16n.bdf"),
            FontName::B16 => include_str!("../../../assets/fonts/scacelith-gif/sg-16b.bdf"),
            FontName::N24 => include_str!("../../../assets/fonts/scacelith-gif/sg-24n.bdf"),
            FontName::B24 => include_str!("../../../assets/fonts/scacelith-gif/sg-24b.bdf"),
        }
    }
}

/// A BDF file the parser cannot read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BdfError(String);

impl fmt::Display for BdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BDF: {}", self.0)
    }
}

impl std::error::Error for BdfError {}

/// One glyph of a BDF file.
#[derive(Clone, Debug)]
pub(crate) struct BdfGlyph {
    w: u32,
    h: u32,
    xoff: i32,
    yoff: i32,
    dwidth: i32,
    /// One value per row, the `w` leftmost bits of the row.
    rows: Vec<u64>,
}

/// A parsed BDF file (the subset of BDF 2.1 that bitmap fonts like Terminus use).
#[derive(Clone, Debug)]
pub(crate) struct Bdf {
    ascent: i32,
    descent: i32,
    glyphs: Vec<(u32, BdfGlyph)>,
}

/// The integers of the first line that is `<keyword> <n> <n>...` exactly, as many as `count`.
fn line_values(text: &str, keyword: &str, count: usize) -> Option<Vec<i64>> {
    text.split(['\n', '\r']).find_map(|line| {
        let rest = line.strip_prefix(keyword)?.strip_prefix(' ')?;
        let parts: Vec<&str> = rest.split(' ').collect();
        if parts.len() != count {
            return None;
        }
        parts
            .iter()
            .map(|p| {
                let digits = p.strip_prefix('-').unwrap_or(p);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                p.parse::<i64>().ok()
            })
            .collect()
    })
}

/// Parses a BDF font.
pub(crate) fn parse_bdf(text: &str) -> Result<Bdf, BdfError> {
    let one = |kw: &str| line_values(text, kw, 1).map(|v| v[0]);
    let (Some(ascent), Some(descent)) = (one("FONT_ASCENT"), one("FONT_DESCENT")) else {
        return Err(BdfError("FONT_ASCENT / FONT_DESCENT missing".into()));
    };
    let mut glyphs = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("STARTCHAR") {
        let after = &rest[start..];
        let Some(nl) = after.find('\n') else { break };
        let body_and_more = &after[nl + 1..];
        let Some(end) = body_and_more.find("ENDCHAR") else { break };
        let body = &body_and_more[..end];
        rest = &body_and_more[end + "ENDCHAR".len()..];

        let enc = line_values(body, "ENCODING", 1).map(|v| v[0]);
        let bbx = line_values(body, "BBX", 4).filter(|v| v[0] >= 0 && v[1] >= 0);
        let dw = line_values(body, "DWIDTH", 2);
        let bitmap = body.find("BITMAP\n");
        let (Some(enc), Some(bbx), Some(dw), Some(bi)) = (enc, bbx, dw, bitmap) else { continue };
        let Ok(enc) = u32::try_from(enc) else { continue };
        let (w, h) = (bbx[0] as u32, bbx[1] as u32);
        let hex: Vec<&str> = js_trim(&body[bi + 7..]).split('\n').filter(|l| !l.is_empty()).collect();
        if hex.len() != h as usize {
            return Err(BdfError(format!("glyph {enc} has {} rows, {h} expected", hex.len())));
        }
        let rows = hex
            .iter()
            .map(|l| {
                // parseInt(l, 16) of a line of hexadecimal digits.
                let digits = l.bytes().take_while(u8::is_ascii_hexdigit).count();
                let v = u64::from_str_radix(&l[..digits], 16).unwrap_or(0);
                let bits = l.len() as u32 * 4;
                if bits >= w { v >> (bits - w).min(63) } else { v }
            })
            .collect();
        glyphs.push((
            enc,
            BdfGlyph { w, h, xoff: bbx[2] as i32, yoff: bbx[3] as i32, dwidth: dw[0] as i32, rows },
        ));
    }
    Ok(Bdf { ascent: ascent as i32, descent: descent as i32, glyphs })
}

/// A glyph ready to draw.
#[derive(Clone, Debug)]
struct Glyph {
    /// Ink pixels (x from the left ink column, y from the top of the line box), row-major.
    px: Vec<(i32, i32)>,
    /// Width of the ink.
    ink: i32,
    blank: bool,
}

/// An indexed picture: one palette index per pixel, row-major.
#[derive(Debug)]
pub(crate) struct Canvas {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// A bitmap font with proportional text setting.
#[derive(Debug)]
pub(crate) struct BitmapFont {
    /// Pixels between glyphs.
    pub gap: i32,
    /// Advance of a space.
    pub space: i32,
    /// First row of a capital letter, from the top of the line box.
    pub cap_top: i32,
    /// Height of a capital letter.
    pub cap_height: i32,
    /// Advance of a digit without the gap (digits share one width).
    digit: i32,
    glyphs: HashMap<u32, Glyph>,
    #[cfg(test)]
    height: i32,
}

const SPACE: u32 = 0x20;

fn is_digit(cp: u32) -> bool {
    (0x30..=0x39).contains(&cp)
}

impl BitmapFont {
    pub fn new(bdf: &Bdf) -> BitmapFont {
        let height = bdf.ascent + bdf.descent;
        let gap = (jsmath::round(f64::from(height) / 12.0) as i32).max(1);
        let cell = bdf
            .glyphs
            .iter()
            .rev()
            .find(|(cp, _)| *cp == 0x30)
            .map_or_else(|| jsmath::round(f64::from(height) / 2.0) as i32, |(_, g)| g.dwidth);
        let mut glyphs = HashMap::new();
        for (cp, g) in &bdf.glyphs {
            let (mut left, mut right) = (i32::MAX, i32::MIN);
            let mut px = Vec::new();
            for (y, &row) in g.rows.iter().enumerate() {
                for x in 0..g.w {
                    let shift = g.w - 1 - x;
                    if shift < 64 && (row >> shift) & 1 == 1 {
                        let gx = g.xoff + x as i32;
                        // The top of the glyph box is ascent - (yoff + h) below the line top.
                        px.push((gx, bdf.ascent - (g.yoff + g.h as i32) + y as i32));
                        left = left.min(gx);
                        right = right.max(gx);
                    }
                }
            }
            let blank = px.is_empty();
            for p in &mut px {
                p.0 -= left;
            }
            // Later duplicates of an encoding replace earlier ones, as in a JavaScript Map.
            glyphs.insert(*cp, Glyph { px, ink: if blank { 0 } else { right - left + 1 }, blank });
        }
        let space = (jsmath::round(f64::from(cell) / 2.0) as i32).max(2);
        let (mut cap_top, mut cap_bottom) = (0, bdf.ascent - 1);
        if let Some(h) = glyphs.get(&0x48).filter(|g| !g.blank) {
            cap_top = h.px.iter().map(|p| p.1).min().unwrap_or(0);
            cap_bottom = h.px.iter().map(|p| p.1).max().unwrap_or(0);
        }
        let digit = (0x30..=0x39).filter_map(|d| glyphs.get(&d)).map(|g| g.ink).max().unwrap_or(0).max(0);
        BitmapFont {
            gap,
            space,
            cap_top,
            cap_height: cap_bottom - cap_top + 1,
            digit,
            glyphs,
            #[cfg(test)]
            height,
        }
    }

    /// The glyph of a character, else the question mark's.
    fn glyph(&self, cp: u32) -> Option<&Glyph> {
        self.glyphs.get(&cp).or_else(|| self.glyphs.get(&0x3f))
    }

    /// The advance of a character and the gap that follows its ink (0 for a space).
    fn advance(&self, cp: u32, g: Option<&Glyph>) -> (i32, i32) {
        match g {
            Some(g) if cp != SPACE && !g.blank => {
                (if is_digit(cp) { self.digit } else { g.ink } + self.gap, self.gap)
            }
            _ => (self.space, 0),
        }
    }

    /// Width of a text in pixels (without the trailing gap).
    pub fn measure(&self, text: &str) -> i32 {
        let mut w = 0;
        let mut last = 0;
        for c in text.chars() {
            let cp = u32::from(c);
            let (a, gap) = self.advance(cp, self.glyph(cp));
            w += a;
            last = gap;
        }
        (w - last).max(0)
    }

    /// The longest prefix of a text that fits in `max_width`, with an ellipsis when cut; empty
    /// when even one character with the ellipsis does not fit.
    pub fn fit(&self, text: &str, max_width: i32) -> String {
        if self.measure(text) <= max_width {
            return text.to_string();
        }
        let ends: Vec<usize> = text.char_indices().map(|(i, _)| i).skip(1).collect();
        for &end in ends.iter().rev() {
            let t = format!("{}\u{2026}", js_trim_end(&text[..end]));
            if self.measure(&t) <= max_width {
                return t;
            }
        }
        String::new()
    }

    /// Draws a text (one line) clipped to the canvas, `top` being the top of the line box.
    /// Returns the x after the text (after the last glyph's gap).
    pub fn draw(&self, canvas: &mut Canvas, x: i32, top: i32, text: &str, color: u8) -> i32 {
        let (w, h) = (canvas.width as i32, canvas.height as i32);
        let mut cx = x;
        for c in text.chars() {
            let cp = u32::from(c);
            let g = self.glyph(cp);
            if let Some(g) = g
                && !g.blank
                && cp != SPACE
            {
                // Digits are centred in their fixed cell.
                let ox = if is_digit(cp) { cx + ((self.digit - g.ink) >> 1) } else { cx };
                for &(gx, gy) in &g.px {
                    let (px, py) = (ox + gx, top + gy);
                    if px >= 0 && px < w && py >= 0 && py < h {
                        canvas.data[py as usize * canvas.width + px as usize] = color;
                    }
                }
            }
            cx += self.advance(cp, g).0;
        }
        cx
    }
}

static FONTS: LazyLock<HashMap<FontName, BitmapFont>> = LazyLock::new(|| {
    [FontName::N12, FontName::B12, FontName::N16, FontName::B16, FontName::N24, FontName::B24]
        .into_iter()
        .map(|name| {
            (name, BitmapFont::new(&parse_bdf(name.source()).expect("the embedded GIF fonts are valid BDF")))
        })
        .collect()
});

/// One of the GIF fonts (all six are parsed together on first use).
pub(crate) fn gif_font(name: FontName) -> &'static BitmapFont {
    &FONTS[&name]
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [(FontName, i32); 6] = [
        (FontName::N12, 12),
        (FontName::B12, 12),
        (FontName::N16, 16),
        (FontName::B16, 16),
        (FontName::N24, 24),
        (FontName::B24, 24),
    ];

    #[test]
    fn metrics_of_the_fonts() {
        // (gap, space, capTop, capHeight, digit) computed by the Node server.
        let want = [
            (1, 3, 2, 8, 5),
            (1, 3, 2, 8, 5),
            (1, 4, 2, 10, 6),
            (1, 4, 2, 10, 7),
            (2, 6, 4, 15, 9),
            (2, 6, 4, 15, 10),
        ];
        for ((name, _), m) in ALL.into_iter().zip(want) {
            let f = gif_font(name);
            assert_eq!((f.gap, f.space, f.cap_top, f.cap_height, f.digit), m, "{name:?}");
        }
    }

    #[test]
    fn bitmap_fonts_characters_crisp_drawing_measure_and_fit() {
        let mut needed: Vec<u32> = (32..127).collect();
        needed.extend([0xbd, 0x2026, 0xb7]);
        for (name, height) in ALL {
            let f = gif_font(name);
            assert_eq!(f.height, height);
            for c in &needed {
                assert!(f.glyphs.contains_key(c), "{name:?}: U+{c:x}");
            }
            assert!(f.cap_height * 3 > f.height && f.cap_height < f.height);
            // Drawing writes only the colour asked for, inside the canvas.
            let mut canvas = Canvas { width: 300, height: 40, data: vec![0; 300 * 40] };
            let end = f.draw(&mut canvas, 2, 4, "Carlsen 2830 \u{bd}-\u{bd}", 7);
            assert!(end > 2 && end <= 300);
            assert!(canvas.data.iter().all(|&v| v == 0 || v == 7));
            assert!(canvas.data.iter().filter(|&&v| v == 7).count() > 50);
            // Proportional text: "il" is narrower than "MW"; digits share one width.
            assert!(f.measure("il") < f.measure("MW"));
            assert_eq!(f.measure("1111"), f.measure("8888"));
            assert_eq!(f.measure(""), 0);
            let long = "abcdefghijklmnopqrstuvwxyz";
            let cut = f.fit(long, f.measure("abcdefgh"));
            assert!(cut.ends_with('\u{2026}') && f.measure(&cut) <= f.measure("abcdefgh"));
            assert_eq!(f.fit("abc", 1000), "abc");
            assert_eq!(f.fit("abc", 0), "");
            // Unknown characters print as '?'; drawing clips at the canvas edges.
            assert_eq!(f.measure("\u{4e2d}"), f.measure("?"));
            f.draw(&mut canvas, -50, -10, "clipped", 3);
            f.draw(&mut canvas, 290, 30, "clipped", 3);
        }
        assert_eq!(
            parse_bdf("STARTFONT 2.1\n").unwrap_err().to_string(),
            "BDF: FONT_ASCENT / FONT_DESCENT missing"
        );
        let tiny = BitmapFont::new(
            &parse_bdf("FONT_ASCENT 2\nFONT_DESCENT 0\nSTARTCHAR A\nENCODING 65\nDWIDTH 2 0\nBBX 2 2 0 0\nBITMAP\n80\n40\nENDCHAR\n")
                .unwrap(),
        );
        let mut c = Canvas { width: 4, height: 2, data: vec![0; 8] };
        tiny.draw(&mut c, 0, 0, "A", 1);
        assert_eq!(c.data, [1, 0, 0, 0, 0, 1, 0, 0]);
        assert!(parse_bdf("FONT_ASCENT 2\nFONT_DESCENT 0\nSTARTCHAR A\nENCODING 65\nDWIDTH 2 0\nBBX 2 2 0 0\nBITMAP\n80\nENDCHAR\n").is_err());
    }
}
