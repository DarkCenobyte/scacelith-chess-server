//! Numeric helpers that reproduce the former JavaScript server bit for bit, so that features,
//! scores and evidence computed by the Rust server equal the ones already stored: `Math.round`,
//! `Number.prototype.toFixed`, V8's `Math.exp` (an fdlibm port, which differs from the C
//! library's `exp` in about one result out of ten) and JSON numbers written the JavaScript way
//! (integral values without a fraction).

use serde_json::Value;

/// `Math.round`: the closest integer, halves rounded towards positive infinity.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    // x - f is exact: both are within one unit of each other.
    if x - f >= 0.5 { f + 1.0 } else { f }
}

/// `Math.round(x * 10^d) / 10^d`, the rounding of the former server's features and scores.
pub fn round_to(x: f64, d: u32) -> f64 {
    let f = 10f64.powi(d as i32);
    js_round(x * f) / f
}

/// `Number.prototype.toFixed(d)`: the exact decimal value of `x`, rounded half up (away from
/// zero for negative values) to `d` digits after the point.
pub fn to_fixed(x: f64, d: usize) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.abs() >= 1e21 {
        return js_number(x);
    }
    // 1100 digits hold the exact expansion of every f64 below 1e21.
    let exact = format!("{:.1100}", x.abs());
    let (int_part, frac_part) = exact.split_once('.').expect("fixed notation has a point");
    let mut digits: Vec<u8> = int_part.bytes().chain(frac_part.bytes().take(d)).collect();
    if frac_part.as_bytes()[d] >= b'5' {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let split = digits.len() - d;
    let mut out = String::with_capacity(digits.len() + 2);
    if x < 0.0 {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&digits[..split]).expect("ASCII digits"));
    if d > 0 {
        out.push('.');
        out.push_str(std::str::from_utf8(&digits[split..]).expect("ASCII digits"));
    }
    out
}

/// A number as JavaScript's `String(x)` writes it (template literals, messages): the shortest
/// digits that read back as `x`, in fixed notation from 1e-6 to 1e21, else exponential.
pub fn js_number(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if x == 0.0 {
        return "0".to_string();
    }
    // `{:e}` gives the shortest round-trip digits: "d.ddde-n".
    let e = format!("{:e}", x.abs());
    let (mantissa, exp) = e.split_once('e').expect("exponential notation");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().expect("exponent") + 1;
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if n - 1 < 0 { '-' } else { '+' });
        out.push_str(&(n - 1).abs().to_string());
    }
    out
}

/// A JSON number as `JSON.stringify` writes it: integral values as integers, non-finite values
/// as `null`.
pub fn json_num(x: f64) -> Value {
    if !x.is_finite() {
        return Value::Null;
    }
    if x == x.trunc() && x.abs() < 9_007_199_254_740_992.0 {
        return Value::from(x as i64);
    }
    Value::from(x)
}

/// [`json_num`] of an optional value (`None` is `null`).
pub fn json_opt(x: Option<f64>) -> Value {
    x.map_or(Value::Null, json_num)
}

/// `JSON.stringify` of a value, numbers written as JavaScript writes them (the tests compare
/// records with the former server's output).
#[cfg(test)]
pub fn js_json(v: &Value) -> String {
    match v {
        Value::Number(n) if n.is_f64() => js_number(n.as_f64().expect("a float")),
        Value::Array(a) => format!("[{}]", a.iter().map(js_json).collect::<Vec<_>>().join(",")),
        Value::Object(m) => {
            let fields: Vec<String> =
                m.iter().map(|(k, v)| format!("{}:{}", Value::from(k.as_str()), js_json(v))).collect();
            format!("{{{}}}", fields.join(","))
        }
        other => other.to_string(),
    }
}

/// JavaScript truthiness of a JSON field (a missing field is falsy).
pub fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// Unary `+` of a JSON field as JavaScript applies it to a stored value: a missing field is NaN,
/// `null` and `false` are 0, `true` is 1, decimal text is its number (blank text is 0), anything
/// else is NaN.
pub fn js_to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
            match t {
                "" => 0.0,
                "Infinity" | "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                _ if t
                    .bytes()
                    .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-')) =>
                {
                    t.parse().unwrap_or(f64::NAN)
                }
                _ => f64::NAN,
            }
        }
        Some(_) => f64::NAN,
    }
}

fn hi_word(x: f64) -> u32 {
    (x.to_bits() >> 32) as u32
}

fn from_words(hi: u32, lo: u32) -> f64 {
    f64::from_bits((u64::from(hi) << 32) | u64::from(lo))
}

/// V8's `Math.exp`: fdlibm's `__ieee754_exp`, with V8's exact result for `exp(1)`.
// The constants keep fdlibm's digits, so they can be checked against its source.
#[allow(clippy::excessive_precision, clippy::approx_constant)]
pub fn js_exp(x: f64) -> f64 {
    const HALF: [f64; 2] = [0.5, -0.5];
    const O_THRESHOLD: f64 = 7.097_827_128_933_839_730_96e2;
    const U_THRESHOLD: f64 = -7.451_332_191_019_411_084_20e2;
    const LN2_HI: [f64; 2] = [6.931_471_803_691_238_164_90e-1, -6.931_471_803_691_238_164_90e-1];
    const LN2_LO: [f64; 2] = [1.908_214_929_270_587_700_02e-10, -1.908_214_929_270_587_700_02e-10];
    const INV_LN2: f64 = 1.442_695_040_888_963_387_00;
    const P1: f64 = 1.666_666_666_666_660_190_37e-1;
    const P2: f64 = -2.777_777_777_701_559_338_42e-3;
    const P3: f64 = 6.613_756_321_437_934_361_17e-5;
    const P4: f64 = -1.653_390_220_546_525_153_90e-6;
    const P5: f64 = 4.138_136_797_057_238_460_39e-8;
    const HUGE: f64 = 1.0e300;
    const TWO_M1000: f64 = 9.332_636_185_032_188_789_90e-302;
    const TWO_1023: f64 = 8.988_465_674_311_579_539e307;

    let mut x = x;
    let mut hx = hi_word(x);
    let xsb = ((hx >> 31) & 1) as usize;
    hx &= 0x7fff_ffff;
    let (mut hi, mut lo) = (0.0, 0.0);
    let k: i32;
    if hx >= 0x4086_2E42 {
        if hx >= 0x7ff0_0000 {
            if ((hx & 0xf_ffff) | x.to_bits() as u32) != 0 {
                return x + x; // NaN
            }
            return if xsb == 0 { x } else { 0.0 };
        }
        if x > O_THRESHOLD {
            return f64::INFINITY;
        }
        if x < U_THRESHOLD {
            return 0.0;
        }
    }
    if hx > 0x3fd6_2e42 {
        if hx < 0x3FF0_A2B2 {
            if x == 1.0 {
                return std::f64::consts::E;
            }
            hi = x - LN2_HI[xsb];
            lo = LN2_LO[xsb];
            k = 1 - 2 * xsb as i32;
        } else {
            k = (INV_LN2 * x + HALF[xsb]) as i32;
            let t = f64::from(k);
            hi = x - t * LN2_HI[0];
            lo = t * LN2_LO[0];
        }
        x = hi - lo;
    } else if hx < 0x3e30_0000 {
        if HUGE + x > 1.0 {
            return 1.0 + x;
        }
        k = 0;
    } else {
        k = 0;
    }
    let t = x * x;
    let twopk = if k >= -1021 {
        from_words((0x3ff0_0000 + (k << 20)) as u32, 0)
    } else {
        from_words((0x3ff0_0000 + ((k + 1000) << 20)) as u32, 0)
    };
    let c = x - t * (P1 + t * (P2 + t * (P3 + t * (P4 + t * P5))));
    if k == 0 {
        return 1.0 - ((x * c) / (c - 2.0) - x);
    }
    let y = 1.0 - ((lo - (x * c) / (2.0 - c)) - hi);
    if k >= -1021 {
        if k == 1024 {
            return y * 2.0 * TWO_1023;
        }
        y * twopk
    } else {
        y * twopk * TWO_M1000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_round_rounds_halves_up() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(0.49999999999999994), 0.0);
        assert_eq!(js_round(-0.6), -1.0);
        assert_eq!(round_to(1.005, 2), 1.0, "1.005 is below the half in binary");
        assert_eq!(round_to(2.0 / 3.0, 4), 0.6667);
    }

    #[test]
    fn to_fixed_matches_javascript() {
        // Values checked with Node 22.
        let cases: [(f64, usize, &str); 14] = [
            (0.125, 2, "0.13"),
            (1.005, 2, "1.00"),
            (2.5, 0, "3"),
            (-2.5, 0, "-3"),
            (-0.0, 2, "0.00"),
            (-0.001, 2, "-0.00"),
            (99.95, 1, "100.0"),
            (9.995, 2, "9.99"),
            (0.0, 1, "0.0"),
            (79.4, 1, "79.4"),
            (1234.5678, 2, "1234.57"),
            (3.499999, 2, "3.50"),
            (1e21, 2, "1e+21"),
            (0.000001, 3, "0.000"),
        ];
        for (x, d, want) in cases {
            assert_eq!(to_fixed(x, d), want, "{x}.toFixed({d})");
        }
    }

    #[test]
    fn numbers_are_written_as_javascript_writes_them() {
        let cases: [(f64, &str); 10] = [
            (3.0, "3"),
            (-0.0, "0"),
            (0.1, "0.1"),
            (1e21, "1e+21"),
            (123456789012345680000.0, "123456789012345680000"),
            (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"),
            (-2.5, "-2.5"),
            (1e-7, "1e-7"),
            (2.0f64.powi(70), "1.1805916207174113e+21"),
        ];
        for (x, want) in cases {
            assert_eq!(js_number(x), want);
        }
    }

    #[test]
    fn json_numbers_are_written_as_javascript_writes_them() {
        assert_eq!(json_num(3.0).to_string(), "3");
        assert_eq!(json_num(-0.0).to_string(), "0");
        assert_eq!(json_num(0.1).to_string(), "0.1");
        assert_eq!(json_num(f64::NAN), Value::Null);
        assert_eq!(json_opt(None), Value::Null);
    }

    #[test]
    fn unary_plus_of_stored_values() {
        use serde_json::json;
        assert!(js_to_number(None).is_nan());
        assert_eq!(js_to_number(Some(&Value::Null)), 0.0);
        assert_eq!(js_to_number(Some(&json!(true))), 1.0);
        assert_eq!(js_to_number(Some(&json!(2.5))), 2.5);
        assert_eq!(js_to_number(Some(&json!(" 12.5e1\n"))), 125.0);
        assert_eq!(js_to_number(Some(&json!(""))), 0.0);
        assert_eq!(js_to_number(Some(&json!("-Infinity"))), f64::NEG_INFINITY);
        for nan in [json!("inf"), json!("NaN"), json!("1e"), json!("."), json!({})] {
            assert!(js_to_number(Some(&nan)).is_nan(), "{nan}");
        }
    }

    #[test]
    fn exp_follows_v8() {
        assert_eq!(js_exp(1.0), std::f64::consts::E);
        assert_eq!(js_exp(0.0), 1.0);
        assert_eq!(js_exp(f64::NEG_INFINITY), 0.0);
        assert_eq!(js_exp(f64::INFINITY), f64::INFINITY);
        assert!(js_exp(f64::NAN).is_nan());
        assert_eq!(js_exp(710.0), f64::INFINITY);
        assert_eq!(js_exp(-746.0), 0.0);
        // Inputs where the C library's exp differs from V8's (bit patterns from Node 22).
        for (x, bits) in [
            (0x3ffa_6695_8e00_0000_u64, 0x4014_d42f_e792_1a70_u64),
            (0xbfec_a58b_2ab4_b0d5, 0x3fda_253c_1ced_b196),
            (0xbfca_f50d_313f_6a97, 0x3fe9_ec4c_fc87_1bf2),
            (0x3ffc_dd37_dfe0_0000, 0x4018_4bb8_5265_71b6),
            (0xbff1_7321_fe39_90ab, 0x3fd5_8138_3715_ddf6),
            (0xbffc_9689_3bb5_8106, 0x3fc5_70bf_e221_0cb6),
        ] {
            let x = f64::from_bits(x);
            assert_eq!(js_exp(x).to_bits(), bits, "exp({x})");
        }
        assert_eq!(js_exp(0.5).to_bits(), 0x3ffa_6129_8e1e_069c);
        assert_eq!(js_exp(2.1).to_bits(), 0x4020_5514_3908_1d4e);
    }
}
