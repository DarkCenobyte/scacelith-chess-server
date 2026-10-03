//! Prometheus metrics (DESIGN 4; their endpoint: DESIGN 5.7).
//!
//! Metrics are registered once (usually in a `static` built with [`std::sync::LazyLock`]) and
//! updated with atomics, so the hot paths never lock. Registering the same name and kind twice
//! returns the existing metric; another kind panics (a programming error). The registry renders
//! the Prometheus text format 0.0.4 in registration order; label children render in creation
//! order. Numbers print like JavaScript's `String(n)` (`1`, not `1.0`), infinities as `+Inf` and
//! `-Inf`.
//!
//! ```ignore
//! static MOVES: LazyLock<CounterVec> =
//!     LazyLock::new(|| metrics::counter_vec("scacelith_moves_total", "Moves played.", &["result"]));
//! MOVES.with(&["ok"]).inc();
//! ```
//!
//! [`start_process_metrics`] adds the health of the process itself. The former server's V8 and
//! event-loop gauges have no meaning here and are replaced:
//!
//! | Former metric | Now |
//! |---|---|
//! | `scacelith_process_event_loop_delay_{p50,p99,max}_ms` | `scacelith_runtime_lateness_{p50,p99,max}_ms` and the `scacelith_runtime_lateness_ms` histogram |
//! | `scacelith_process_heap_used_bytes`, `scacelith_process_external_bytes` | removed (`scacelith_process_rss_bytes` covers the memory) |
//! | `scacelith_process_cpu_ratio`, `scacelith_process_rss_bytes`, `scacelith_process_uptime_seconds` | kept |
//! | (new) | `scacelith_process_cpu_seconds_total`, `scacelith_process_open_fds`, `scacelith_process_max_fds`, `scacelith_process_threads`, `scacelith_process_start_time_seconds`, `scacelith_log_dropped_total` |
//!
//! The runtime lateness is how late a task sleeping 10 ms on the async runtime wakes up: the time
//! a ready task waits for a runtime thread. Unlike the former event-loop delay it does not include
//! the 10 ms period, so an idle server reads about 0-1 ms (timer resolution) instead of 10 ms.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Counter,
    CounterFn,
    Gauge,
    GaugeFn,
    Histogram,
}

impl Kind {
    fn type_name(self) -> &'static str {
        match self {
            Kind::Counter | Kind::CounterFn => "counter",
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
    /// Adds one.
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// Adds `n`.
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// The current count.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A value that goes up and down.
#[derive(Clone, Debug)]
pub struct Gauge(Arc<F64Cell>);

impl Gauge {
    /// Sets the value.
    pub fn set(&self, v: f64) {
        self.0.set(v);
    }

    /// Adds `d` (negative to subtract).
    pub fn add(&self, d: f64) {
        self.0.add(d);
    }

    /// Adds one.
    pub fn inc(&self) {
        self.0.add(1.0);
    }

    /// Subtracts one.
    pub fn dec(&self) {
        self.0.add(-1.0);
    }

    /// The current value.
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
    /// Records one value (a `NaN` lands in the first bucket, as it did before).
    pub fn observe(&self, v: f64) {
        let i = if v.is_nan() {
            0
        } else {
            self.0.bounds.iter().position(|b| v <= *b).unwrap_or(self.0.bounds.len())
        };
        self.0.counts[i].fetch_add(1, Ordering::Relaxed);
        self.0.sum.add(v);
        self.0.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Number of values recorded.
    pub fn count(&self) -> u64 {
        self.0.count.load(Ordering::Relaxed)
    }

    /// Sum of the values recorded.
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

type ReadFn = Box<dyn Fn() -> f64 + Send + Sync>;

struct Family {
    name: String,
    help: String,
    kind: Kind,
    labels: Vec<String>,
    bounds: Arc<[f64]>,
    children: RwLock<IndexMap<Vec<String>, Child>>,
    func: Option<ReadFn>,
    last_fn_value: F64Cell,
}

impl Family {
    fn child(&self, values: &[&str]) -> Child {
        assert_eq!(values.len(), self.labels.len(), "metric {}: wrong number of label values", self.name);
        let key: Vec<String> = values.iter().map(|s| s.to_string()).collect();
        if let Some(c) = self.children.read().get(&key) {
            return c.clone();
        }
        let mut w = self.children.write();
        w.entry(key)
            .or_insert_with(|| match self.kind {
                Kind::Counter | Kind::CounterFn => Child::Counter(Counter(Arc::new(AtomicU64::new(0)))),
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

    /// Reads a function metric: `NaN` reads as 0 and a panic keeps the last value (the former
    /// `+fn() || 0` in a `try`).
    fn read_fn(&self, func: &ReadFn) -> f64 {
        if let Ok(v) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(func)) {
            self.last_fn_value.set(if v.is_nan() { 0.0 } else { v });
        }
        self.last_fn_value.get()
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

/// The process registry, served on `METRICS_PORT`.
pub fn registry() -> &'static Registry {
    &REGISTRY
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Registry {
        Registry { families: Mutex::new(Vec::new()), by_name: Mutex::new(HashMap::new()) }
    }

    fn define(
        &self,
        name: &str,
        help: &str,
        kind: Kind,
        labels: &[&str],
        bounds: &[f64],
        func: Option<ReadFn>,
    ) -> Arc<Family> {
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
        if labels.is_empty() && fam.func.is_none() {
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
                let _ = writeln!(out, "{} {}", f.name, js_number(f.read_fn(func)));
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

/// Formats a sample value: JavaScript's `String(n)` (`1`, `0.25`, `1e-7`), with the Prometheus
/// spellings `+Inf` and `-Inf` for the infinities.
pub fn js_number(v: f64) -> String {
    if v.is_infinite() {
        return if v > 0.0 { "+Inf" } else { "-Inf" }.to_string();
    }
    crate::util::js::number_to_string(v)
}

/// Labelled counters.
#[derive(Clone)]
pub struct CounterVec(Arc<Family>);

impl CounterVec {
    /// The counter of these label values (in the declared order). Cache it on hot paths.
    pub fn with(&self, values: &[&str]) -> Counter {
        match self.0.child(values) {
            Child::Counter(c) => c,
            _ => unreachable!("a counter family only has counters"),
        }
    }
}

/// Labelled gauges.
#[derive(Clone)]
pub struct GaugeVec(Arc<Family>);

impl GaugeVec {
    /// The gauge of these label values (in the declared order).
    pub fn with(&self, values: &[&str]) -> Gauge {
        match self.0.child(values) {
            Child::Gauge(g) => g,
            _ => unreachable!("a gauge family only has gauges"),
        }
    }
}

/// Labelled histograms.
#[derive(Clone)]
pub struct HistogramVec(Arc<Family>);

impl HistogramVec {
    /// The histogram of these label values (in the declared order).
    pub fn with(&self, values: &[&str]) -> Histogram {
        match self.0.child(values) {
            Child::Histogram(h) => h,
            _ => unreachable!("a histogram family only has histograms"),
        }
    }
}

/// Registers (or returns) a counter without labels.
pub fn counter(name: &str, help: &str) -> Counter {
    CounterVec(registry().define(name, help, Kind::Counter, &[], &[], None)).with(&[])
}

/// Registers (or returns) a labelled counter family.
pub fn counter_vec(name: &str, help: &str, labels: &[&str]) -> CounterVec {
    CounterVec(registry().define(name, help, Kind::Counter, labels, &[], None))
}

/// A counter read from `f` at every scrape (a total kept elsewhere, such as the CPU time).
/// `NaN` reads as 0; a panic keeps the last value.
pub fn counter_fn(name: &str, help: &str, f: impl Fn() -> f64 + Send + Sync + 'static) {
    registry().define(name, help, Kind::CounterFn, &[], &[], Some(Box::new(f)));
}

/// Registers (or returns) a gauge without labels.
pub fn gauge(name: &str, help: &str) -> Gauge {
    GaugeVec(registry().define(name, help, Kind::Gauge, &[], &[], None)).with(&[])
}

/// Registers (or returns) a labelled gauge family.
pub fn gauge_vec(name: &str, help: &str, labels: &[&str]) -> GaugeVec {
    GaugeVec(registry().define(name, help, Kind::Gauge, labels, &[], None))
}

/// A gauge read from `f` at every scrape. `NaN` reads as 0; a panic keeps the last value.
pub fn gauge_fn(name: &str, help: &str, f: impl Fn() -> f64 + Send + Sync + 'static) {
    registry().define(name, help, Kind::GaugeFn, &[], &[], Some(Box::new(f)));
}

/// Registers (or returns) a histogram without labels; `buckets` are the upper bounds, ascending.
pub fn histogram(name: &str, help: &str, buckets: &[f64]) -> Histogram {
    HistogramVec(registry().define(name, help, Kind::Histogram, &[], buckets, None)).with(&[])
}

/// Registers (or returns) a labelled histogram family.
pub fn histogram_vec(name: &str, help: &str, labels: &[&str], buckets: &[f64]) -> HistogramVec {
    HistogramVec(registry().define(name, help, Kind::Histogram, labels, buckets, None))
}

// ---- Process and runtime health ----------------------------------------------------------------

/// Period of the runtime lateness probe.
const PROBE_PERIOD: Duration = Duration::from_millis(10);
/// The CPU ratio and the lateness gauges are refreshed this often.
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);
/// The lateness percentiles cover the samples of this window (then start again).
const LATENESS_WINDOW: Duration = Duration::from_secs(10);
const LATENESS_BUCKETS: [f64; 11] = [0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];

#[derive(Default)]
struct ProcessGauges {
    cpu_ratio: F64Cell,
    lateness_p50: F64Cell,
    lateness_p99: F64Cell,
    lateness_max: F64Cell,
}

static PROCESS: LazyLock<ProcessGauges> = LazyLock::new(ProcessGauges::default);

/// The running process metrics sampler, from [`start_process_metrics`].
#[derive(Debug)]
pub struct ProcessMetrics {
    task: tokio::task::JoinHandle<()>,
}

impl ProcessMetrics {
    /// The 99th percentile of the runtime lateness over the current window, in milliseconds
    /// (what `/healthz` or an overload check may look at).
    pub fn lateness_p99_ms(&self) -> f64 {
        PROCESS.lateness_p99.get()
    }

    /// Stops the sampler (the gauges keep their last values).
    pub fn stop(self) {
        self.task.abort();
    }
}

/// Registers the process metrics and starts their sampler (the runtime lateness probe and the
/// 1 s CPU sample) on the current tokio runtime. Call once, from `start`.
pub fn start_process_metrics() -> ProcessMetrics {
    register_process_metrics();
    let lateness = histogram(
        "scacelith_runtime_lateness_ms",
        "How late a 10 ms timer of the async runtime fired (time a ready task waits for a runtime thread)",
        &LATENESS_BUCKETS,
    );
    ProcessMetrics { task: tokio::spawn(run_sampler(lateness)) }
}

/// Registers the process gauges that need no sampler (memory, files, threads, CPU time, uptime).
pub fn register_process_metrics() {
    let started = process_start_time_seconds();
    gauge_fn(
        "scacelith_process_cpu_ratio",
        "CPU time / wall time over the last second (1 = one core)",
        || PROCESS.cpu_ratio.get(),
    );
    counter_fn(
        "scacelith_process_cpu_seconds_total",
        "CPU time (user and system) used by the process",
        crate::sys::process_cpu_seconds,
    );
    gauge_fn("scacelith_process_rss_bytes", "Resident set size", || {
        read_proc("/proc/self/statm")
            .and_then(|s| parse_statm_rss_pages(&s))
            .map_or(f64::NAN, |pages| (pages * crate::sys::page_size()) as f64)
    });
    gauge_fn("scacelith_process_open_fds", "Open file descriptors", || {
        match std::fs::read_dir("/proc/self/fd") {
            // The directory being read is itself one of the listed descriptors.
            Ok(dir) => dir.count().saturating_sub(1) as f64,
            Err(_) => f64::NAN,
        }
    });
    gauge_fn(
        "scacelith_process_max_fds",
        "Limit of open file descriptors (RLIMIT_NOFILE soft limit)",
        || crate::sys::nofile_limit().map_or(f64::NAN, |(soft, _)| soft as f64),
    );
    gauge_fn("scacelith_process_threads", "Threads of the process", || {
        read_proc("/proc/self/stat").and_then(|s| parse_stat(&s)).map_or(f64::NAN, |st| st.threads as f64)
    });
    gauge_fn("scacelith_process_start_time_seconds", "Start time of the process, Unix seconds", move || {
        started
    });
    gauge_fn("scacelith_process_uptime_seconds", "Process uptime", move || {
        (crate::clock::wall_ms() as f64 / 1000.0 - started).max(0.0)
    });
    counter_fn(
        "scacelith_log_dropped_total",
        "Log records dropped because the log writer was behind",
        || crate::log::dropped() as f64,
    );
    gauge_fn(
        "scacelith_runtime_lateness_p50_ms",
        "Lateness of a 10 ms runtime timer, median over the last window",
        || PROCESS.lateness_p50.get(),
    );
    gauge_fn(
        "scacelith_runtime_lateness_p99_ms",
        "Lateness of a 10 ms runtime timer, 99th percentile over the last window",
        || PROCESS.lateness_p99.get(),
    );
    gauge_fn(
        "scacelith_runtime_lateness_max_ms",
        "Lateness of a 10 ms runtime timer, maximum over the last window",
        || PROCESS.lateness_max.get(),
    );
}

async fn run_sampler(lateness: Histogram) {
    let mut window = LatenessWindow::default();
    let mut window_start = Instant::now();
    let mut last_sample = window_start;
    let mut last_cpu = crate::sys::process_cpu_seconds();
    loop {
        let before = Instant::now();
        tokio::time::sleep(PROBE_PERIOD).await;
        let now = Instant::now();
        let late_ms = now.duration_since(before).saturating_sub(PROBE_PERIOD).as_secs_f64() * 1000.0;
        lateness.observe(late_ms);
        window.push(late_ms);
        let dt = now.duration_since(last_sample);
        if dt < SAMPLE_PERIOD {
            continue;
        }
        let cpu = crate::sys::process_cpu_seconds();
        PROCESS.cpu_ratio.set((cpu - last_cpu).max(0.0) / dt.as_secs_f64());
        last_cpu = cpu;
        last_sample = now;
        if let Some(s) = window.summary() {
            PROCESS.lateness_p50.set(s.p50);
            PROCESS.lateness_p99.set(s.p99);
            PROCESS.lateness_max.set(s.max);
        }
        if now.duration_since(window_start) >= LATENESS_WINDOW {
            window.clear();
            window_start = now;
        }
    }
}

/// Lateness samples of the current window.
#[derive(Debug, Default)]
struct LatenessWindow {
    samples: Vec<f64>,
}

#[derive(Debug, PartialEq)]
struct LatenessSummary {
    p50: f64,
    p99: f64,
    max: f64,
}

impl LatenessWindow {
    fn push(&mut self, ms: f64) {
        self.samples.push(ms);
    }

    fn clear(&mut self) {
        self.samples.clear();
    }

    /// Nearest-rank percentiles; `None` without samples.
    fn summary(&self) -> Option<LatenessSummary> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(f64::total_cmp);
        let rank = |p: f64| {
            let i = (p / 100.0 * sorted.len() as f64).ceil() as usize;
            sorted[i.clamp(1, sorted.len()) - 1]
        };
        Some(LatenessSummary { p50: rank(50.0), p99: rank(99.0), max: sorted[sorted.len() - 1] })
    }
}

fn read_proc(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Resident pages, the second field of `/proc/<pid>/statm`.
fn parse_statm_rss_pages(text: &str) -> Option<u64> {
    text.split_ascii_whitespace().nth(1)?.parse().ok()
}

struct ProcStat {
    threads: u64,
    start_ticks: u64,
}

/// `num_threads` (field 20) and `starttime` (field 22) of `/proc/<pid>/stat`. The command name
/// (field 2) is in parentheses and may hold spaces, so fields are counted after the last `)`.
fn parse_stat(text: &str) -> Option<ProcStat> {
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    Some(ProcStat { threads: fields.get(17)?.parse().ok()?, start_ticks: fields.get(19)?.parse().ok()? })
}

/// `btime` of `/proc/stat`: boot time, Unix seconds.
fn parse_boot_time(text: &str) -> Option<u64> {
    text.lines().find_map(|l| l.strip_prefix("btime ")).and_then(|v| v.trim().parse().ok())
}

/// Start time of the process, Unix seconds (boot time plus the start offset of `/proc/self/stat`;
/// the current time when `/proc` cannot be read).
pub fn process_start_time_seconds() -> f64 {
    let from_proc = || -> Option<f64> {
        let st = parse_stat(&read_proc("/proc/self/stat")?)?;
        let boot = parse_boot_time(&read_proc("/proc/stat")?)?;
        Some(boot as f64 + st.start_ticks as f64 / crate::sys::clock_ticks_per_second() as f64)
    };
    from_proc().unwrap_or_else(|| crate::clock::wall_ms() as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let c = counter_vec("t_metrics_requests_total", "Requests.", &["route"]);
        c.with(&["a\"b"]).add(2);
        c.with(&["x\\y\nz"]).inc();
        let h = histogram("t_metrics_latency_ms", "Latency.", &[1.0, 2.5]);
        h.observe(0.5);
        h.observe(3.0);
        gauge_fn("t_metrics_fn", "Fn.", || 4.0);
        counter_fn("t_metrics_fn_total", "Fn total.", || 7.5);
        gauge("t_metrics_gauge", "Gauge.").set(0.1 + 0.2);
        let out = registry().render();
        assert!(out.contains(
            "# HELP t_metrics_requests_total Requests.\n# TYPE t_metrics_requests_total counter\n"
        ));
        assert!(out.contains("t_metrics_requests_total{route=\"a\\\"b\"} 2\n"));
        assert!(out.contains("t_metrics_requests_total{route=\"x\\\\y\\nz\"} 1\n"));
        assert!(out.contains("t_metrics_latency_ms_bucket{le=\"1\"} 1\n"));
        assert!(out.contains("t_metrics_latency_ms_bucket{le=\"2.5\"} 1\n"));
        assert!(out.contains("t_metrics_latency_ms_bucket{le=\"+Inf\"} 2\n"));
        assert!(out.contains("t_metrics_latency_ms_sum 3.5\n"));
        assert!(out.contains("t_metrics_latency_ms_count 2\n"));
        assert!(out.contains("t_metrics_fn 4\n"));
        assert!(out.contains("# TYPE t_metrics_fn_total counter\nt_metrics_fn_total 7.5\n"));
        assert!(out.contains("t_metrics_gauge 0.30000000000000004\n"));
    }

    #[test]
    fn registering_twice_returns_the_same_metric() {
        counter("t_metrics_twice_total", "Twice.").add(3);
        assert_eq!(counter("t_metrics_twice_total", "Twice.").get(), 3);
        let v = gauge_vec("t_metrics_twice_g", "Twice.", &["k"]);
        v.with(&["a"]).set(2.0);
        assert_eq!(gauge_vec("t_metrics_twice_g", "Twice.", &["k"]).with(&["a"]).get(), 2.0);
    }

    #[test]
    #[should_panic(expected = "redefined with another kind")]
    fn another_kind_panics() {
        counter("t_metrics_kind", "Kind.");
        gauge("t_metrics_kind", "Kind.");
    }

    #[test]
    fn function_metrics_read_nan_as_zero_and_keep_the_last_value_on_panic() {
        // A registry of its own: the other tests render the process registry concurrently.
        let reg = Registry::new();
        let calls = AtomicU64::new(0);
        let probe = move || match calls.fetch_add(1, Ordering::Relaxed) {
            0 => 5.0,
            1 => panic!("probe failed"),
            _ => f64::NAN,
        };
        reg.define("t_metrics_flaky", "Flaky.", Kind::GaugeFn, &[], &[], Some(Box::new(probe)));
        let line =
            || reg.render().lines().find(|l| l.starts_with("t_metrics_flaky ")).map(str::to_string).unwrap();
        assert_eq!(line(), "t_metrics_flaky 5");
        assert_eq!(line(), "t_metrics_flaky 5", "a panic keeps the last value");
        assert_eq!(line(), "t_metrics_flaky 0");
    }

    #[test]
    fn histogram_buckets_follow_the_former_rule() {
        let h = histogram("t_metrics_nan", "NaN.", &[1.0, 2.0]);
        h.observe(f64::NAN);
        h.observe(1.0);
        h.observe(2.000_1);
        let out = registry().render();
        assert!(out.contains("t_metrics_nan_bucket{le=\"1\"} 2\n"), "NaN and 1 count in le=1");
        assert!(out.contains("t_metrics_nan_bucket{le=\"2\"} 2\n"));
        assert!(out.contains("t_metrics_nan_bucket{le=\"+Inf\"} 3\n"));
        assert!(out.contains("t_metrics_nan_sum NaN\n"));
    }

    #[test]
    fn js_number_formatting() {
        assert_eq!(js_number(1.0), "1");
        assert_eq!(js_number(0.25), "0.25");
        assert_eq!(js_number(f64::INFINITY), "+Inf");
        assert_eq!(js_number(f64::NEG_INFINITY), "-Inf");
        assert_eq!(js_number(-3.0), "-3");
        assert_eq!(js_number(-0.0), "0");
        assert_eq!(js_number(1e-7), "1e-7");
        assert_eq!(js_number(1e21), "1e+21");
        assert_eq!(js_number(f64::NAN), "NaN");
    }

    #[test]
    fn lateness_percentiles_use_the_nearest_rank() {
        let mut w = LatenessWindow::default();
        assert_eq!(w.summary(), None);
        for i in (1..=200).rev() {
            w.push(f64::from(i));
        }
        assert_eq!(w.summary(), Some(LatenessSummary { p50: 100.0, p99: 198.0, max: 200.0 }));
        w.clear();
        w.push(3.0);
        assert_eq!(w.summary(), Some(LatenessSummary { p50: 3.0, p99: 3.0, max: 3.0 }));
    }

    #[test]
    fn proc_files_parse() {
        assert_eq!(parse_statm_rss_pages("12345 678 90 1 0 2 0\n"), Some(678));
        let stat = "4242 (scacelith (x) y) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 6 0 0 20 0 9 0 777 1000 \
                    200 18446744073709551615";
        let st = parse_stat(stat).unwrap();
        assert_eq!((st.threads, st.start_ticks), (9, 777));
        assert_eq!(parse_boot_time("cpu 1 2 3\nbtime 1790000000\nprocesses 5\n"), Some(1_790_000_000));
        let own = parse_stat(&read_proc("/proc/self/stat").unwrap()).unwrap();
        assert!(own.threads >= 1);
        let started = process_start_time_seconds();
        let now = crate::clock::wall_ms() as f64 / 1000.0;
        assert!(started <= now + 1.0 && started > now - 86_400.0 * 365.0);
    }

    #[test]
    fn process_metrics_render() {
        register_process_metrics();
        let out = registry().render();
        let value = |name: &str| -> f64 {
            let line = out.lines().find(|l| l.starts_with(&format!("{name} "))).unwrap();
            line[name.len() + 1..].parse().unwrap()
        };
        assert!(value("scacelith_process_rss_bytes") > 0.0);
        assert!(value("scacelith_process_open_fds") >= 3.0);
        assert!(value("scacelith_process_max_fds") >= value("scacelith_process_open_fds"));
        assert!(value("scacelith_process_threads") >= 1.0);
        assert!(value("scacelith_process_cpu_seconds_total") >= 0.0);
        assert!(value("scacelith_process_uptime_seconds") >= 0.0);
        assert!(out.contains("# TYPE scacelith_process_cpu_seconds_total counter\n"));
    }

    #[test]
    fn the_sampler_measures_runtime_lateness() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        rt.block_on(async {
            let m = start_process_metrics();
            tokio::time::sleep(Duration::from_millis(60)).await;
            assert!(m.lateness_p99_ms() >= 0.0);
            m.stop();
        });
        let out = registry().render();
        let count = out.lines().find(|l| l.starts_with("scacelith_runtime_lateness_ms_count ")).unwrap();
        assert!(count["scacelith_runtime_lateness_ms_count ".len()..].parse::<u64>().unwrap() >= 1);
    }
}
