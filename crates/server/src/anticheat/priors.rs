//! Prior population statistics of the per-game analysis metrics, used until the server has its
//! own data: they are blended with it as [`PRIOR_GAMES`] pseudo-games, so real data replaces them
//! progressively, bucket by bucket. docs/ANTICHEAT.md section 4 gives their sources: lichess'
//! accuracy and average centipawn loss by rating, top-1 engine agreement of human players by
//! rating in non-trivial positions, and the finding that humans spend more time on harder
//! decisions. The accuracy row follows the ACPL row through the relation the analysis pipeline
//! measures (accuracy about 100 - 0.28 ACPL up to an ACPL of 90, flatter above). The standard
//! deviations are inflated ([`SD_INFLATION`]) so that, before real data exists, z-scores are
//! smaller than they should be.

use std::fmt;

use super::analysis::stats::clamp;

/// A per-game metric of the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Metric {
    /// Mean of the arithmetic and harmonic means of the move accuracies (0..100).
    Accuracy,
    /// Average centipawn loss.
    Acpl,
    /// Share of moves matching the deep search's best move.
    T1Deep,
    /// Share of moves matching the shallow search's best move.
    T1Fast,
    /// Share of complex positions where the deep best move was played.
    T1Complex,
    /// Rank correlation of think time and position complexity.
    TimeCorr,
    /// Coefficient of variation of the think times.
    TimeCv,
}

impl Metric {
    /// Every metric, in the model's order (the order of the stored statistics).
    pub const ALL: [Metric; 7] = [
        Metric::Accuracy,
        Metric::Acpl,
        Metric::T1Deep,
        Metric::T1Fast,
        Metric::T1Complex,
        Metric::TimeCorr,
        Metric::TimeCv,
    ];

    /// The name used in features, statistics keys and evidence (`t1Deep`...).
    pub fn name(self) -> &'static str {
        match self {
            Metric::Accuracy => "accuracy",
            Metric::Acpl => "acpl",
            Metric::T1Deep => "t1Deep",
            Metric::T1Fast => "t1Fast",
            Metric::T1Complex => "t1Complex",
            Metric::TimeCorr => "timeCorr",
            Metric::TimeCv => "timeCv",
        }
    }

    /// The metric of a name.
    pub fn parse(s: &str) -> Option<Metric> {
        Metric::ALL.into_iter().find(|m| m.name() == s)
    }

    /// Position in [`Metric::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Time class of a time control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeClass {
    Bullet,
    Blitz,
    Rapid,
    Classical,
}

impl TimeClass {
    /// The name (`bullet`, `blitz`, `rapid`, `classical`).
    pub fn name(self) -> &'static str {
        match self {
            TimeClass::Bullet => "bullet",
            TimeClass::Blitz => "blitz",
            TimeClass::Rapid => "rapid",
            TimeClass::Classical => "classical",
        }
    }

    fn adjustment(self) -> &'static TimeClassAdjustment {
        &TIME_CLASS[self as usize]
    }
}

const RATINGS: [f64; 6] = [600.0, 1000.0, 1500.0, 2000.0, 2500.0, 2900.0];

struct Row {
    mean: [f64; 6],
    sd: [f64; 6],
}

// Mean and per-game standard deviation at each rating of RATINGS, in Metric::ALL order.
const TABLE: [Row; 7] = [
    Row { mean: [65.0, 71.0, 79.0, 86.0, 91.0, 93.0], sd: [10.0, 9.0, 8.0, 6.5, 5.0, 4.5] },
    Row { mean: [150.0, 110.0, 75.0, 50.0, 32.0, 24.0], sd: [70.0, 55.0, 40.0, 28.0, 18.0, 14.0] },
    Row { mean: [0.30, 0.35, 0.42, 0.49, 0.56, 0.60], sd: [0.10, 0.10, 0.10, 0.10, 0.09, 0.09] },
    Row { mean: [0.30, 0.35, 0.41, 0.47, 0.53, 0.56], sd: [0.10, 0.10, 0.10, 0.10, 0.09, 0.09] },
    Row { mean: [0.28, 0.32, 0.37, 0.42, 0.47, 0.50], sd: [0.17, 0.17, 0.17, 0.17, 0.16, 0.16] },
    Row { mean: [0.15, 0.17, 0.20, 0.22, 0.24, 0.25], sd: [0.25, 0.25, 0.25, 0.25, 0.25, 0.25] },
    Row { mean: [1.00, 1.00, 1.00, 1.00, 1.00, 1.00], sd: [0.35, 0.35, 0.35, 0.35, 0.35, 0.35] },
];

// Faster games: humans are less accurate, agree less with the engine, and have less room to
// spend time where it matters.
struct TimeClassAdjustment {
    accuracy: f64,
    acpl_mul: f64,
    t1: f64,
    time_corr: f64,
    time_cv: f64,
}

const TIME_CLASS: [TimeClassAdjustment; 4] = [
    TimeClassAdjustment { accuracy: -6.0, acpl_mul: 1.35, t1: -0.05, time_corr: -0.07, time_cv: -0.15 },
    TimeClassAdjustment { accuracy: -2.0, acpl_mul: 1.10, t1: -0.02, time_corr: -0.02, time_cv: -0.05 },
    TimeClassAdjustment { accuracy: 0.0, acpl_mul: 1.00, t1: 0.0, time_corr: 0.0, time_cv: 0.0 },
    TimeClassAdjustment { accuracy: 2.0, acpl_mul: 0.90, t1: 0.02, time_corr: 0.02, time_cv: 0.05 },
];

/// Weight of the prior, in games, when blended with the server's own statistics.
pub const PRIOR_GAMES: f64 = 40.0;
/// Prior standard deviations are multiplied by this (conservative until real data exists).
pub const SD_INFLATION: f64 = 1.25;

/// Time class of a time control, from the estimated game duration base + 40 increments
/// (lichess' convention): under 3 min bullet, under 10 blitz, under 30 rapid, else classical.
pub fn time_class(base_ms: f64, inc_ms: f64) -> TimeClass {
    let est = base_ms / 1000.0 + 40.0 * inc_ms / 1000.0;
    if est < 180.0 {
        TimeClass::Bullet
    } else if est < 600.0 {
        TimeClass::Blitz
    } else if est < 1800.0 {
        TimeClass::Rapid
    } else {
        TimeClass::Classical
    }
}

/// Time class of a category id such as `3+2` (`custom` and anything else count as rapid).
pub fn time_class_of_category(category: &str) -> TimeClass {
    let parsed = category.split_once('+').and_then(|(b, i)| {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
        (digits(b) && digits(i)).then(|| (b.parse::<f64>().ok(), i.parse::<f64>().ok()))
    });
    match parsed {
        Some((Some(base_min), Some(inc_sec))) => time_class(base_min * 60000.0, inc_sec * 1000.0),
        _ => TimeClass::Rapid,
    }
}

fn interp(arr: &[f64; 6], rating: f64) -> f64 {
    let r = clamp(rating, RATINGS[0], RATINGS[5]);
    for i in 0..RATINGS.len() - 1 {
        if r <= RATINGS[i + 1] {
            let t = (r - RATINGS[i]) / (RATINGS[i + 1] - RATINGS[i]);
            return arr[i] + t * (arr[i + 1] - arr[i]);
        }
    }
    arr[5]
}

/// Mean and standard deviation of a distribution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prior {
    pub mean: f64,
    pub sd: f64,
}

/// Prior distribution of a metric for players of `rating` in a time class.
pub fn prior_for(metric: Metric, rating: f64, tc: TimeClass) -> Prior {
    let row = &TABLE[metric.index()];
    let adj = tc.adjustment();
    let mut mean = interp(&row.mean, rating);
    let mut sd = interp(&row.sd, rating) * SD_INFLATION;
    match metric {
        Metric::Accuracy => mean = clamp(mean + adj.accuracy, 0.0, 100.0),
        Metric::Acpl => {
            mean *= adj.acpl_mul;
            sd *= adj.acpl_mul;
        }
        Metric::T1Deep | Metric::T1Fast | Metric::T1Complex => mean = clamp(mean + adj.t1, 0.0, 1.0),
        Metric::TimeCorr => mean += adj.time_corr,
        Metric::TimeCv => mean += adj.time_cv,
    }
    Prior { mean, sd }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensible_ordering_by_rating_and_time_control() {
        let p = |m, r| prior_for(m, r, TimeClass::Rapid).mean;
        assert!(p(Metric::Accuracy, 2400.0) > p(Metric::Accuracy, 1200.0));
        assert!(p(Metric::Acpl, 2400.0) < p(Metric::Acpl, 1200.0));
        assert!(
            prior_for(Metric::Accuracy, 1500.0, TimeClass::Bullet).mean
                < prior_for(Metric::Accuracy, 1500.0, TimeClass::Classical).mean
        );
        assert_eq!(time_class(60000.0, 0.0), TimeClass::Bullet);
        assert_eq!(time_class(180000.0, 2000.0), TimeClass::Blitz);
        assert_eq!(time_class(900000.0, 10000.0), TimeClass::Rapid);
        assert_eq!(time_class(1800000.0, 20000.0), TimeClass::Classical);
        assert_eq!(time_class_of_category("3+2"), TimeClass::Blitz);
        assert_eq!(time_class_of_category("1+0"), TimeClass::Bullet);
        assert_eq!(time_class_of_category("90+30"), TimeClass::Classical);
        for other in ["custom", "3+", "+2", "3+2+1", "a+b", ""] {
            assert_eq!(time_class_of_category(other), TimeClass::Rapid, "{other}");
        }
    }

    #[test]
    fn the_accuracy_row_follows_the_measured_accuracy_acpl_relation() {
        // accuracy ~ 100 - 0.28 ACPL up to an ACPL of 90, and the measured values above it at
        // the ACPL of the two lowest ratings (docs/ANTICHEAT.md section 4).
        for rating in RATINGS {
            let acpl = prior_for(Metric::Acpl, rating, TimeClass::Rapid).mean;
            let accuracy = prior_for(Metric::Accuracy, rating, TimeClass::Rapid).mean;
            let expected = if acpl <= 90.0 {
                100.0 - 0.28 * acpl
            } else if acpl == 110.0 {
                71.0
            } else if acpl == 150.0 {
                65.0
            } else {
                panic!("rating {rating}: no measured accuracy for an ACPL of {acpl}")
            };
            assert!(
                (accuracy - expected).abs() <= 0.5,
                "rating {rating}: accuracy {accuracy}, relation {expected}"
            );
        }
    }

    #[test]
    fn interpolation_adjustments_and_names() {
        let p = prior_for(Metric::Acpl, 1250.0, TimeClass::Bullet);
        assert_eq!(p.mean, (110.0 + 0.5 * (75.0 - 110.0)) * 1.35);
        assert_eq!(p.sd, (55.0 + 0.5 * (40.0 - 55.0)) * SD_INFLATION * 1.35);
        assert_eq!(prior_for(Metric::TimeCv, 100.0, TimeClass::Rapid).mean, 1.0, "ratings are clamped");
        assert_eq!(
            prior_for(Metric::T1Deep, 5000.0, TimeClass::Classical).mean,
            0.56 + 1.0 * (0.60 - 0.56) + 0.02
        );
        for m in Metric::ALL {
            assert_eq!(Metric::parse(m.name()), Some(m));
        }
        assert_eq!(Metric::parse("top3"), None);
        assert_eq!(TimeClass::Classical.name(), "classical");
    }
}
