//! Small numeric helpers of the analysis and the scoring model, with the former server's
//! operation order (results are bit-identical).

use crate::anticheat::num::js_exp;

/// Win probability in percent for a centipawn score (lichess' logistic model).
pub fn win_percent(cp: f64) -> f64 {
    let c = clamp(cp, -1000.0, 1000.0);
    50.0 + 50.0 * (2.0 / (1.0 + js_exp(-0.00368208 * c)) - 1.0)
}

/// Accuracy of one move (0..=100) from the mover's win-probability drop, as lichess computes it
/// (including its +1 "imperfect analysis" bonus).
pub fn move_accuracy(win_before: f64, win_after: f64) -> f64 {
    let d = (win_before - win_after).max(0.0);
    let a = 103.1668100711649 * js_exp(-0.04354415386753951 * d) - 3.166924740191411 + 1.0;
    clamp(a, 0.0, 100.0)
}

/// Arithmetic mean (NaN for an empty list).
pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut s = 0.0;
    for &x in xs {
        s += x;
    }
    s / xs.len() as f64
}

/// Harmonic mean of positive values (values below `floor` count as `floor`).
pub fn harmonic_mean(xs: &[f64], floor: f64) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut s = 0.0;
    for &x in xs {
        s += 1.0 / floor.max(x);
    }
    xs.len() as f64 / s
}

/// Sample standard deviation (0 for fewer than two values).
pub fn stdev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let mut s = 0.0;
    for &x in xs {
        s += (x - m) * (x - m);
    }
    (s / (xs.len() - 1) as f64).sqrt()
}

/// Ranks with ties averaged (1-based).
pub fn ranks(xs: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..xs.len()).collect();
    idx.sort_by(|&a, &b| xs[a].partial_cmp(&xs[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut r = vec![0.0; xs.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && xs[idx[j + 1]] == xs[idx[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for &k in &idx[i..=j] {
            r[k] = avg;
        }
        i = j + 1;
    }
    r
}

/// Pearson correlation; `None` when a side has no variance or there are fewer than 3 points.
pub fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let n = xs.len();
    if n < 3 || ys.len() != n {
        return None;
    }
    let (mx, my) = (mean(xs), mean(ys));
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (dx, dy) = (xs[i] - mx, ys[i] - my);
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx <= 0.0 || syy <= 0.0 {
        return None;
    }
    Some(sxy / (sxx * syy).sqrt())
}

/// Spearman rank correlation (Pearson of the tie-averaged ranks); `None` when undefined.
pub fn spearman(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() != ys.len() || xs.len() < 3 {
        return None;
    }
    pearson(&ranks(xs), &ranks(ys))
}

/// Coefficient of variation (sd / mean); `None` when the mean is not positive.
pub fn coefficient_of_variation(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let m = mean(xs);
    if m.is_nan() || m <= 0.0 {
        return None;
    }
    Some(stdev(xs) / m)
}

/// Running statistics of one metric (Welford): count, mean and sum of squared deviations.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Welford {
    pub n: f64,
    pub mean: f64,
    pub m2: f64,
}

impl Welford {
    /// Adds one value.
    pub fn push(&mut self, x: f64) {
        let n = self.n + 1.0;
        let delta = x - self.mean;
        let mean = self.mean + delta / n;
        self.m2 += delta * (x - mean);
        self.n = n;
        self.mean = mean;
    }

    /// Sample variance (0 below two values).
    pub fn variance(&self) -> f64 {
        if self.n > 1.0 { self.m2 / (self.n - 1.0) } else { 0.0 }
    }
}

/// Clamps `x` into `[lo, hi]` (NaN stays NaN).
pub fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    }
}

/// Upper tail of the standard normal distribution, P(Z >= z) (Abramowitz-Stegun 7.1.26).
pub fn normal_tail(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let erfc = t
        * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))))
        * js_exp(-x * x);
    if z >= 0.0 { erfc / 2.0 } else { 1.0 - erfc / 2.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_helpers() {
        assert_eq!(ranks(&[10.0, 20.0, 20.0, 5.0]), [2.0, 3.5, 3.5, 1.0]);
        assert_eq!(spearman(&[1.0, 2.0, 3.0, 4.0], &[10.0, 20.0, 30.0, 40.0]), Some(1.0));
        assert!((spearman(&[1.0, 2.0, 3.0, 4.0], &[4.0, 3.0, 2.0, 1.0]).unwrap() + 1.0).abs() < 1e-12);
        assert_eq!(spearman(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]), None);
        assert_eq!(win_percent(0.0), 50.0);
        assert!(win_percent(1000.0) > 97.0 && win_percent(5000.0) == win_percent(1000.0));
        assert_eq!(move_accuracy(60.0, 60.0), 100.0);
        assert!(move_accuracy(80.0, 30.0) < 15.0);
        assert!(coefficient_of_variation(&[1.0, 1.0, 1.0, 1.0]).unwrap().abs() < 1e-12);
        let mut s = Welford::default();
        for x in [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0] {
            s.push(x);
        }
        assert_eq!(s.mean, 5.0);
        assert!((s.variance() - 32.0 / 7.0).abs() < 1e-12);
        assert!((normal_tail(1.96) - 0.025).abs() < 1e-3);
        assert!((normal_tail(-1.0) - 0.8413).abs() < 1e-3);
    }

    #[test]
    fn edge_cases() {
        assert!(mean(&[]).is_nan());
        assert!(harmonic_mean(&[], 1.0).is_nan());
        assert_eq!(harmonic_mean(&[0.0, 4.0], 1.0), 2.0 / 1.25, "values below the floor count as the floor");
        assert_eq!(stdev(&[3.0]), 0.0);
        assert_eq!(pearson(&[1.0, 2.0], &[1.0, 2.0]), None);
        assert_eq!(pearson(&[1.0, 2.0, 3.0], &[1.0, 2.0]), None);
        assert_eq!(coefficient_of_variation(&[0.0, 0.0]), None);
        assert_eq!(coefficient_of_variation(&[5.0]), None);
        assert_eq!(Welford::default().variance(), 0.0);
        assert!(clamp(f64::NAN, 0.0, 1.0).is_nan());
    }
}
