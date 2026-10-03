//! Synthetic per-game features for the tests of the scoring model (the former server's
//! `testing/synthetic.js`, draw for draw). Honest players are drawn from the prior population
//! (true per-game sd = prior sd / SD_INFLATION) with a persistent personal offset `theta`;
//! assisted players mix engine-like values into a fraction of their moves.
//!
//! The draws reproduce the former generator bit for bit: mulberry32, and V8's `Math.log` and
//! `Math.cos` (fdlibm ports, which differ from the C library in a few results out of a hundred).
//! The scoring tests can then compare exact numbers with the former server.

use super::analysis::stats::clamp;
use super::priors::{Metric, SD_INFLATION, prior_for, time_class_of_category};
use super::scoring::{NumField, Population, PopulationSource, SideRecord};

/// Deterministic PRNG (mulberry32), as the former tests used it.
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Rng {
        Rng(seed)
    }

    /// A value in [0, 1).
    pub fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6D2B_79F5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

/// Standard normal draw (Box-Muller, as the former generator).
pub fn gauss(r: &mut Rng) -> f64 {
    let mut u = 0.0;
    let mut v = 0.0;
    while u == 0.0 {
        u = r.next();
    }
    while v == 0.0 {
        v = r.next();
    }
    (-2.0 * v8_log(u)).sqrt() * v8_cos(2.0 * std::f64::consts::PI * v)
}

/// Engine-assisted per-game values (mean, sd) by metric, as measured with a real engine relay.
const ENGINE_PROFILE: [(f64, f64); 7] =
    [(98.0, 1.5), (7.5, 5.5), (0.7, 0.13), (0.56, 0.12), (0.6, 0.15), (0.0, 0.2), (0.26, 0.08)];

const LIMITS: [(f64, f64); 7] =
    [(0.0, 100.0), (0.0, 1000.0), (0.0, 1.0), (0.0, 1.0), (0.0, 1.0), (-1.0, 1.0), (0.0, 5.0)];

/// Options of one synthetic side.
#[derive(Clone, Debug)]
pub struct SideOptions {
    pub user_id: u32,
    pub rating: f64,
    pub category: &'static str,
    /// Personal offset in per-game sd units (positive = stronger than the rating).
    pub theta: f64,
    /// Personal timing offset in per-game sd units (positive = flatter, less correlated).
    pub time_style: f64,
    /// Fraction of engine moves (0..1).
    pub engine: f64,
    /// Relay timing even when `engine` < 1 (default: `engine` > 0).
    pub engine_timing: Option<bool>,
    pub n: f64,
    pub n_complex: f64,
    pub game_id: f64,
    pub ended_at: f64,
    pub rating_games: f64,
    pub tau: f64,
}

impl Default for SideOptions {
    fn default() -> SideOptions {
        SideOptions {
            user_id: 1,
            rating: 1500.0,
            category: "5+0",
            theta: 0.0,
            time_style: 0.0,
            engine: 0.0,
            engine_timing: None,
            n: 25.0,
            n_complex: 8.0,
            game_id: 0.0,
            ended_at: 0.0,
            rating_games: 100.0,
            tau: 0.4,
        }
    }
}

/// One side of an analysed game.
pub fn synthetic_side(r: &mut Rng, o: &SideOptions) -> SideRecord {
    let tc = time_class_of_category(o.category);
    let engine_timing = o.engine_timing.unwrap_or(o.engine > 0.0);
    let mut side = SideRecord {
        game_id: o.game_id,
        category: o.category.to_string(),
        ended_at: o.ended_at,
        analysed_at: o.ended_at,
        user_id: o.user_id,
        rating: NumField::Number(o.rating),
        rating_games: NumField::Number(o.rating_games),
        n: o.n,
        n_complex: o.n_complex,
        n_timed: o.n,
        ..SideRecord::default()
    };
    let within = (1.0 - o.tau * o.tau).sqrt();
    let common = gauss(r);
    for m in Metric::ALL {
        let p = prior_for(m, o.rating, tc);
        let sd = p.sd / SD_INFLATION;
        let sign = if matches!(m, Metric::Acpl | Metric::TimeCorr | Metric::TimeCv) { -1.0 } else { 1.0 };
        let timing = matches!(m, Metric::TimeCorr | Metric::TimeCv);
        let shared = if timing { gauss(r) } else { 0.6 * common + 0.8 * gauss(r) };
        let human = p.mean + sd * sign * ((if timing { o.time_style } else { o.theta }) + within * shared);
        let (e_mean, e_sd) = ENGINE_PROFILE[m.index()];
        let eng = e_mean + e_sd * gauss(r);
        let frac = if timing { if engine_timing { o.engine } else { 0.0 } } else { o.engine };
        let (lo, hi) = LIMITS[m.index()];
        side.set_value(m, Some(clamp(frac * eng + (1.0 - frac) * human, lo, hi)));
    }
    side
}

/// A player's history of `games` synthetic games, oldest first; games from index `engine_from`
/// on are assisted with the fraction `engine`.
pub fn synthetic_history(
    r: &mut Rng,
    games: usize,
    engine_from: Option<usize>,
    engine: f64,
    o: &SideOptions,
) -> Vec<SideRecord> {
    history_from(r, games, engine_from, engine, 1_800_000_000_000.0, o)
}

/// [`synthetic_history`] with the end time of the first game.
pub fn history_from(
    r: &mut Rng,
    games: usize,
    engine_from: Option<usize>,
    engine: f64,
    start_at: f64,
    o: &SideOptions,
) -> Vec<SideRecord> {
    (0..games)
        .map(|i| {
            let side = SideOptions {
                engine: if engine_from.is_some_and(|f| i >= f) { engine } else { 0.0 },
                game_id: f64::from(o.user_id.max(1)) * 100_000.0 + i as f64,
                ended_at: start_at + i as f64 * 3_600_000.0,
                ..o.clone()
            };
            synthetic_side(r, &side)
        })
        .collect()
}

/// Feeds `count` honest synthetic games into a population, as the server's own data would.
pub fn learn_population(
    pop: &Population,
    src: &dyn PopulationSource,
    r: &mut Rng,
    count: usize,
    categories: &[&'static str],
) {
    let (min_rating, max_rating) = (700.0, 2600.0);
    for i in 0..count {
        let rating = min_rating + (r.next() * (max_rating - min_rating)).floor();
        let category = categories[i % categories.len()];
        let theta = 0.4 * gauss(r);
        let time_style = 0.5 * gauss(r);
        let s = synthetic_side(
            r,
            &SideOptions { user_id: 1, rating, category, theta, time_style, ..SideOptions::default() },
        );
        let bucket = 500f64.max(2900f64.min((rating / 100.0).floor() * 100.0)) as i64;
        pop.update(src, category, bucket, &s, time_class_of_category(category));
    }
}

// ---- V8's fdlibm log and cos ------------------------------------------------------------------
// The constants keep fdlibm's digits, so they can be checked against its source.

fn hi_word(x: f64) -> u32 {
    (x.to_bits() >> 32) as u32
}

fn lo_word(x: f64) -> u32 {
    x.to_bits() as u32
}

fn from_words(hi: u32, lo: u32) -> f64 {
    f64::from_bits((u64::from(hi) << 32) | u64::from(lo))
}

/// V8's `Math.log` (fdlibm's `__ieee754_log`).
#[allow(clippy::excessive_precision)]
pub fn v8_log(mut x: f64) -> f64 {
    const LN2_HI: f64 = 6.93147180369123816490e-01;
    const LN2_LO: f64 = 1.90821492927058770002e-10;
    const TWO54: f64 = 1.80143985094819840000e+16;
    const LG: [f64; 7] = [
        6.666666666666735130e-01,
        3.999999999940941908e-01,
        2.857142874366239149e-01,
        2.222219843214978396e-01,
        1.818357216161805012e-01,
        1.531383769920937332e-01,
        1.479819860511658591e-01,
    ];
    let mut hx = hi_word(x) as i32;
    let lx = lo_word(x);
    let mut k: i32 = 0;
    if hx < 0x0010_0000 {
        if ((hx & 0x7fff_ffff) as u32 | lx) == 0 {
            return f64::NEG_INFINITY;
        }
        if hx < 0 {
            return f64::NAN;
        }
        k -= 54;
        x *= TWO54;
        hx = hi_word(x) as i32;
    }
    if hx >= 0x7ff0_0000 {
        return x + x;
    }
    k += (hx >> 20) - 1023;
    hx &= 0x000f_ffff;
    let i = (hx + 0x95f64) & 0x10_0000;
    x = from_words((hx | (i ^ 0x3ff0_0000)) as u32, lo_word(x));
    k += i >> 20;
    let f = x - 1.0;
    let dk = f64::from(k);
    if (0x000f_ffff & (2 + hx)) < 3 {
        if f == 0.0 {
            return if k == 0 { 0.0 } else { dk * LN2_HI + dk * LN2_LO };
        }
        let r = f * f * (0.5 - 0.33333333333333333 * f);
        return if k == 0 { f - r } else { dk * LN2_HI - ((r - dk * LN2_LO) - f) };
    }
    let s = f / (2.0 + f);
    let z = s * s;
    let w = z * z;
    let t1 = w * (LG[1] + w * (LG[3] + w * LG[5]));
    let t2 = z * (LG[0] + w * (LG[2] + w * (LG[4] + w * LG[6])));
    let i = (hx - 0x6147a) | (0x6b851 - hx);
    let r = t2 + t1;
    if i > 0 {
        let hfsq = 0.5 * f * f;
        if k == 0 {
            f - (hfsq - s * (hfsq + r))
        } else {
            dk * LN2_HI - ((hfsq - (s * (hfsq + r) + dk * LN2_LO)) - f)
        }
    } else if k == 0 {
        f - s * (f - r)
    } else {
        dk * LN2_HI - ((s * (f - r) - dk * LN2_LO) - f)
    }
}

#[allow(clippy::excessive_precision)]
fn kernel_cos(x: f64, y: f64) -> f64 {
    const C: [f64; 6] = [
        4.16666666666666019037e-02,
        -1.38888888888741095749e-03,
        2.48015872894767294178e-05,
        -2.75573143513906633035e-07,
        2.08757232129817482790e-09,
        -1.13596475577881948265e-11,
    ];
    let ix = (hi_word(x) & 0x7fff_ffff) as i32;
    if ix < 0x3E40_0000 && x as i32 == 0 {
        return 1.0;
    }
    let z = x * x;
    let r = z * (C[0] + z * (C[1] + z * (C[2] + z * (C[3] + z * (C[4] + z * C[5])))));
    if ix < 0x3FD3_3333 {
        return 1.0 - (0.5 * z - (z * r - x * y));
    }
    let qx = if ix > 0x3FE9_0000 { 0.28125 } else { from_words((ix - 0x0020_0000) as u32, 0) };
    let hz = 0.5 * z - qx;
    let a = 1.0 - qx;
    a - (hz - (z * r - x * y))
}

#[allow(clippy::excessive_precision)]
fn kernel_sin(x: f64, y: f64) -> f64 {
    const S: [f64; 6] = [
        -1.66666666666666324348e-01,
        8.33333333332248946124e-03,
        -1.98412698298579493134e-04,
        2.75573137070700676789e-06,
        -2.50507602534068634195e-08,
        1.58969099521155010221e-10,
    ];
    let ix = (hi_word(x) & 0x7fff_ffff) as i32;
    if ix < 0x3E40_0000 && x as i32 == 0 {
        return x;
    }
    let z = x * x;
    let v = z * x;
    let r = S[1] + z * (S[2] + z * (S[3] + z * (S[4] + z * S[5])));
    x - ((z * (0.5 * y - v * r) - y) - v * S[0])
}

// Argument reduction by pi/2 for |x| < 2^19 pi/2 (the medium-size path of fdlibm).
#[allow(clippy::excessive_precision, clippy::approx_constant)]
fn rem_pio2(x: f64) -> (i32, f64, f64) {
    const NPIO2_HW: [i32; 32] = [
        0x3FF921FB, 0x400921FB, 0x4012D97C, 0x401921FB, 0x401F6A7A, 0x4022D97C, 0x4025FDBB, 0x402921FB,
        0x402C463A, 0x402F6A7A, 0x4031475C, 0x4032D97C, 0x40346B9C, 0x4035FDBB, 0x40378FDB, 0x403921FB,
        0x403AB41B, 0x403C463A, 0x403DD85A, 0x403F6A7A, 0x40407E4C, 0x4041475C, 0x4042106C, 0x4042D97C,
        0x4043A28C, 0x40446B9C, 0x404534AC, 0x4045FDBB, 0x4046C6CB, 0x40478FDB, 0x404858EB, 0x404921FB,
    ];
    const INVPIO2: f64 = 6.36619772367581382433e-01;
    const PIO2_1: f64 = 1.57079632673412561417e+00;
    const PIO2_1T: f64 = 6.07710050650619224932e-11;
    const PIO2_2: f64 = 6.07710050630396597660e-11;
    const PIO2_2T: f64 = 2.02226624879595063154e-21;
    const PIO2_3: f64 = 2.02226624871116645580e-21;
    const PIO2_3T: f64 = 8.47842766036889956997e-32;
    let hx = hi_word(x) as i32;
    let ix = hx & 0x7fff_ffff;
    if ix < 0x4002_D97C {
        if hx > 0 {
            let z = x - PIO2_1;
            if ix != 0x3FF9_21FB {
                let y0 = z - PIO2_1T;
                return (1, y0, (z - y0) - PIO2_1T);
            }
            let z = z - PIO2_2;
            let y0 = z - PIO2_2T;
            return (1, y0, (z - y0) - PIO2_2T);
        }
        let z = x + PIO2_1;
        if ix != 0x3FF9_21FB {
            let y0 = z + PIO2_1T;
            return (-1, y0, (z - y0) + PIO2_1T);
        }
        let z = z + PIO2_2;
        let y0 = z + PIO2_2T;
        return (-1, y0, (z - y0) + PIO2_2T);
    }
    assert!(ix <= 0x4139_21FB, "large arguments are not needed by the tests");
    let mut t = x.abs();
    let n = (t * INVPIO2 + 0.5) as i32;
    let fnn = f64::from(n);
    let mut r = t - fnn * PIO2_1;
    let mut w = fnn * PIO2_1T;
    let mut y0 = r - w;
    if !(n < 32 && ix != NPIO2_HW[(n - 1) as usize]) {
        let j = ix >> 20;
        let mut i = j - ((hi_word(y0) >> 20) & 0x7ff) as i32;
        if i > 16 {
            t = r;
            w = fnn * PIO2_2;
            r = t - w;
            w = fnn * PIO2_2T - ((t - r) - w);
            y0 = r - w;
            i = j - ((hi_word(y0) >> 20) & 0x7ff) as i32;
            if i > 49 {
                t = r;
                w = fnn * PIO2_3;
                r = t - w;
                w = fnn * PIO2_3T - ((t - r) - w);
                y0 = r - w;
            }
        }
    }
    let y1 = (r - y0) - w;
    if hx < 0 { (-n, -y0, -y1) } else { (n, y0, y1) }
}

/// V8's `Math.cos` (fdlibm's `cos`) for the arguments of the tests (|x| < 2^19 pi/2).
pub fn v8_cos(x: f64) -> f64 {
    let ix = hi_word(x) & 0x7fff_ffff;
    if ix <= 0x3FE9_21FB {
        return kernel_cos(x, 0.0);
    }
    if ix >= 0x7ff0_0000 {
        return f64::NAN;
    }
    let (n, y0, y1) = rem_pio2(x);
    match n & 3 {
        0 => kernel_cos(y0, y1),
        1 => -kernel_sin(y0, y1),
        2 => -kernel_cos(y0, y1),
        _ => kernel_sin(y0, y1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Inputs where the C library's log and cos differ from V8's (bit patterns from Node 22).
    const LOG_CASES: [(u64, u64); 4] = [
        (0x3fd3_a1d4_4000_0000, 0xbff2_e84d_cabd_58ec),
        (0x3fda_6a22_43c0_0000, 0xbfec_51a5_caa2_1df4),
        (0x3fe6_fd48_5ec0_0000, 0xbfd5_2a3e_2408_d7f4),
        (0x3fe8_1bc8_cd40_0000, 0xbfd2_1f75_5ab5_1120),
    ];
    const COS_CASES: [(u64, u64); 4] = [
        (0x3fea_f51f_b6ae_a49c, 0x3fe5_4d10_15c2_83cd),
        (0x4010_34ae_2747_3fda, 0xbfe3_a4c1_9a32_fbd2),
        (0x3fca_0aa7_111c_f520, 0x3fef_570a_e8a1_3ff4),
        (0x4010_399c_d7a9_7f3d, 0xbfe3_858c_6de5_abea),
    ];

    #[test]
    fn draws_match_the_former_generator() {
        let mut r = Rng::new(1);
        assert_eq!(r.next(), 0.6270739405881613);
        assert_eq!(r.next(), 0.002735721180215478);
        let mut r = Rng::new(7);
        assert_eq!(gauss(&mut r).to_bits(), 0x4006_1332_2506_09a9);
        assert_eq!(gauss(&mut r).to_bits(), 0xbfb1_6bd0_4ea5_c407);
        for (x, log_bits) in LOG_CASES {
            assert_eq!(v8_log(f64::from_bits(x)).to_bits(), log_bits);
        }
        for (x, cos_bits) in COS_CASES {
            assert_eq!(v8_cos(f64::from_bits(x)).to_bits(), cos_bits);
        }
    }
}
