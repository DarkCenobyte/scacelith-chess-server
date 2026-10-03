//! Latency histograms and counters shared by the load tasks.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// Exact buckets below this value.
const LINEAR: u64 = 64;
/// Sub-buckets per power of two above it (about 3 % resolution).
const SUB: u64 = 32;
const BUCKETS: usize = (LINEAR + (64 - 6) * SUB) as usize;

/// A log-linear histogram of microseconds: exact below 64 µs, then 32 buckets per power of two.
/// The bucket math and the quantile definition (the bucket's middle, the exact maximum for 1)
/// are those of the Node load generator (`bench/lib/hist.js`), so figures stay comparable with
/// its older reports; the range goes past its 2^31 limit.
#[derive(Clone, Debug)]
pub struct Hist {
    counts: Vec<u64>,
    n: u64,
    sum: u128,
    max: u64,
}

impl Default for Hist {
    fn default() -> Hist {
        Hist { counts: vec![0; BUCKETS], n: 0, sum: 0, max: 0 }
    }
}

fn bucket_of(v: u64) -> usize {
    if v < LINEAR {
        return v as usize;
    }
    let k = 63 - u64::from(v.leading_zeros());
    let sub = (v >> (k - 5)) & (SUB - 1);
    (LINEAR + (k - 6) * SUB + sub) as usize
}

/// Middle of a bucket.
fn value_of(b: usize) -> u64 {
    let b = b as u64;
    if b < LINEAR {
        return b;
    }
    let k = (b - LINEAR) / SUB + 6;
    let sub = (b - LINEAR) % SUB;
    let width = 1u64 << (k - 5);
    ((SUB + sub) << (k - 5)) + width / 2
}

impl Hist {
    /// Records one value in microseconds.
    pub fn record_us(&mut self, us: u64) {
        self.counts[bucket_of(us)] += 1;
        self.n += 1;
        self.sum += u128::from(us);
        self.max = self.max.max(us);
    }

    /// Records a duration.
    pub fn record(&mut self, d: Duration) {
        self.record_us(d.as_micros().min(u128::from(u64::MAX)) as u64);
    }

    /// The value at quantile `q` (0..=1) in microseconds (the exact maximum for 1).
    pub fn quantile_us(&self, q: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        if q >= 1.0 {
            return self.max;
        }
        let rank = ((q * self.n as f64).ceil() as u64).max(1);
        let mut seen = 0;
        for (b, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return value_of(b).min(self.max);
            }
        }
        self.max
    }

    /// `{n, mean, p50, p90, p99, p999, max}` in milliseconds.
    pub fn summary_ms(&self) -> Value {
        let ms = |us: u64| round3(us as f64 / 1000.0);
        let mean = if self.n == 0 { 0.0 } else { self.sum as f64 / self.n as f64 / 1000.0 };
        json!({
            "n": self.n,
            "mean": round3(mean),
            "p50": ms(self.quantile_us(0.50)),
            "p90": ms(self.quantile_us(0.90)),
            "p99": ms(self.quantile_us(0.99)),
            "p999": ms(self.quantile_us(0.999)),
            "max": ms(self.max),
        })
    }
}

/// Rounds to 3 decimals (report readability).
pub fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// A histogram shared by tasks, recording only while measurement is on.
#[derive(Debug, Default)]
pub struct SharedHist(Mutex<Hist>);

impl SharedHist {
    /// Records a duration.
    pub fn record(&self, d: Duration) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).record(d);
    }

    /// A copy of the histogram.
    pub fn snapshot(&self) -> Hist {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Empties the histogram.
    pub fn reset(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Hist::default();
    }
}

/// Named counters and latency histograms of a scenario, with a measurement switch.
#[derive(Debug, Default)]
pub struct Stats {
    measuring: AtomicBool,
    counters: Mutex<BTreeMap<String, Arc<AtomicU64>>>,
    hists: Mutex<BTreeMap<String, Arc<SharedHist>>>,
}

impl Stats {
    /// A new set, measurement off.
    pub fn new() -> Arc<Stats> {
        Arc::new(Stats::default())
    }

    /// Whether latencies are recorded now.
    pub fn measuring(&self) -> bool {
        self.measuring.load(Ordering::Relaxed)
    }

    /// Turns recording on (histograms emptied first) or off.
    pub fn set_measuring(&self, on: bool) {
        if on {
            for h in self.hists.lock().unwrap_or_else(|p| p.into_inner()).values() {
                h.reset();
            }
        }
        self.measuring.store(on, Ordering::Relaxed);
    }

    /// The counter `name` (created at zero).
    pub fn counter(&self, name: &str) -> Arc<AtomicU64> {
        let mut map = self.counters.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(name.to_string()).or_default().clone()
    }

    /// Adds `n` to the counter `name`.
    pub fn add(&self, name: &str, n: u64) {
        self.counter(name).fetch_add(n, Ordering::Relaxed);
    }

    /// The histogram `name`.
    pub fn hist(&self, name: &str) -> Arc<SharedHist> {
        let mut map = self.hists.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(name.to_string()).or_default().clone()
    }

    /// Records into histogram `name` when measuring.
    pub fn latency(&self, name: &str, d: Duration) {
        if self.measuring() {
            self.hist(name).record(d);
        }
    }

    /// Every counter's value.
    pub fn counters(&self) -> BTreeMap<String, u64> {
        let map = self.counters.lock().unwrap_or_else(|p| p.into_inner());
        map.iter().map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed))).collect()
    }

    /// The counters as JSON.
    pub fn counters_json(&self) -> Value {
        Value::Object(self.counters().into_iter().map(|(k, v)| (k, json!(v))).collect())
    }

    /// Every histogram's summary as JSON.
    pub fn hists_json(&self) -> Value {
        let map = self.hists.lock().unwrap_or_else(|p| p.into_inner());
        Value::Object(map.iter().map(|(k, h)| (k.clone(), h.snapshot().summary_ms())).collect())
    }
}

/// Differences of counters between two snapshots.
pub fn counter_delta(before: &BTreeMap<String, u64>, after: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    after.iter().map(|(k, v)| (k.clone(), v - before.get(k).copied().unwrap_or(0))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_round_trip_within_resolution() {
        for v in [0u64, 1, 63, 64, 65, 100, 1000, 4096, 123_456, 10_000_000, u64::MAX / 2] {
            let mid = value_of(bucket_of(v));
            let err = (mid as f64 - v as f64).abs() / (v.max(1) as f64);
            assert!(err <= 0.032, "{v} -> {mid}");
        }
        assert!(bucket_of(u64::MAX) < BUCKETS);
    }

    #[test]
    fn quantiles() {
        let mut h = Hist::default();
        for us in 1..=1000 {
            h.record_us(us * 1000);
        }
        assert_eq!(h.n, 1000);
        let p50 = h.quantile_us(0.5) as f64;
        assert!((p50 - 500_000.0).abs() / 500_000.0 < 0.035, "{p50}");
        assert_eq!(h.quantile_us(1.0), 1_000_000);
        h.record_us(5_000_000);
        assert_eq!(h.quantile_us(1.0), 5_000_000);
        assert_eq!(Hist::default().summary_ms()["p99"], 0.0);
    }
}
