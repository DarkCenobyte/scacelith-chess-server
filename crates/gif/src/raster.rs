//! Anti-aliased vector rasterizer of the piece sprites: SVG path data (M L H V C S Q T A Z,
//! absolute and relative) flattened to polylines, strokes turned into polygons (butt / round /
//! square caps, miter / round / bevel joins with the miter limit), and the coverage of each
//! pixel (raster.js of the Node server, reproduced operation for operation).
//!
//! Coverage: every pixel row is cut into [`SUB`] sub-scanlines; on each one the crossings of the
//! polygon edges are sorted and walked with the winding number (nonzero or evenodd rule), and the
//! inside spans are added with their exact horizontal extent. A stroke is the union of its segment
//! rectangles, join wedges and caps, all oriented the same way, filled with the nonzero rule.
//!
//! The coverage buffers are `f32` like the Node server's `Float32Array`: every store rounds the
//! `f64` result to `f32`, every read widens it back.

use std::f64::consts::PI;
use std::fmt;

use crate::jsmath;

/// Sub-scanlines per pixel row (vertical anti-aliasing levels).
pub(crate) const SUB: usize = 16;

/// A syntax error in SVG path data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathError(String);

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "path data: {}", self.0)
    }
}

impl std::error::Error for PathError {}

/// One command of path data as written: its letter (case kept) and its arguments (arc flags as
/// 0 or 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Command {
    pub letter: u8,
    pub args: [f64; 7],
}

/// Number of arguments of a command letter, `None` for anything that is not a command.
fn arg_count(letter: u8) -> Option<usize> {
    Some(match letter.to_ascii_uppercase() {
        b'M' | b'L' | b'T' => 2,
        b'H' | b'V' => 1,
        b'C' => 6,
        b'S' | b'Q' => 4,
        b'A' => 7,
        b'Z' => 0,
        _ => return None,
    })
}

fn is_separator(c: u8) -> bool {
    matches!(c, b' ' | b',' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

struct PathParser<'a> {
    d: &'a [u8],
    i: usize,
}

impl PathParser<'_> {
    fn skip(&mut self) {
        while self.i < self.d.len() && is_separator(self.d[self.i]) {
            self.i += 1;
        }
    }

    fn digits(&self, mut j: usize) -> usize {
        while j < self.d.len() && self.d[j].is_ascii_digit() {
            j += 1;
        }
        j
    }

    /// `[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?` at the current position.
    fn number(&mut self) -> Result<f64, PathError> {
        self.skip();
        let d = self.d;
        let start = self.i;
        let mut j = start;
        if j < d.len() && (d[j] == b'+' || d[j] == b'-') {
            j += 1;
        }
        let int_end = self.digits(j);
        if int_end > j {
            j = int_end;
            if j < d.len() && d[j] == b'.' {
                j = self.digits(j + 1);
            }
        } else if j + 1 < d.len() && d[j] == b'.' && d[j + 1].is_ascii_digit() {
            j = self.digits(j + 1);
        } else {
            return Err(PathError(format!("number expected at {start}")));
        }
        if j < d.len() && (d[j] == b'e' || d[j] == b'E') {
            let mut k = j + 1;
            if k < d.len() && (d[k] == b'+' || d[k] == b'-') {
                k += 1;
            }
            let exp_end = self.digits(k);
            if exp_end > k {
                j = exp_end;
            }
        }
        self.i = j;
        // The matched text is ASCII and a valid Rust float literal.
        let text = std::str::from_utf8(&d[start..j])
            .map_err(|_| PathError(format!("number expected at {start}")))?;
        text.parse::<f64>().map_err(|_| PathError(format!("number expected at {start}")))
    }

    fn flag(&mut self) -> Result<f64, PathError> {
        self.skip();
        match self.d.get(self.i) {
            Some(b'0') => {
                self.i += 1;
                Ok(0.0)
            }
            Some(b'1') => {
                self.i += 1;
                Ok(1.0)
            }
            _ => Err(PathError(format!("arc flag expected at {}", self.i))),
        }
    }
}

/// Parses SVG path data; an implicit command after M / m is L / l.
pub(crate) fn parse_path_data(d: &str) -> Result<Vec<Command>, PathError> {
    let mut p = PathParser { d: d.as_bytes(), i: 0 };
    let mut out = Vec::new();
    let mut cmd: Option<u8> = None;
    loop {
        p.skip();
        let Some(&c) = p.d.get(p.i) else { break };
        if arg_count(c).is_some() {
            cmd = Some(c);
            p.i += 1;
            if c.eq_ignore_ascii_case(&b'Z') {
                out.push(Command { letter: c, args: [0.0; 7] });
                continue;
            }
        }
        let letter = match cmd {
            Some(l) if !l.eq_ignore_ascii_case(&b'Z') => l,
            _ => return Err(PathError(format!("command expected at {}", p.i))),
        };
        let count = arg_count(letter).unwrap_or(0);
        let arc = letter.eq_ignore_ascii_case(&b'A');
        let mut args = [0.0; 7];
        for (k, arg) in args.iter_mut().enumerate().take(count) {
            *arg = if arc && (k == 3 || k == 4) { p.flag()? } else { p.number()? };
        }
        out.push(Command { letter, args });
        match letter {
            b'M' => cmd = Some(b'L'),
            b'm' => cmd = Some(b'l'),
            _ => {}
        }
    }
    Ok(out)
}

/// A path flattened to a polyline, in pixel coordinates: x0, y0, x1, y1, ...
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Subpath {
    pub pts: Vec<f64>,
    pub closed: bool,
}

/// The transform and tolerance of [`flatten_path`]: pixel = user * scale + (dx, dy); `tol` is
/// the largest distance in pixels between a curve and its polyline.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Flatten {
    pub scale: f64,
    pub dx: f64,
    pub dy: f64,
    pub tol: f64,
}

impl Default for Flatten {
    fn default() -> Self {
        Flatten { scale: 1.0, dx: 0.0, dy: 0.0, tol: 0.05 }
    }
}

struct Flattener {
    t: Flatten,
    subpaths: Vec<Subpath>,
    /// Index of the open subpath in `subpaths`.
    cur: Option<usize>,
}

impl Flattener {
    fn emit(&mut self, px: f64, py: f64) {
        let Flatten { scale, dx, dy, .. } = self.t;
        if let Some(i) = self.cur {
            self.subpaths[i].pts.extend([px * scale + dx, py * scale + dy]);
        }
    }

    fn begin(&mut self, px: f64, py: f64) {
        self.subpaths.push(Subpath::default());
        self.cur = Some(self.subpaths.len() - 1);
        self.emit(px, py);
    }

    fn ensure(&mut self, x: f64, y: f64) {
        if self.cur.is_none() {
            self.begin(x, y);
        }
    }
}

/// Flattens parsed path data to polylines.
pub(crate) fn flatten_path(cmds: &[Command], t: Flatten) -> Vec<Subpath> {
    let mut f = Flattener { t, subpaths: Vec::new(), cur: None };
    let (mut x, mut y, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut cx2, mut cy2) = (0.0f64, 0.0f64);
    let mut prev = 0u8;
    let tol_user = t.tol / t.scale;
    for cmd in cmds {
        let c = cmd.letter;
        let a = &cmd.args;
        let rel = c.is_ascii_lowercase();
        let (ox, oy) = if rel { (x, y) } else { (0.0, 0.0) };
        let p = prev.to_ascii_uppercase();
        match c.to_ascii_uppercase() {
            b'M' => {
                x = a[0] + ox;
                y = a[1] + oy;
                sx = x;
                sy = y;
                f.begin(x, y);
            }
            b'L' => {
                f.ensure(x, y);
                x = a[0] + ox;
                y = a[1] + oy;
                f.emit(x, y);
            }
            b'H' => {
                f.ensure(x, y);
                x = a[0] + ox;
                f.emit(x, y);
            }
            b'V' => {
                f.ensure(x, y);
                y = a[0] + oy;
                f.emit(x, y);
            }
            b'C' | b'S' => {
                f.ensure(x, y);
                let (x1, y1, rest) = if c.eq_ignore_ascii_case(&b'C') {
                    (a[0] + ox, a[1] + oy, &a[2..6])
                } else if p == b'C' || p == b'S' {
                    (2.0 * x - cx2, 2.0 * y - cy2, &a[0..4])
                } else {
                    (x, y, &a[0..4])
                };
                let (x2, y2, x3, y3) = (rest[0] + ox, rest[1] + oy, rest[2] + ox, rest[3] + oy);
                cubic(&mut f, [x, y, x1, y1, x2, y2, x3, y3], tol_user);
                cx2 = x2;
                cy2 = y2;
                x = x3;
                y = y3;
            }
            b'Q' | b'T' => {
                f.ensure(x, y);
                let (x1, y1, rest) = if c.eq_ignore_ascii_case(&b'Q') {
                    (a[0] + ox, a[1] + oy, &a[2..4])
                } else if p == b'Q' || p == b'T' {
                    (2.0 * x - cx2, 2.0 * y - cy2, &a[0..2])
                } else {
                    (x, y, &a[0..2])
                };
                let (x2, y2) = (rest[0] + ox, rest[1] + oy);
                quad(&mut f, [x, y, x1, y1, x2, y2], tol_user);
                cx2 = x1;
                cy2 = y1;
                x = x2;
                y = y2;
            }
            b'A' => {
                f.ensure(x, y);
                let (x2, y2) = (a[5] + ox, a[6] + oy);
                arc(&mut f, [x, y, a[0], a[1], a[2], a[3], a[4], x2, y2], tol_user);
                x = x2;
                y = y2;
            }
            b'Z' => {
                if let Some(i) = f.cur.take() {
                    f.subpaths[i].closed = true;
                    x = sx;
                    y = sy;
                }
            }
            _ => {}
        }
        prev = c;
    }
    f.subpaths
}

fn cubic(f: &mut Flattener, [x, y, x1, y1, x2, y2, x3, y3]: [f64; 8], tol_user: f64) {
    let ddx = f64::max((x - 2.0 * x1 + x2).abs(), (x1 - 2.0 * x2 + x3).abs());
    let ddy = f64::max((y - 2.0 * y1 + y2).abs(), (y1 - 2.0 * y2 + y3).abs());
    let steps = f64::max(1.0, (0.75 * jsmath::hypot(ddx, ddy) / tol_user).sqrt().ceil());
    let mut k = 1.0;
    while k <= steps {
        let t = k / steps;
        let u = 1.0 - t;
        let a = u * u * u;
        let b = 3.0 * u * u * t;
        let c = 3.0 * u * t * t;
        let e = t * t * t;
        f.emit(a * x + b * x1 + c * x2 + e * x3, a * y + b * y1 + c * y2 + e * y3);
        k += 1.0;
    }
}

fn quad(f: &mut Flattener, [x, y, x1, y1, x2, y2]: [f64; 6], tol_user: f64) {
    let dd = jsmath::hypot(x - 2.0 * x1 + x2, y - 2.0 * y1 + y2);
    let steps = f64::max(1.0, (0.25 * dd / tol_user).sqrt().ceil());
    let mut k = 1.0;
    while k <= steps {
        let t = k / steps;
        let u = 1.0 - t;
        f.emit(u * u * x + 2.0 * u * t * x1 + t * t * x2, u * u * y + 2.0 * u * t * y1 + t * t * y2);
        k += 1.0;
    }
}

/// Angle between two vectors (SVG 1.1 F.6.5.4).
fn angle(ux: f64, uy: f64, vx: f64, vy: f64) -> f64 {
    jsmath::atan2(ux * vy - uy * vx, ux * vx + uy * vy)
}

/// Angular step of a polyline within `tol` of a circle of radius `r`.
fn arc_step(r: f64, tol: f64) -> f64 {
    if r > tol { 2.0 * jsmath::acos(f64::max(-1.0, 1.0 - tol / r)) } else { PI / 2.0 }
}

/// An elliptical arc (SVG 1.1 F.6.5: endpoint to center parameterization), in user units.
fn arc(f: &mut Flattener, [x1, y1, rx, ry, phi_deg, fa, fs, x2, y2]: [f64; 9], tol: f64) {
    if x1 == x2 && y1 == y2 {
        return;
    }
    let mut rx = rx.abs();
    let mut ry = ry.abs();
    if rx == 0.0 || ry == 0.0 {
        f.emit(x2, y2);
        return;
    }
    let phi = (phi_deg % 360.0) * PI / 180.0;
    let cos = jsmath::cos(phi);
    let sin = jsmath::sin(phi);
    let hx = (x1 - x2) / 2.0;
    let hy = (y1 - y2) / 2.0;
    let x1p = cos * hx + sin * hy;
    let y1p = -sin * hx + cos * hy;
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }
    let num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let coef = (if fa == fs { -1.0 } else { 1.0 }) * f64::max(0.0, num / den).sqrt();
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;
    let cx = cos * cxp - sin * cyp + (x1 + x2) / 2.0;
    let cy = sin * cxp + cos * cyp + (y1 + y2) / 2.0;
    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    // angle(1, 0, ux, uy) written out: `0 * ux` may be -0, which changes the sign of a zero
    // passed to atan2.
    #[allow(clippy::identity_op, clippy::erasing_op)]
    let t1 = jsmath::atan2(1.0 * uy - 0.0 * ux, 1.0 * ux + 0.0 * uy);
    let mut dt = angle(ux, uy, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if fs == 0.0 && dt > 0.0 {
        dt -= 2.0 * PI;
    } else if fs != 0.0 && dt < 0.0 {
        dt += 2.0 * PI;
    }
    let step = arc_step(f64::max(rx, ry), tol);
    let n = f64::max(2.0, (dt.abs() / step).ceil());
    let mut k = 1.0;
    while k < n {
        let t = t1 + dt * k / n;
        let ex = rx * jsmath::cos(t);
        let ey = ry * jsmath::sin(t);
        f.emit(cx + cos * ex - sin * ey, cy + sin * ex + cos * ey);
        k += 1.0;
    }
    f.emit(x2, y2);
}

/// The closed polygons of a fill: every subpath of at least three points, closed or not.
pub(crate) fn polygons_of(subpaths: &[Subpath]) -> Vec<&[f64]> {
    subpaths.iter().filter(|s| s.pts.len() >= 6).map(|s| s.pts.as_slice()).collect()
}

/// A circle as a polygon (positive orientation), within `tol` pixels.
pub(crate) fn circle_polygon(cx: f64, cy: f64, r: f64, tol: f64) -> Vec<f64> {
    let n = f64::max(8.0, (2.0 * PI / arc_step(r, tol)).ceil());
    let mut pts = Vec::with_capacity(2 * n as usize);
    let mut k = 0.0;
    while k < n {
        let t = 2.0 * PI * k / n;
        pts.push(cx + r * jsmath::cos(t));
        pts.push(cy + r * jsmath::sin(t));
        k += 1.0;
    }
    pts
}

fn signed_area(p: &[f64]) -> f64 {
    let n = p.len();
    let mut s = 0.0;
    for i in (0..n).step_by(2) {
        let j = (i + 2) % n;
        s += p[i] * p[j + 1] - p[j] * p[i + 1];
    }
    s / 2.0
}

fn oriented(p: Vec<f64>) -> Vec<f64> {
    if signed_area(&p) >= 0.0 {
        return p;
    }
    p.as_chunks::<2>().0.iter().rev().flatten().copied().collect()
}

/// Line cap of a stroke (SVG `stroke-linecap`; an unknown value draws like `butt`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineCap {
    Butt,
    Round,
    Square,
}

impl LineCap {
    pub fn parse(s: &str) -> LineCap {
        match s {
            "round" => LineCap::Round,
            "square" => LineCap::Square,
            _ => LineCap::Butt,
        }
    }
}

/// Line join of a stroke (SVG `stroke-linejoin`; an unknown value draws like `bevel`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineJoin {
    Miter,
    Round,
    Bevel,
}

impl LineJoin {
    pub fn parse(s: &str) -> LineJoin {
        match s {
            "miter" => LineJoin::Miter,
            "round" => LineJoin::Round,
            _ => LineJoin::Bevel,
        }
    }
}

/// How a stroke is drawn: width in pixels, caps, joins, miter limit, flattening tolerance.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stroke {
    pub width: f64,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f64,
    pub tol: f64,
}

/// The polygons of a stroke: their nonzero union is the stroked area.
pub(crate) fn stroke_polygons(subpaths: &[Subpath], st: Stroke) -> Vec<Vec<f64>> {
    let hw = st.width / 2.0;
    let mut out: Vec<Vec<f64>> = Vec::new();
    // Also refuses a NaN width.
    if hw.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return out;
    }
    let mut add = |p: Vec<f64>| {
        if p.len() >= 6 && signed_area(&p).abs() > 1e-12 {
            out.push(oriented(p));
        }
    };
    for sp in subpaths {
        // Points without consecutive duplicates (and without the closing duplicate of a closed
        // path).
        let mut xs: Vec<f64> = Vec::with_capacity(sp.pts.len() / 2);
        let mut ys: Vec<f64> = Vec::with_capacity(sp.pts.len() / 2);
        for &[px, py] in sp.pts.as_chunks::<2>().0 {
            if let (Some(&lx), Some(&ly)) = (xs.last(), ys.last())
                && (lx - px).abs() < 1e-9
                && (ly - py).abs() < 1e-9
            {
                continue;
            }
            xs.push(px);
            ys.push(py);
        }
        let mut n = xs.len();
        let closed = sp.closed;
        if closed && n > 1 && (xs[0] - xs[n - 1]).abs() < 1e-9 && (ys[0] - ys[n - 1]).abs() < 1e-9 {
            xs.pop();
            ys.pop();
            n -= 1;
        }
        if n == 1 {
            // A zero-length subpath: a dot for round and square caps (SVG 1.1 11.4).
            let (x, y) = (xs[0], ys[0]);
            match st.cap {
                LineCap::Round => add(circle_polygon(x, y, hw, st.tol)),
                LineCap::Square => add(vec![x - hw, y - hw, x + hw, y - hw, x + hw, y + hw, x - hw, y + hw]),
                LineCap::Butt => {}
            }
            continue;
        }
        if n == 0 {
            continue;
        }
        let segs = if closed { n } else { n - 1 };
        let mut dxs = Vec::with_capacity(segs);
        let mut dys = Vec::with_capacity(segs);
        for i in 0..segs {
            let j = (i + 1) % n;
            let ddx = xs[j] - xs[i];
            let ddy = ys[j] - ys[i];
            let len = jsmath::hypot(ddx, ddy);
            let (dx, dy) = (ddx / len, ddy / len);
            dxs.push(dx);
            dys.push(dy);
            let nx = -dy * hw;
            let ny = dx * hw;
            add(vec![
                xs[i] + nx,
                ys[i] + ny,
                xs[j] + nx,
                ys[j] + ny,
                xs[j] - nx,
                ys[j] - ny,
                xs[i] - nx,
                ys[i] - ny,
            ]);
        }
        // Joins: every vertex of a closed path, the inner vertices of an open one.
        let (first, end) = if closed { (0, n) } else { (1, n - 1) };
        for v in first..end {
            let a = (v + segs - 1) % segs;
            let b = v % segs;
            let (d0x, d0y, d1x, d1y) = (dxs[a], dys[a], dxs[b], dys[b]);
            let cross = d0x * d1y - d0y * d1x;
            let dot = d0x * d1x + d0y * d1y;
            if cross.abs() < 1e-12 && dot > 0.0 {
                continue; // straight on
            }
            let (vx, vy) = (xs[v], ys[v]);
            if st.join == LineJoin::Round {
                add(circle_polygon(vx, vy, hw, st.tol));
                continue;
            }
            // The outer side of the turn.
            let s = if cross > 0.0 { -1.0 } else { 1.0 };
            let (n0x, n0y, n1x, n1y) = (-d0y, d0x, -d1y, d1x);
            let (p0x, p0y) = (vx + s * hw * n0x, vy + s * hw * n0y);
            let (p1x, p1y) = (vx + s * hw * n1x, vy + s * hw * n1y);
            let nd = n0x * n1x + n0y * n1y;
            let sum = jsmath::hypot(n0x + n1x, n0y + n1y);
            if st.join == LineJoin::Miter && sum > 1e-9 && 2.0 / sum <= st.miter_limit {
                let tx = vx + s * hw * (n0x + n1x) / (1.0 + nd);
                let ty = vy + s * hw * (n0y + n1y) / (1.0 + nd);
                add(vec![vx, vy, p0x, p0y, tx, ty, p1x, p1y]);
            } else {
                add(vec![vx, vy, p0x, p0y, p1x, p1y]);
            }
        }
        if !closed {
            for (k, ox, oy) in [(0, -dxs[0], -dys[0]), (n - 1, dxs[segs - 1], dys[segs - 1])] {
                match st.cap {
                    LineCap::Round => add(circle_polygon(xs[k], ys[k], hw, st.tol)),
                    LineCap::Square => {
                        let nx = -oy * hw;
                        let ny = ox * hw;
                        let ex = xs[k] + ox * hw;
                        let ey = ys[k] + oy * hw;
                        add(vec![
                            xs[k] + nx,
                            ys[k] + ny,
                            ex + nx,
                            ey + ny,
                            ex - nx,
                            ey - ny,
                            xs[k] - nx,
                            ys[k] - ny,
                        ]);
                    }
                    LineCap::Butt => {}
                }
            }
        }
    }
    out
}

/// Winding rule of a fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FillRule {
    NonZero,
    EvenOdd,
}

struct Edge {
    x: f64,
    y0: f64,
    y1: f64,
    slope: f64,
    dir: i8,
}

/// Coverage of closed polygons (pixel coordinates) on a `width` x `height` grid, pixel (x, y)
/// being the square [x, x+1) x [y, y+1). `out` (width * height values in 0..1) is overwritten.
pub(crate) fn rasterize<P: AsRef<[f64]>>(
    polygons: &[P],
    rule: FillRule,
    width: usize,
    height: usize,
    out: &mut [f32],
) {
    out.fill(0.0);
    let mut edges: Vec<Edge> = Vec::new();
    let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in polygons {
        let p = p.as_ref();
        let n = p.len();
        for i in (0..n).step_by(2) {
            let j = (i + 2) % n;
            let (mut x0, mut y0, mut x1, mut y1) = (p[i], p[i + 1], p[j], p[j + 1]);
            if y0 == y1 {
                continue;
            }
            let mut dir = 1;
            if y0 > y1 {
                std::mem::swap(&mut x0, &mut x1);
                std::mem::swap(&mut y0, &mut y1);
                dir = -1;
            }
            edges.push(Edge { x: x0, y0, y1, slope: (x1 - x0) / (y1 - y0), dir });
            if y0 < min_y {
                min_y = y0;
            }
            if y1 > max_y {
                max_y = y1;
            }
        }
    }
    let m = edges.len();
    if m == 0 {
        return;
    }
    // Edges sorted by their top (a stable sort, as JavaScript's), walked with an active list.
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|&a, &b| edges[a].y0.partial_cmp(&edges[b].y0).unwrap_or(std::cmp::Ordering::Equal));
    let mut active: Vec<usize> = Vec::new();
    let mut next_edge = 0;
    let mut xs = vec![0.0f64; m];
    let mut ds = vec![0i8; m];
    let mut acc = vec![0.0f32; width + 1];
    let w1 = 1.0 / SUB as f64;
    let wf = width as f64;
    let row_start = f64::max(0.0, min_y.floor());
    let row_end = f64::min(height as f64, max_y.ceil());
    let mut row = row_start;
    while row < row_end {
        acc.fill(0.0);
        let mut any = false;
        for s in 0..SUB {
            let sy = row + (s as f64 + 0.5) * w1;
            while next_edge < m && edges[order[next_edge]].y0 <= sy {
                active.push(order[next_edge]);
                next_edge += 1;
            }
            let mut k = 0;
            let mut a = 0;
            while a < active.len() {
                let e = &edges[active[a]];
                if e.y1 <= sy {
                    active.swap_remove(a);
                    continue;
                }
                // Insertion sort by x as the crossings are collected.
                let x = e.x + (sy - e.y0) * e.slope;
                let mut j = k;
                k += 1;
                while j > 0 && xs[j - 1] > x {
                    xs[j] = xs[j - 1];
                    ds[j] = ds[j - 1];
                    j -= 1;
                }
                xs[j] = x;
                ds[j] = e.dir;
                a += 1;
            }
            if k < 2 {
                continue;
            }
            let mut wind = 0i32;
            for j in 0..k - 1 {
                wind += i32::from(ds[j]);
                let inside = match rule {
                    FillRule::EvenOdd => wind & 1 != 0,
                    FillRule::NonZero => wind != 0,
                };
                if !inside {
                    continue;
                }
                let a = f64::max(xs[j], 0.0);
                let b = f64::min(xs[j + 1], wf);
                if b <= a {
                    continue;
                }
                any = true;
                let ia = a.floor();
                let ib = b.floor();
                let add = |acc: &mut [f32], i: f64, v: f64| {
                    let i = i as usize;
                    acc[i] = (f64::from(acc[i]) + v) as f32;
                };
                if ia == ib {
                    add(&mut acc, ia, (b - a) * w1);
                } else {
                    add(&mut acc, ia, (ia + 1.0 - a) * w1);
                    let mut q = ia + 1.0;
                    while q < ib {
                        add(&mut acc, q, w1);
                        q += 1.0;
                    }
                    if ib < wf {
                        add(&mut acc, ib, (b - ib) * w1);
                    }
                }
            }
        }
        if any {
            let base = row as usize * width;
            for (o, &v) in out[base..base + width].iter_mut().zip(&acc) {
                if v > 0.0 {
                    *o = if v > 1.0 { 1.0 } else { v };
                }
            }
        }
        row += 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmds(d: &str) -> Vec<(char, Vec<f64>)> {
        parse_path_data(d)
            .unwrap()
            .into_iter()
            .map(|c| {
                let n = arg_count(c.letter).unwrap();
                (c.letter as char, c.args[..n].to_vec())
            })
            .collect()
    }

    fn flatten(d: &str, tol: f64) -> Vec<Subpath> {
        flatten_path(&parse_path_data(d).unwrap(), Flatten { tol, ..Flatten::default() })
    }

    fn area<P: AsRef<[f64]>>(polys: &[P], rule: FillRule, w: usize, h: usize) -> f64 {
        let mut out = vec![0.0f32; w * h];
        rasterize(polys, rule, w, h, &mut out);
        out.iter().map(|&v| f64::from(v)).sum()
    }

    fn stroke(width: f64, cap: LineCap, join: LineJoin, miter_limit: f64, tol: f64) -> Stroke {
        Stroke { width, cap, join, miter_limit, tol }
    }

    #[test]
    fn path_data_commands_implicit_repeats_compact_numbers_and_arc_flags() {
        assert_eq!(cmds("M1 2L3 4"), vec![('M', vec![1.0, 2.0]), ('L', vec![3.0, 4.0])]);
        assert_eq!(cmds("m1,2 3,4"), vec![('m', vec![1.0, 2.0]), ('l', vec![3.0, 4.0])]);
        assert_eq!(
            cmds("M1.5.5-1e1 2z"),
            vec![('M', vec![1.5, 0.5]), ('L', vec![-10.0, 2.0]), ('z', vec![])]
        );
        // cburnett writes arcs as "a.5.5 0 1 1-1 0": flags are single characters.
        assert_eq!(cmds("a.5.5 0 1 1-1 0"), vec![('a', vec![0.5, 0.5, 0.0, 1.0, 1.0, -1.0, 0.0])]);
        assert_eq!(cmds("A35 35 1 0 0 23 0")[0].1, vec![35.0, 35.0, 1.0, 0.0, 0.0, 23.0, 0.0]);
        assert_eq!(cmds("h1 2 3").len(), 3);
        assert_eq!(cmds("M+.5-.866 1. 2"), vec![('M', vec![0.5, -0.866]), ('L', vec![1.0, 2.0])]);
        assert_eq!(cmds("M1e1 2E-1"), vec![('M', vec![10.0, 0.2])]);
        for bad in ["1 2", "M1", "M1 2 L", "a1 1 0 2 0 1 1", "M1 2 Z 3"] {
            assert!(parse_path_data(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_path_data("M1").unwrap_err().to_string(), "path data: number expected at 2");
    }

    #[test]
    fn fill_coverage_exact_areas_sub_pixel_edges_nonzero_and_evenodd() {
        // A 10 x 10 square at (2.5, 2.5): area 100, half-covered edge pixels.
        let sq = flatten("M2.5 2.5h10v10h-10z", 0.05);
        let mut cov = vec![0.0f32; 16 * 16];
        rasterize(&polygons_of(&sq), FillRule::NonZero, 16, 16, &mut cov);
        let sum: f64 = cov.iter().map(|&v| f64::from(v)).sum();
        assert!((sum - 100.0).abs() < 1e-3, "area {sum}");
        assert!((cov[2 * 16 + 5] - 0.5).abs() < 1e-6);
        assert!((cov[2 * 16 + 2] - 0.25).abs() < 1e-6);
        assert_eq!(cov[5 * 16 + 5], 1.0);
        assert_eq!(cov[0], 0.0);
        let circle = [circle_polygon(32.0, 32.0, 20.0, 0.01)];
        assert!((area(&circle, FillRule::NonZero, 64, 64) - PI * 400.0).abs() < 2.0);
        // Two concentric circles in the same direction: evenodd makes a hole, nonzero does not.
        let ring = [circle_polygon(32.0, 32.0, 20.0, 0.01), circle_polygon(32.0, 32.0, 10.0, 0.01)];
        assert!((area(&ring, FillRule::EvenOdd, 64, 64) - PI * 300.0).abs() < 2.0);
        assert!((area(&ring, FillRule::NonZero, 64, 64) - PI * 400.0).abs() < 2.0);
        // Shapes partly outside the grid are clipped.
        let out = flatten("M-5 -5h10v10h-10z", 0.05);
        assert!((area(&polygons_of(&out), FillRule::NonZero, 8, 8) - 25.0).abs() < 1e-3);
        // Curves and arcs: the path of a circle with two arcs has the circle's area.
        let arcs = flatten("M52 32A20 20 0 1 1 12 32A20 20 0 1 1 52 32Z", 0.01);
        assert!((area(&polygons_of(&arcs), FillRule::NonZero, 64, 64) - PI * 400.0).abs() < 2.0);
        let quad = flatten("M0 0Q10 20 20 0T40 0", 0.01);
        assert!(quad[0].pts.len() > 10);
        assert_eq!(quad[0].pts[quad[0].pts.len() - 2], 40.0);
    }

    #[test]
    fn strokes_width_caps_and_joins() {
        let line = flatten("M10 10H50", 0.05);
        let a = |polys: Vec<Vec<f64>>| area(&polys, FillRule::NonZero, 64, 64);
        assert!(
            (a(stroke_polygons(&line, stroke(4.0, LineCap::Butt, LineJoin::Miter, 4.0, 0.05))) - 160.0).abs()
                < 1e-3
        );
        assert!(
            (a(stroke_polygons(&line, stroke(4.0, LineCap::Square, LineJoin::Miter, 4.0, 0.05))) - 176.0)
                .abs()
                < 1e-3
        );
        let round = a(stroke_polygons(&line, stroke(4.0, LineCap::Round, LineJoin::Miter, 4.0, 0.002)));
        assert!((round - (160.0 + PI * 4.0)).abs() < 0.05);
        // A right-angle corner: a miter join fills the corner square, a bevel half of it, round a
        // quarter circle.
        let corner = flatten("M10 10H40V40", 0.05);
        let miter = a(stroke_polygons(&corner, stroke(6.0, LineCap::Butt, LineJoin::Miter, 4.0, 0.05)));
        let bevel = a(stroke_polygons(&corner, stroke(6.0, LineCap::Butt, LineJoin::Bevel, 4.0, 0.05)));
        let round = a(stroke_polygons(&corner, stroke(6.0, LineCap::Butt, LineJoin::Round, 4.0, 0.002)));
        assert!((miter - bevel - 4.5).abs() < 0.05, "{miter} {bevel}");
        assert!((miter - round - (9.0 - PI * 9.0 / 4.0)).abs() < 0.2, "{miter} {round}");
        // A very sharp angle exceeds the miter limit: beveled.
        let sharp = flatten("M10 10L50 12L10 14", 0.05);
        let m = a(stroke_polygons(&sharp, stroke(2.0, LineCap::Butt, LineJoin::Miter, 4.0, 0.05)));
        let b = a(stroke_polygons(&sharp, stroke(2.0, LineCap::Butt, LineJoin::Bevel, 4.0, 0.05)));
        assert!((m - b).abs() < 0.01);
        // A closed rectangle stroked: outer minus inner.
        let rect = flatten("M10 10h20v20h-20z", 0.05);
        let r = a(stroke_polygons(&rect, stroke(2.0, LineCap::Butt, LineJoin::Miter, 4.0, 0.05)));
        assert!((r - (22.0 * 22.0 - 18.0 * 18.0)).abs() < 1e-3);
        // A zero-length subpath with round caps is a dot; with butt caps nothing.
        let dot = flatten("M20 20z", 0.05);
        let d = a(stroke_polygons(&dot, stroke(4.0, LineCap::Round, LineJoin::Round, 4.0, 0.002)));
        assert!((d - PI * 4.0).abs() < 0.05);
        assert!(stroke_polygons(&dot, stroke(4.0, LineCap::Butt, LineJoin::Round, 4.0, 0.05)).is_empty());
    }
}
