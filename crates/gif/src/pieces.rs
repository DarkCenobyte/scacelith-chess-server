//! The chess pieces: the "cburnett" set by Colin M.L. Burnett (GPLv2+, see
//! `assets/pieces/cburnett/LICENSE.md`), embedded as SVG and rasterized anti-aliased at any square
//! size on first use, then cached (pieces.js of the Node server).
//!
//! The SVG reader covers what these files use, and a little more: `<svg viewBox>`, nested `<g>`
//! with inherited presentation attributes (fill, fill-rule, fill-opacity, stroke, stroke-width,
//! stroke-linecap, stroke-linejoin, stroke-miterlimit, stroke-opacity, opacity) given as
//! attributes or in a style attribute, and the shapes `<path>`, `<circle>`, `<ellipse>`, `<rect>`,
//! `<line>`, `<polyline>`, `<polygon>`. Colours: `#rgb`, `#rrggbb`, `black`, `white`, `none`.
//! Shapes are painted in document order, the fill then the stroke of each, source over.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use crate::raster::{self, FillRule, Flatten, LineCap, LineJoin, Stroke};
use crate::text::{js_number, js_trim, split_list};

/// The piece codes (type | colour << 3; type 1 pawn .. 6 king, colour 0 White, 1 Black) in a
/// fixed order.
pub const PIECE_CODES: [u8; 12] = [1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14];

/// Smallest and largest square sizes a sprite is rasterized at (pixels).
pub const SPRITE_SIZES: std::ops::RangeInclusive<u32> = 8..=256;

/// The SVG source of a piece.
fn piece_svg(code: u8) -> Option<&'static str> {
    Some(match code {
        1 => include_str!("../../../assets/pieces/cburnett/wP.svg"),
        2 => include_str!("../../../assets/pieces/cburnett/wN.svg"),
        3 => include_str!("../../../assets/pieces/cburnett/wB.svg"),
        4 => include_str!("../../../assets/pieces/cburnett/wR.svg"),
        5 => include_str!("../../../assets/pieces/cburnett/wQ.svg"),
        6 => include_str!("../../../assets/pieces/cburnett/wK.svg"),
        9 => include_str!("../../../assets/pieces/cburnett/bP.svg"),
        10 => include_str!("../../../assets/pieces/cburnett/bN.svg"),
        11 => include_str!("../../../assets/pieces/cburnett/bB.svg"),
        12 => include_str!("../../../assets/pieces/cburnett/bR.svg"),
        13 => include_str!("../../../assets/pieces/cburnett/bQ.svg"),
        14 => include_str!("../../../assets/pieces/cburnett/bK.svg"),
        _ => return None,
    })
}

/// An SVG document the reader cannot draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvgError(String);

impl fmt::Display for SvgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SvgError {}

/// A colour in 0..1 per channel.
type Rgb = [f64; 3];

/// Parses an SVG colour: `None` for none or transparent.
pub(crate) fn parse_color(s: &str) -> Result<Option<Rgb>, SvgError> {
    let v = js_trim(s).to_ascii_lowercase();
    let hex = |h: &str| u32::from_str_radix(h, 16).ok().filter(|_| h.bytes().all(|b| b.is_ascii_hexdigit()));
    match v.as_str() {
        "none" | "transparent" => return Ok(None),
        "black" => return Ok(Some([0.0, 0.0, 0.0])),
        "white" => return Ok(Some([1.0, 1.0, 1.0])),
        _ => {}
    }
    if let Some(h) = v.strip_prefix('#') {
        if h.len() == 6
            && let Some(n) = hex(h)
        {
            return Ok(Some([
                f64::from(n >> 16) / 255.0,
                f64::from((n >> 8) & 255) / 255.0,
                f64::from(n & 255) / 255.0,
            ]));
        }
        if h.len() == 3 && hex(h).is_some() {
            let channel = |c: char| f64::from(hex(&format!("{c}{c}")).unwrap_or(0)) / 255.0;
            let mut cs = h.chars().map(channel);
            let (r, g, b) = (cs.next(), cs.next(), cs.next());
            if let (Some(r), Some(g), Some(b)) = (r, g, b) {
                return Ok(Some([r, g, b]));
            }
        }
    }
    Err(SvgError(format!("unsupported colour {s}")))
}

/// The presentation attributes a shape inherits from its parents, with SVG's defaults.
const INHERITED: [&str; 9] = [
    "fill",
    "fill-rule",
    "fill-opacity",
    "stroke",
    "stroke-width",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-opacity",
];

const DEFAULT_STYLE: [&str; 9] = ["black", "nonzero", "1", "none", "1", "butt", "miter", "4", "1"];

/// Presentation attribute values, indexed like [`INHERITED`].
type Style = [String; 9];

fn style_value<'a>(style: &'a Style, key: &str) -> &'a str {
    INHERITED.iter().position(|k| *k == key).map_or("", |i| style[i].as_str())
}

/// One shape of a drawing.
#[derive(Clone, Debug)]
pub(crate) struct Shape {
    kind: String,
    attrs: HashMap<String, String>,
    style: Style,
    opacity: f64,
}

/// A drawing: the view box and the shapes in paint order.
#[derive(Clone, Debug)]
pub(crate) struct Drawing {
    view_box: [f64; 4],
    shapes: Vec<Shape>,
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_name_char(b: u8) -> bool {
    is_word(b) || b == b':' || b == b'-'
}

/// ASCII whitespace of a regular expression's `\s` (the SVG files are ASCII).
fn is_space(b: u8) -> bool {
    b.is_ascii_whitespace() || b == 0x0b
}

/// The attributes of a tag (`name="value"`, double quotes only); a `style` attribute's
/// declarations override the attributes.
fn attributes_of(text: &str) -> HashMap<String, String> {
    let b = text.as_bytes();
    let mut attrs = HashMap::new();
    let mut i = 0;
    while i < b.len() {
        if !is_name_char(b[i]) {
            i += 1;
            continue;
        }
        let name_start = i;
        while i < b.len() && is_name_char(b[i]) {
            i += 1;
        }
        let name_end = i;
        let mut j = i;
        while j < b.len() && is_space(b[j]) {
            j += 1;
        }
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && is_space(b[j]) {
                j += 1;
            }
            if j < b.len()
                && b[j] == b'"'
                && let Some(len) = text[j + 1..].find('"')
            {
                attrs.insert(text[name_start..name_end].to_string(), text[j + 1..j + 1 + len].to_string());
                i = j + 1 + len + 1;
            }
        }
    }
    if let Some(style) = attrs.get("style").cloned() {
        for decl in style.split(';') {
            if let Some(k) = decl.find(':')
                && k > 0
            {
                attrs.insert(js_trim(&decl[..k]).to_string(), js_trim(&decl[k + 1..]).to_string());
            }
        }
    }
    attrs
}

/// Removes the comments of an XML text.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        match rest[start + 4..].find("-->") {
            Some(len) => {
                out.push_str(&rest[..start]);
                rest = &rest[start + 4 + len + 3..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// One tag: `<` `/`? name rest `/`? `>`.
struct Tag<'a> {
    closing: bool,
    name: &'a str,
    rest: &'a str,
    self_closing: bool,
}

/// The tags of an XML text, in order (`<(\/?)([a-zA-Z][\w:-]*)([^>]*?)(\/?)>`).
fn tags(text: &str) -> impl Iterator<Item = Tag<'_>> {
    let b = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < b.len() {
            let start = i;
            i += 1;
            if b[start] != b'<' {
                continue;
            }
            let mut j = start + 1;
            let closing = b.get(j) == Some(&b'/');
            if closing {
                j += 1;
            }
            if !b.get(j).is_some_and(u8::is_ascii_alphabetic) {
                continue;
            }
            let name_start = j;
            while j < b.len() && is_name_char(b[j]) {
                j += 1;
            }
            let Some(len) = text[j..].find('>') else { continue };
            let end = j + len;
            let self_closing = end > j && b[end - 1] == b'/';
            let rest_end = if self_closing { end - 1 } else { end };
            i = end + 1;
            return Some(Tag { closing, name: &text[name_start..j], rest: &text[j..rest_end], self_closing });
        }
        None
    })
}

/// Reads the subset of SVG described in the module documentation.
pub(crate) fn parse_svg(text: &str) -> Result<Drawing, SvgError> {
    struct Group {
        style: Style,
        opacity: f64,
    }
    let mut shapes = Vec::new();
    let mut stack = vec![Group { style: DEFAULT_STYLE.map(String::from), opacity: 1.0 }];
    let mut view_box = None;
    let text = strip_comments(text);
    for tag in tags(&text) {
        if tag.closing {
            if (tag.name == "g" || tag.name == "svg") && stack.len() > 1 {
                stack.pop();
            }
            continue;
        }
        let attrs = attributes_of(tag.rest);
        let parent = stack.last().expect("the root group stays on the stack");
        let mut style = parent.style.clone();
        for (k, value) in INHERITED.iter().zip(style.iter_mut()) {
            if let Some(v) = attrs.get(*k)
                && v != "inherit"
            {
                v.clone_into(value);
            }
        }
        let opacity = parent.opacity * attrs.get("opacity").map_or(1.0, |v| js_number(v));
        match tag.name {
            "svg" => {
                let vb: Option<Vec<f64>> = attrs
                    .get("viewBox")
                    .filter(|v| !v.is_empty())
                    .map(|v| split_list(js_trim(v)).into_iter().map(js_number).collect());
                let size = |key: &str| attrs.get(key).map_or(f64::NAN, |v| js_number(v));
                let or_45 = |v: f64| if v.is_nan() || v == 0.0 { 45.0 } else { v };
                view_box = Some(match vb {
                    Some(v) if v.len() == 4 && v.iter().all(|x| x.is_finite()) => [v[0], v[1], v[2], v[3]],
                    _ => [0.0, 0.0, or_45(size("width")), or_45(size("height"))],
                });
                if !tag.self_closing {
                    stack.push(Group { style, opacity });
                }
            }
            "g" => {
                if !tag.self_closing {
                    stack.push(Group { style, opacity });
                }
            }
            "path" | "circle" | "ellipse" | "rect" | "line" | "polyline" | "polygon" => {
                shapes.push(Shape { kind: tag.name.to_string(), attrs, style, opacity });
            }
            _ => {}
        }
    }
    let view_box = view_box.ok_or_else(|| SvgError("not an SVG document".into()))?;
    Ok(Drawing { view_box, shapes })
}

/// `Number(v)` when finite, else `default`.
fn num(v: Option<&str>, default: f64) -> f64 {
    let n = v.map_or(f64::NAN, js_number);
    if n.is_finite() { n } else { default }
}

/// A number as JavaScript prints it into path data; any text that reads back as the same value
/// does (negative zero reads back as zero in JavaScript).
fn js_str(v: f64) -> String {
    if v == 0.0 { "0".into() } else { v.to_string() }
}

/// The geometry of a shape as path data (user units).
fn shape_path(shape: &Shape) -> String {
    let a = &shape.attrs;
    let n = |k: &str| num(a.get(k).map(String::as_str), 0.0);
    match shape.kind.as_str() {
        "path" => a.get("d").cloned().unwrap_or_default(),
        "circle" | "ellipse" => {
            let (cx, cy) = (n("cx"), n("cy"));
            let (rx, ry) = if shape.kind == "circle" { (n("r"), n("r")) } else { (n("rx"), n("ry")) };
            if rx > 0.0 && ry > 0.0 {
                let (rx_s, ry_s) = (js_str(rx), js_str(ry));
                format!(
                    "M{} {cy}A{rx_s} {ry_s} 0 1 1 {} {cy}A{rx_s} {ry_s} 0 1 1 {} {cy}Z",
                    js_str(cx + rx),
                    js_str(cx - rx),
                    js_str(cx + rx),
                    cy = js_str(cy),
                )
            } else {
                String::new()
            }
        }
        "rect" => {
            let (x, y, w, h) = (n("x"), n("y"), n("width"), n("height"));
            if w > 0.0 && h > 0.0 {
                format!("M{} {}h{}v{}h{}Z", js_str(x), js_str(y), js_str(w), js_str(h), js_str(-w))
            } else {
                String::new()
            }
        }
        "line" => format!("M{} {}L{} {}", js_str(n("x1")), js_str(n("y1")), js_str(n("x2")), js_str(n("y2"))),
        "polyline" | "polygon" => {
            let points = a.get("points").map_or("", |s| js_trim(s));
            let p: Vec<String> = split_list(points)
                .into_iter()
                .map(|s| {
                    let v = js_number(s);
                    if v.is_nan() { "NaN".into() } else { js_str(v) }
                })
                .collect();
            if p.len() < 4 {
                return String::new();
            }
            format!("M{}{}", p.join(" "), if shape.kind == "polygon" { "Z" } else { "" })
        }
        _ => String::new(),
    }
}

/// Paints a drawing into a premultiplied RGBA buffer of `size` x `size` pixels (the view box
/// scaled to it).
pub(crate) fn render_drawing(drawing: &Drawing, size: usize) -> Result<Vec<f32>, SvgError> {
    let [vx, vy, vw, vh] = drawing.view_box;
    let sz = size as f64;
    let scale = f64::min(sz / vw, sz / vh);
    let dx = -vx * scale + (sz - vw * scale) / 2.0;
    let dy = -vy * scale + (sz - vh * scale) / 2.0;
    let mut out = vec![0.0f32; size * size * 4];
    let mut cov = vec![0.0f32; size * size];
    let paint = |out: &mut [f32], cov: &[f32], [r, g, b]: Rgb, alpha: f64| {
        for (&c, px) in cov.iter().zip(out.as_chunks_mut::<4>().0) {
            let a = f64::from(c) * alpha;
            if a <= 0.0 {
                continue;
            }
            let k = 1.0 - a;
            px[0] = (r * a + f64::from(px[0]) * k) as f32;
            px[1] = (g * a + f64::from(px[1]) * k) as f32;
            px[2] = (b * a + f64::from(px[2]) * k) as f32;
            px[3] = (a + f64::from(px[3]) * k) as f32;
        }
    };
    for shape in &drawing.shapes {
        let d = shape_path(shape);
        if d.is_empty() {
            continue;
        }
        let st = &shape.style;
        let get = |k: &str| style_value(st, k);
        let cmds = raster::parse_path_data(&d).map_err(|e| SvgError(e.to_string()))?;
        let subpaths = raster::flatten_path(&cmds, Flatten { scale, dx, dy, tol: 0.03 });
        if let Some(fill) = parse_color(get("fill"))? {
            let rule = if get("fill-rule") == "evenodd" { FillRule::EvenOdd } else { FillRule::NonZero };
            raster::rasterize(&raster::polygons_of(&subpaths), rule, size, size, &mut cov);
            paint(&mut out, &cov, fill, shape.opacity * num(Some(get("fill-opacity")), 1.0));
        }
        let stroke = parse_color(get("stroke"))?;
        let width = num(Some(get("stroke-width")), 1.0) * scale;
        if let Some(stroke) = stroke
            && width > 0.0
        {
            let polys = raster::stroke_polygons(
                &subpaths,
                Stroke {
                    width,
                    cap: LineCap::parse(get("stroke-linecap")),
                    join: LineJoin::parse(get("stroke-linejoin")),
                    miter_limit: num(Some(get("stroke-miterlimit")), 4.0),
                    tol: 0.03,
                },
            );
            raster::rasterize(&polys, FillRule::NonZero, size, size, &mut cov);
            paint(&mut out, &cov, stroke, shape.opacity * num(Some(get("stroke-opacity")), 1.0));
        }
    }
    Ok(out)
}

/// An anti-aliased piece: `size` x `size` premultiplied RGBA pixels in 0..1, row-major.
pub type Sprite = Arc<[f32]>;

/// An invalid sprite request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpriteError {
    /// Not one of [`PIECE_CODES`].
    BadCode(u8),
    /// Outside [`SPRITE_SIZES`].
    BadSize(u32),
}

impl fmt::Display for SpriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpriteError::BadCode(c) => write!(f, "bad piece code {c}"),
            SpriteError::BadSize(s) => write!(f, "bad sprite size {s}"),
        }
    }
}

impl std::error::Error for SpriteError {}

static DRAWINGS: LazyLock<HashMap<u8, Drawing>> = LazyLock::new(|| {
    PIECE_CODES
        .iter()
        .map(|&code| {
            let svg = piece_svg(code).expect("every piece code has an SVG");
            (code, parse_svg(svg).expect("the embedded piece SVGs are valid"))
        })
        .collect()
});

static SPRITES: LazyLock<Mutex<HashMap<(u8, u32), Sprite>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The sprite of a piece at a square size (rasterized once, then cached for the process).
pub fn piece_sprite(code: u8, size: u32) -> Result<Sprite, SpriteError> {
    if piece_svg(code).is_none() {
        return Err(SpriteError::BadCode(code));
    }
    if !SPRITE_SIZES.contains(&size) {
        return Err(SpriteError::BadSize(size));
    }
    let cached = SPRITES.lock().unwrap_or_else(PoisonError::into_inner).get(&(code, size)).cloned();
    if let Some(s) = cached {
        return Ok(s);
    }
    // Rasterized outside the lock: another thread may do the same work, with the same result.
    let pixels = render_drawing(&DRAWINGS[&code], size as usize).expect("the embedded piece SVGs draw");
    let sprite: Sprite = pixels.into();
    let mut map = SPRITES.lock().unwrap_or_else(PoisonError::into_inner);
    Ok(map.entry((code, size)).or_insert(sprite).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_subset_inheritance_colours_shapes() {
        assert_eq!(parse_color("#fff").unwrap(), Some([1.0, 1.0, 1.0]));
        assert_eq!(parse_color("#000000").unwrap(), Some([0.0, 0.0, 0.0]));
        assert_eq!(parse_color(" None ").unwrap(), None);
        assert!((parse_color("#ececec").unwrap().unwrap()[0] - 236.0 / 255.0).abs() < 1e-9);
        assert!(parse_color("red").is_err());
        assert!(parse_color("#ggg").is_err());
        let d = parse_svg(
            "<svg viewBox=\"0 0 10 10\"><!-- <g fill=\"red\"> --><g fill=\"#fff\" stroke=\"#000\" stroke-width=\"2\">\
             <g style=\"stroke-width:1\"><path d=\"M0 0h1\"/></g><circle cx=\"5\" cy=\"5\" r=\"2\" fill=\"none\"/></g>\
             <rect x=\"1\" y=\"1\" width=\"2\" height=\"2\"/></svg>",
        )
        .unwrap();
        assert_eq!(d.view_box, [0.0, 0.0, 10.0, 10.0]);
        assert_eq!(d.shapes.len(), 3);
        assert_eq!(style_value(&d.shapes[0].style, "fill"), "#fff");
        assert_eq!(style_value(&d.shapes[0].style, "stroke-width"), "1");
        assert_eq!(style_value(&d.shapes[1].style, "fill"), "none");
        assert_eq!(style_value(&d.shapes[1].style, "stroke-width"), "2");
        assert_eq!(style_value(&d.shapes[2].style, "fill"), "black");
        assert_eq!(style_value(&d.shapes[2].style, "stroke"), "none");
        let img = render_drawing(&d, 20).unwrap();
        assert_eq!(img.len(), 20 * 20 * 4);
        // The black rect (1..3 in a 10 box, scaled 2x) is opaque black at (4, 4).
        let i = (4 * 20 + 4) * 4;
        assert_eq!(&img[i..i + 4], &[0.0, 0.0, 0.0, 1.0]);
        assert!(parse_svg("<p>no svg</p>").is_err());
        // Width and height stand in for a missing view box.
        assert_eq!(parse_svg("<svg width=\"20\" height=\"0\"/>").unwrap().view_box, [0.0, 0.0, 20.0, 45.0]);
    }

    #[test]
    fn shapes_as_path_data() {
        let shape = |kind: &str, attrs: &[(&str, &str)]| Shape {
            kind: kind.into(),
            attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            style: DEFAULT_STYLE.map(String::from),
            opacity: 1.0,
        };
        assert_eq!(
            shape_path(&shape("circle", &[("cx", "5"), ("cy", "5.5"), ("r", "2")])),
            "M7 5.5A2 2 0 1 1 3 5.5A2 2 0 1 1 7 5.5Z"
        );
        assert_eq!(shape_path(&shape("rect", &[("width", "2"), ("height", "3")])), "M0 0h2v3h-2Z");
        assert_eq!(shape_path(&shape("rect", &[("width", "0"), ("height", "3")])), "");
        assert_eq!(shape_path(&shape("line", &[("x2", "1e1")])), "M0 0L10 0");
        assert_eq!(shape_path(&shape("polygon", &[("points", " 1,2 3 4 ")])), "M1 2 3 4Z");
        assert_eq!(shape_path(&shape("polyline", &[("points", "1 2 3")])), "");
        assert_eq!(shape_path(&shape("ellipse", &[("rx", "-1"), ("ry", "1")])), "");
    }

    #[test]
    fn attributes_double_quotes_and_style() {
        let a = attributes_of(" a=\"1\" b = \"2\" c='3' d:e-f=\"x\" style=\"a: 9 ; :z; g:h:i\"");
        assert_eq!(a.get("a").map(String::as_str), Some("9"));
        assert_eq!(a.get("b").map(String::as_str), Some("2"));
        assert_eq!(a.get("c"), None);
        assert_eq!(a.get("d:e-f").map(String::as_str), Some("x"));
        assert_eq!(a.get("g").map(String::as_str), Some("h:i"));
    }

    #[test]
    fn cburnett_sprites_every_piece_sane_coverage_cached_per_size() {
        for size in [16u32, 32, 45, 72, 100] {
            for code in PIECE_CODES {
                let s = piece_sprite(code, size).unwrap();
                let n = size as usize;
                assert_eq!(s.len(), n * n * 4);
                let (mut alpha, mut light) = (0.0f64, 0.0f64);
                let (mut min_x, mut max_x) = (n, 0);
                let mut partial = 0;
                for y in 0..n {
                    for x in 0..n {
                        let j = (y * n + x) * 4;
                        let a = f64::from(s[j + 3]);
                        assert!((0.0..=1.0 + 1e-6).contains(&a));
                        assert!(f64::from(s[j]) <= a + 1e-6, "premultiplied");
                        alpha += a;
                        light += f64::from(s[j]);
                        if a > 0.5 {
                            min_x = min_x.min(x);
                            max_x = max_x.max(x);
                        }
                        if a > 0.05 && a < 0.95 {
                            partial += 1;
                        }
                    }
                }
                let fill = alpha / (n * n) as f64;
                assert!(fill > 0.1 && fill < 0.6, "piece {code} at {size}: coverage {fill}");
                let lightness = light / alpha;
                if code < 8 {
                    assert!(lightness > 0.35, "white piece {code}: {lightness}");
                } else {
                    assert!(lightness < 0.3, "black piece {code}: {lightness}");
                }
                let centre = (min_x + max_x) as f64 / 2.0;
                assert!((centre - (n - 1) as f64 / 2.0).abs() < n as f64 * 0.08, "piece {code} centred");
                assert!(partial > n / 2, "piece {code} at {size} anti-aliased");
            }
        }
        // The pawn is left-right symmetric.
        let p = piece_sprite(1, 45).unwrap();
        let mut asym = 0.0f64;
        for y in 0..45 {
            for x in 0..45 {
                asym += (f64::from(p[(y * 45 + x) * 4 + 3]) - f64::from(p[(y * 45 + 44 - x) * 4 + 3])).abs();
            }
        }
        assert!(asym < 3.0, "pawn asymmetry {asym}");
        assert!(Arc::ptr_eq(&piece_sprite(6, 48).unwrap(), &piece_sprite(6, 48).unwrap()));
        assert_eq!(piece_sprite(7, 48), Err(SpriteError::BadCode(7)));
        assert_eq!(piece_sprite(1, 4), Err(SpriteError::BadSize(4)));
    }
}
