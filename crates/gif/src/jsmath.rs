//! The floating-point functions of JavaScript (V8, Node 22) that the renderer depends on, bit for
//! bit: the anti-aliased pieces are mapped to palette indices, so a last-bit difference in a sine
//! can change a pixel of the GIF.
//!
//! * `sin`/`cos`: fdlibm 5.3 as in V8's `src/base/ieee754.cc` (neither the system libm nor the
//!   `libm` crate evaluate the kernels in the same order).
//! * `atan2`/`acos`: the `libm` crate (fdlibm `e_atan2.c`/`e_acos.c`, the algorithms of V8).
//! * `hypot`: V8's `Math.hypot` builtin (Kahan sum of the squares scaled by the largest value),
//!   not the C library's.
//! * `round`: `Math.round` (halves toward +infinity).
//!
//! Callers keep JavaScript's left-to-right evaluation order and never fuse multiplications and
//! additions.

const S1: f64 = f64::from_bits(0xBFC5_5555_5555_5549);
const S2: f64 = f64::from_bits(0x3F81_1111_1110_F8A6);
const S3: f64 = f64::from_bits(0xBF2A_01A0_19C1_61D5);
const S4: f64 = f64::from_bits(0x3EC7_1DE3_57B1_FE7D);
const S5: f64 = f64::from_bits(0xBE5A_E5E6_8A2B_9CEB);
const S6: f64 = f64::from_bits(0x3DE5_D93A_5ACF_D57C);

const C1: f64 = f64::from_bits(0x3FA5_5555_5555_554C);
const C2: f64 = f64::from_bits(0xBF56_C16C_16C1_5177);
const C3: f64 = f64::from_bits(0x3EFA_01A0_19CB_1590);
const C4: f64 = f64::from_bits(0xBE92_7E4F_809C_52AD);
const C5: f64 = f64::from_bits(0x3E21_EE9E_BDB4_B1C4);
const C6: f64 = f64::from_bits(0xBDA8_FAE9_BE88_38D4);

const INVPIO2: f64 = f64::from_bits(0x3FE4_5F30_6DC9_C883);
const PIO2_1: f64 = f64::from_bits(0x3FF9_21FB_5440_0000);
const PIO2_1T: f64 = f64::from_bits(0x3DD0_B461_1A62_6331);
const PIO2_2: f64 = f64::from_bits(0x3DD0_B461_1A60_0000);
const PIO2_2T: f64 = f64::from_bits(0x3BA3_198A_2E03_7073);
const PIO2_3: f64 = f64::from_bits(0x3BA3_198A_2E00_0000);
const PIO2_3T: f64 = f64::from_bits(0x397B_839A_2520_49C1);

/// High words of n * pi/2 for n = 1..32 (fdlibm `npio2_hw`).
const NPIO2_HW: [i32; 32] = [
    0x3FF921FB, 0x400921FB, 0x4012D97C, 0x401921FB, 0x401F6A7A, 0x4022D97C, 0x4025FDBB, 0x402921FB,
    0x402C463A, 0x402F6A7A, 0x4031475C, 0x4032D97C, 0x40346B9C, 0x4035FDBB, 0x40378FDB, 0x403921FB,
    0x403AB41B, 0x403C463A, 0x403DD85A, 0x403F6A7A, 0x40407E4C, 0x4041475C, 0x4042106C, 0x4042D97C,
    0x4043A28C, 0x40446B9C, 0x404534AC, 0x4045FDBB, 0x4046C6CB, 0x40478FDB, 0x404858EB, 0x404921FB,
];

/// The high 32 bits of a double, as fdlibm's `GET_HIGH_WORD` into a signed integer.
fn high_word(x: f64) -> i32 {
    (x.to_bits() >> 32) as u32 as i32
}

/// sin(x + y) on [-pi/4, pi/4], y the tail of x (`iy == 0`: y is zero).
fn kernel_sin(x: f64, y: f64, iy: i32) -> f64 {
    let ix = high_word(x) & 0x7FFF_FFFF;
    if ix < 0x3E40_0000 && x as i32 == 0 {
        return x;
    }
    let z = x * x;
    let v = z * x;
    let r = S2 + z * (S3 + z * (S4 + z * (S5 + z * S6)));
    if iy == 0 { x + v * (S1 + z * r) } else { x - ((z * (0.5 * y - v * r) - y) - v * S1) }
}

/// cos(x + y) on [-pi/4, pi/4], y the tail of x.
fn kernel_cos(x: f64, y: f64) -> f64 {
    let ix = high_word(x) & 0x7FFF_FFFF;
    if ix < 0x3E40_0000 && x as i32 == 0 {
        return 1.0;
    }
    let z = x * x;
    let r = z * (C1 + z * (C2 + z * (C3 + z * (C4 + z * (C5 + z * C6)))));
    if ix < 0x3FD3_3333 {
        return 1.0 - (0.5 * z - (z * r - x * y));
    }
    let qx =
        if ix > 0x3FE9_0000 { 0.28125 } else { f64::from_bits(((ix - 0x0020_0000) as u32 as u64) << 32) };
    let hz = 0.5 * z - qx;
    let a = 1.0 - qx;
    a - (hz - (z * r - x * y))
}

/// x reduced modulo pi/2: (n, y0, y1) with x = n * pi/2 + y0 + y1, or `None` beyond the medium
/// range (|x| > 2^19 * pi/2), which needs fdlibm's `__kernel_rem_pio2`.
fn rem_pio2(x: f64) -> Option<(i32, f64, f64)> {
    let hx = high_word(x);
    let ix = hx & 0x7FFF_FFFF;
    if ix <= 0x3FE9_21FB {
        return Some((0, x, 0.0));
    }
    if ix < 0x4002_D97C {
        // |x| < 3pi/4: n = +-1.
        return Some(if hx > 0 {
            let z = x - PIO2_1;
            if ix != 0x3FF9_21FB {
                let y0 = z - PIO2_1T;
                (1, y0, (z - y0) - PIO2_1T)
            } else {
                let z = z - PIO2_2;
                let y0 = z - PIO2_2T;
                (1, y0, (z - y0) - PIO2_2T)
            }
        } else {
            let z = x + PIO2_1;
            if ix != 0x3FF9_21FB {
                let y0 = z + PIO2_1T;
                (-1, y0, (z - y0) + PIO2_1T)
            } else {
                let z = z + PIO2_2;
                let y0 = z + PIO2_2T;
                (-1, y0, (z - y0) + PIO2_2T)
            }
        });
    }
    if ix > 0x4139_21FB {
        return None;
    }
    let t = x.abs();
    let n = (t * INVPIO2 + 0.5) as i32;
    let f = n as f64;
    let mut r = t - f * PIO2_1;
    let mut w = f * PIO2_1T;
    let mut y0 = r - w;
    if !(n < 32 && ix != NPIO2_HW[(n - 1) as usize]) {
        let j = ix >> 20;
        let i = j - ((high_word(y0) >> 20) & 0x7FF);
        if i > 16 {
            // Second iteration, good to 118 bits.
            let t = r;
            w = f * PIO2_2;
            r = t - w;
            w = f * PIO2_2T - ((t - r) - w);
            y0 = r - w;
            let i = j - ((high_word(y0) >> 20) & 0x7FF);
            if i > 49 {
                // Third iteration, 151 bits.
                let t = r;
                w = f * PIO2_3;
                r = t - w;
                w = f * PIO2_3T - ((t - r) - w);
                y0 = r - w;
            }
        }
    }
    let y1 = (r - y0) - w;
    Some(if hx < 0 { (-n, -y0, -y1) } else { (n, y0, y1) })
}

/// `Math.sin`.
pub(crate) fn sin(x: f64) -> f64 {
    let ix = high_word(x) & 0x7FFF_FFFF;
    if ix <= 0x3FE9_21FB {
        return kernel_sin(x, 0.0, 0);
    }
    if ix >= 0x7FF0_0000 {
        return f64::NAN; // sin and cos of infinity or NaN
    }
    match rem_pio2(x) {
        Some((n, y0, y1)) => match n & 3 {
            0 => kernel_sin(y0, y1, 1),
            1 => kernel_cos(y0, y1),
            2 => -kernel_sin(y0, y1, 1),
            _ => -kernel_cos(y0, y1),
        },
        // Arguments beyond 2^19 * pi/2 never occur in the renderer (angles stay within a few
        // turns); the libm crate shares fdlibm's large-argument reduction.
        None => libm::sin(x),
    }
}

/// `Math.cos`.
pub(crate) fn cos(x: f64) -> f64 {
    let ix = high_word(x) & 0x7FFF_FFFF;
    if ix <= 0x3FE9_21FB {
        return kernel_cos(x, 0.0);
    }
    if ix >= 0x7FF0_0000 {
        return f64::NAN; // sin and cos of infinity or NaN
    }
    match rem_pio2(x) {
        Some((n, y0, y1)) => match n & 3 {
            0 => kernel_cos(y0, y1),
            1 => -kernel_sin(y0, y1, 1),
            2 => -kernel_cos(y0, y1),
            _ => kernel_sin(y0, y1, 1),
        },
        None => libm::cos(x),
    }
}

/// `Math.atan2`.
pub(crate) fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

/// `Math.acos`.
pub(crate) fn acos(x: f64) -> f64 {
    libm::acos(x)
}

/// `Math.hypot(a, b)` as V8 computes it.
pub(crate) fn hypot(a: f64, b: f64) -> f64 {
    let values = [a.abs(), b.abs()];
    let mut max = 0.0f64;
    let mut nan = false;
    for &v in &values {
        if v.is_nan() {
            nan = true;
        } else if v > max {
            max = v;
        }
    }
    if max == f64::INFINITY {
        return f64::INFINITY;
    }
    if nan {
        return f64::NAN;
    }
    if max == 0.0 {
        return 0.0;
    }
    let mut sum = 0.0f64;
    let mut compensation = 0.0f64;
    for &v in &values {
        let n = v / max;
        let summand = n * n - compensation;
        let preliminary = sum + summand;
        compensation = (preliminary - sum) - summand;
        sum = preliminary;
    }
    sum.sqrt() * max
}

/// `Math.round`: the nearest integer, halves toward +infinity.
pub(crate) fn round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn round_like_javascript() {
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(0.499_999_999_999_999_94), 0.0);
        assert_eq!(round(-0.6), -1.0);
        assert_eq!(round(12.0), 12.0);
        assert!(round(f64::NAN).is_nan());
    }

    #[test]
    fn hypot_like_v8() {
        assert_eq!(hypot(3.0, 4.0), 5.0);
        assert_eq!(hypot(0.0, -0.0), 0.0);
        assert_eq!(hypot(f64::NAN, f64::INFINITY), f64::INFINITY);
        assert!(hypot(f64::NAN, 1.0).is_nan());
        // Values printed by Node 22 (Math.hypot), as bit patterns.
        assert_eq!(hypot(0.1, 0.2).to_bits(), 0x3FCC_9F25_C5BF_EDDA);
        assert_eq!(hypot(1e-200, 3e-200).to_bits(), 0x1683_5D52_44B6_9495);
        assert_eq!(hypot(1.5, 2.25).to_bits(), 0x4005_A220_7349_0377);
    }

    #[test]
    fn sin_cos_values_of_v8() {
        // Values printed by Node 22 (Math.sin / Math.cos), as bit patterns.
        let cases: [(f64, u64, u64); 12] = [
            (0.5, 0x3FDE_AEE8_744B_05F0, 0x3FEC_1528_065B_7D50),
            (1.0, 0x3FEA_ED54_8F09_0CEE, 0x3FE1_4A28_0FB5_068C),
            (2.0, 0x3FED_18F6_EAD1_B446, 0xBFDA_A226_5753_7205),
            (PI, 0x3CA1_A626_3314_5C07, 0xBFF0_0000_0000_0000),
            (-4.0, 0x3FE8_37B9_DDDC_1EAE, 0xBFE4_EAA6_06DB_24C1),
            (6.0, 0xBFD1_E1F1_8AB0_A2C0, 0x3FEE_B9B7_0978_22F6),
            (PI / 4.0, 0x3FE6_A09E_667F_3BCC, 0x3FE6_A09E_667F_3BCD),
            (3.0 * PI / 4.0, 0x3FE6_A09E_667F_3BCD, 0xBFE6_A09E_667F_3BCC),
            (PI / 2.0, 0x3FF0_0000_0000_0000, 0x3C91_A626_3314_5C07),
            (3.0 * PI / 2.0, 0xBFF0_0000_0000_0000, 0xBCAA_7939_4C9E_8A0A),
            (5.0 * PI / 4.0, 0xBFE6_A09E_667F_3BCC, 0xBFE6_A09E_667F_3BCE),
            (7.0 * PI / 4.0, 0xBFE6_A09E_667F_3BCE, 0x3FE6_A09E_667F_3BCB),
        ];
        for (x, s, c) in cases {
            assert_eq!(sin(x).to_bits(), s, "sin({x})");
            assert_eq!(cos(x).to_bits(), c, "cos({x})");
        }
        assert!(sin(f64::INFINITY).is_nan());
        assert_eq!(sin(1e-300), 1e-300);
        assert_eq!(cos(1e-300), 1.0);
    }
}
