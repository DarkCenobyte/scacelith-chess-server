//! Prometheus metrics.
//!
//! Metrics are registered once (usually in a `static` built with [`std::sync::LazyLock`]) and
//! updated with atomics, so the hot paths never lock. Registering the same name and kind twice
//! returns the existing metric; another kind panics (a programming error). The registry renders
//! the Prometheus text format 0.0.4 in registration order; label children render in creation
//! order. Numbers print like JavaScript's `String(n)`: integers without a fraction, `+Inf`.
//!
//! ```ignore
//! static MOVES: LazyLock<CounterVec> =
//!     LazyLock::new(|| metrics::counter_vec("scacelith_moves_total", "Moves played.", &["result"]));
//! MOVES.with(&["ok"]).inc();
//! ```

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Counter,
    Gauge,
    GaugeFn,
    Histogram,
}

impl Kind {
    fn type_name(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge | Kind::GaugeFn => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

#[derive(Debug, Default)]
struct F64Cell(AtomicU64);

impl F64Cell {
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }

    fn add(&self, d: f64) {
        let mut cur = self.0.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(cur) + d).to_bits();
            match self.0.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(v) => cur = v,
            }
        }
    }
}

/// A monotonically increasing count.
#[derive(Clone, Debug)]
pub struct Counter(Arc<AtomicU64>);

impl Counter {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A value that goes up and down.
#[derive(Clone, Debug)]
pub struct Gauge(Arc<F64Cell>);

impl Gauge {
    pub fn set(&self, v: f64) {
        self.0.set(v);
    }

    pub fn add(&self, d: f64) {
        self.0.add(d);
    }

    pub fn inc(&self) {
        self.0.add(1.0);
    }

    pub fn dec(&self) {
        self.0.add(-1.0);
    }

    pub fn get(&self) -> f64 {
        self.0.get()
    }
}

#[derive(Debug)]
struct HistInner {
    bounds: Arc<[f64]>,
    counts: Box<[AtomicU64]>,
    sum: F64Cell,
    count: AtomicU64,
}

/// A distribution over fixed buckets (`v <= bound`).
#[derive(Clone, Debug)]
pub struct Histogram(Arc<HistInner>);

impl Histogram {
    pub fn observe(&self, v: f64) {
        let i = self.0.bounds.iter().position(|b| v <= *b).unwrap_or(self.0.bounds.len());
        self.0.counts[i].fetch_add(1, Ordering::Relaxed);
        self.0.sum.add(v);
        self.0.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.0.count.load(Ordering::Relaxed)
    }

    pub fn sum(&self) -> f64 {
        self.0.sum.get()
    }
}

#[derive(Clone, Debug)]
enum Child {
    Counter(Counter),
    Gauge(Gauge),
    Histogram(Histogram),
}

type GaugeFnBox = Box<dyn Fn() -> f64 + Send + Sync>;

struct Family {
    name: String,
    help: String,
    kind: Kind,
    labels: Vec<String>,
    bounds: Arc<[f64]>,
    children: RwLock<IndexMap<Vec<String>, Child>>,
    func: Option<GaugeFnBox>,
    last_fn_value: F64Cell,
}

impl Family {
    fn child(&self, values: &[&str]) -> Child {
        assert_eq!(values.len(), self.labels.len(), "metric {}: wrong number of label values", self.name);
        if let Some(c) = self.children.read().get(&values.iter().map(|s| s.to_string()).collect::<Vec<_>>()) {
            return c.clone();
        }
        let key: Vec<String> = values.iter().map(|s| s.to_string()).collect();
        let mut w = self.children.write();
        w.entry(key)
            .or_insert_with(|| match self.kind {
                Kind::Counter => Child::Counter(Counter(Arc::new(AtomicU64::new(0)))),
                Kind::Gauge | Kind::GaugeFn => Child::Gauge(Gauge(Arc::new(F64Cell::default()))),
                Kind::Histogram => Child::Histogram(Histogram(Arc::new(HistInner {
                    bounds: self.bounds.clone(),
                    counts: (0..=self.bounds.len()).map(|_| AtomicU64::new(0)).collect(),
                    sum: F64Cell::default(),
                    count: AtomicU64::new(0),
                }))),
            })
            .clone()
    }
}

/// The metric registry.
pub struct Registry {
    families: Mutex<Vec<Arc<Family>>>,
    by_name: Mutex<HashMap<String, Arc<Family>>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry").field("families", &self.families.lock().len()).finish()
    }
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

/// The process registry.
pub fn registry() -> &'static Registry {
    &REGISTRY
}

impl Registry {
    pub fn new() -> Registry {
        Registry { families: Mutex::new(Vec::new()), by_name: Mutex::new(HashMap::new()) }
    }

    fn define(&self, name: &str, help: &str, kind: Kind, labels: &[&str], bounds: &[f64], func: Option<GaugeFnBox>) -> Arc<Family> {
        let mut by_name = self.by_name.lock();
        if let Some(f) = by_name.get(name) {
            assert_eq!(f.kind, kind, "metric {name} redefined with another kind");
            return f.clone();
        }
        let fam = Arc::new(Family {
            name: name.to_string(),
            help: help.to_string(),
            kind,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            bounds: bounds.into(),
            children: RwLock::new(IndexMap::new()),
            func,
            last_fn_value: F64Cell::default(),
        });
        if labels.is_empty() && kind != Kind::GaugeFn {
            fam.child(&[]);
        }
        by_name.insert(name.to_string(), fam.clone());
        self.families.lock().push(fam.clone());
        fam
    }

    /// Renders every metric in the Prometheus text format 0.0.4.
    pub fn render(&self) -> String {
        let fams: Vec<Arc<Family>> = self.families.lock().clone();
        let mut out = String::with_capacity(fams.len() * 128);
        for f in fams {
            let _ = writeln!(out, "# HELP {} {}", f.name, f.help);
            let _ = writeln!(out, "# TYPE {} {}", f.name, f.kind.type_name());
            if let Some(func) = &f.func {
                let v = std::panic::catch_unwind(std::panic::AssertUnwindSafe(func)).unwrap_or(f64::NAN);
                if v.is_finite() {
                    f.last_fn_value.set(v);
                }
                let _ = writeln!(out, "{} {}", f.name, js_number(f.last_fn_value.get()));
                continue;
            }
            for (values, child) in f.children.read().iter() {
                let labels = render_labels(&f.labels, values, None);
                match child {
                    Child::Counter(c) => {
                        let _ = writeln!(out, "{}{} {}", f.name, labels, c.get());
                    }
                    Child::Gauge(g) => {
                        let _ = writeln!(out, "{}{} {}", f.name, labels, js_number(g.get()));
                    }
                    Child::Histogram(h) => {
                        let mut cum = 0u64;
                        for (i, b) in h.0.bounds.iter().enumerate() {
                            cum += h.0.counts[i].load(Ordering::Relaxed);
                            let l = render_labels(&f.labels, values, Some(&js_number(*b)));
                            let _ = writeln!(out, "{}_bucket{} {}", f.name, l, cum);
                        }
                        cum += h.0.counts[h.0.bounds.len()].load(Ordering::Relaxed);
                        let l = render_labels(&f.labels, values, Some("+Inf"));
                        let _ = writeln!(out, "{}_bucket{} {}", f.name, l, cum);
                        let _ = writeln!(out, "{}_sum{} {}", f.name, labels, js_number(h.sum()));
                        let _ = writeln!(out, "{}_count{} {}", f.name, labels, h.count());
                    }
                }
            }
        }
        out
    }
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new()
    }
}

fn render_labels(names: &[String], values: &[String], le: Option<&str>) -> String {
    if names.is_empty() && le.is_none() {
        return String::new();
    }
    let mut s = String::from("{");
    let mut first = true;
    for (n, v) in names.iter().zip(values) {
        if !first {
            s.push(',');
        }
        first = false;
        let _ = write!(s, "{n}=\"{}\"", escape_label(v));
    }
    if let Some(le) = le {
        if !first {
            s.push(',');
        }
        let _ = write!(s, "le=\"{le}\"");
    }
    s.push('}');
    s
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n").replace('"', "\\\"")
}

/// Formats a number like JavaScript's `String(n)` for the values metrics hold.
pub fn js_number(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf".into() } else { "-Inf".into() }
    } else if v == v.trunc() && v.abs() < 1e21 {
        format!("{}", v as i128)
    } else {
        format!("{v}")
    }
}

/// Labelled counters.
#[derive(Clone)]
pub struct CounterVec(Arc<Family>);

impl CounterVec {
    pub fn with(&self, values: &[&str]) -> Counter {
        match self.0.child(values) {
            Child::Counter(c) => c,
            _ => unreachable!(),
        }
    }
}

/// Labelled gauges.
#[derive(Clone)]
pub struct GaugeVec(Arc<Family>);

impl GaugeVec {
    pub fn with(&self, values: &[&str]) -> Gauge {
        match self.0.child(values) {
            Child::Gauge(g) => g,
            _ => unreachable!(),
        }
    }
}

/// Labelled histograms.
#[derive(Clone)]
pub struct HistogramVec(Arc<Family>);

impl HistogramVec {
    pub fn with(&self, values: &[&str]) -> Histogram {
        match self.0.child(values) {
            Child::Histogram(h) => h,
            _ => unreachable!(),
        }
    }
}

pub fn counter(name: &str, help: &str) -> Counter {
    CounterVec(registry().define(name, help, Kind::Counter, &[], &[], None)).with(&[])
}

pub fn counter_vec(name: &str, help: &str, labels: &[&str]) -> CounterVec {
    CounterVec(registry().define(name, help, Kind::Counter, labels, &[], None))
}

pub fn gauge(name: &str, help: &str) -> Gauge {
    GaugeVec(registry().define(name, help, Kind::Gauge, &[], &[], None)).with(&[])
}

pub fn gauge_vec(name: &str, help: &str, labels: &[&str]) -> GaugeVec {
    GaugeVec(registry().define(name, help, Kind::Gauge, labels, &[], None))
}

/// A gauge read from `f` at every scrape (a panic keeps the last value).
pub fn gauge_fn(name: &str, help: &str, f: impl Fn() -> f64 + Send + Sync + 'static) {
    registry().define(name, help, Kind::GaugeFn, &[], &[], Some(Box::new(f)));
}

pub fn histogram(name: &str, help: &str, buckets: &[f64]) -> Histogram {
    HistogramVec(registry().define(name, help, Kind::Histogram, &[], buckets, None)).with(&[])
}

pub fn histogram_vec(name: &str, help: &str, labels: &[&str], buckets: &[f64]) -> HistogramVec {
    HistogramVec(registry().define(name, help, Kind::Histogram, labels, buckets, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let c = counter_vec("t_metrics_requests_total", "Requests.", &["route"]);
        c.with(&["a\"b"]).add(2);
        let h = histogram("t_metrics_latency_ms", "Latency.", &[1.0, 2.5]);
        h.observe(0.5);
        h.observe(3.0);
        gauge_fn("t_metrics_fn", "Fn.", || 4.0);
        let out = registry().render();
        assert!(out.contains("t_metrics_requests_total{route=\"a\\\"b\"} 2\n"));
        assert!(out.contains("t_metrics_latency_ms_bucket{le=\"1\"} 1\n"));
        assert!(out.contains("t_metrics_latency_ms_bucket{le=\"2.5\"} 1\n"));
        assert!(out.contains("t_metrics_latency_ms_bucket{le=\"+Inf\"} 2\n"));
        assert!(out.contains("t_metrics_latency_ms_sum 3.5\n"));
        assert!(out.contains("t_metrics_fn 4\n"));
    }

    #[test]
    fn js_number_formatting() {
        assert_eq!(js_number(1.0), "1");
        assert_eq!(js_number(0.25), "0.25");
        assert_eq!(js_number(f64::INFINITY), "+Inf");
        assert_eq!(js_number(-3.0), "-3");
    }
}
