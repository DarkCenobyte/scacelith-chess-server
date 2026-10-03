//! Linux `/proc` readers: CPU time and memory of the server's whole process tree (the Node
//! primary and its shard workers, or the one Rust process), summed.
//!
//! Every reader returns `None` (or zero) when `/proc` is unavailable or the process is gone, so
//! a run on another system still reports its latencies.

use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::{Value, json};

use crate::stats::round3;

fn getconf(name: &str, fallback: u64) -> u64 {
    Command::new("getconf")
        .arg(name)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(fallback)
}

/// Clock ticks per second (unit of `utime` and `stime`).
fn clk_tck() -> u64 {
    static TCK: OnceLock<u64> = OnceLock::new();
    *TCK.get_or_init(|| getconf("CLK_TCK", 100))
}

/// Bytes per memory page.
fn page_size() -> u64 {
    static PAGE: OnceLock<u64> = OnceLock::new();
    *PAGE.get_or_init(|| getconf("PAGESIZE", 4096))
}

/// What `/proc/<pid>/stat` says about one process.
#[derive(Clone, Copy, Debug)]
struct ProcStat {
    ppid: u32,
    ticks: u64,
    rss: u64,
}

fn proc_stat(pid: u32) -> Option<ProcStat> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may hold spaces and parentheses: fields restart after the last ')'.
    let rest = &text[text.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split(' ').collect();
    // f[0] is field 3 (state): ppid is field 4, utime 14, stime 15, rss 24 (pages).
    let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
    Some(ProcStat {
        ppid: u32::try_from(num(1)?).ok()?,
        ticks: num(11)? + num(12)?,
        rss: num(21)? * page_size(),
    })
}

/// `root` and every descendant of it.
pub fn process_tree(root: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else { return vec![root] };
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        if let Some(st) = proc_stat(pid) {
            children.entry(st.ppid).or_default().push(pid);
        }
    }
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        if let Some(kids) = children.get(&out[i]) {
            out.extend_from_slice(kids);
        }
        i += 1;
    }
    out
}

/// Proportional set size of a process (shared pages split between their users), in bytes.
fn pss(pid: u32) -> Option<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let line = text.lines().find(|l| l.starts_with("Pss:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Totals over a process tree at one instant.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// When it was taken.
    pub at: Instant,
    /// CPU time, user plus system, of every process alive now (seconds).
    pub cpu_s: f64,
    /// Sum of the resident set sizes (bytes).
    pub rss: u64,
    /// Sum of the proportional set sizes (bytes; 0 when unreadable).
    pub pss: u64,
    /// Processes counted.
    pub processes: usize,
}

/// Samples the server's process tree.
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    root: Option<u32>,
}

impl Probe {
    /// A probe of the tree rooted at `pid` (`None`: no server process to watch).
    pub fn new(pid: Option<u32>) -> Probe {
        Probe { root: pid }
    }

    /// Whether the root process exists.
    pub fn alive(&self) -> bool {
        self.root.is_some_and(|pid| proc_stat(pid).is_some())
    }

    /// One sample (`None` without a root or once it is gone).
    pub fn sample(&self) -> Option<Sample> {
        let root = self.root?;
        proc_stat(root)?;
        let tree = process_tree(root);
        let (mut ticks, mut rss, mut pss_sum, mut n) = (0u64, 0u64, 0u64, 0usize);
        for pid in tree {
            if let Some(st) = proc_stat(pid) {
                ticks += st.ticks;
                rss += st.rss;
                pss_sum += pss(pid).unwrap_or(0);
                n += 1;
            }
        }
        Some(Sample {
            at: Instant::now(),
            cpu_s: ticks as f64 / clk_tck() as f64,
            rss,
            pss: pss_sum,
            processes: n,
        })
    }
}

/// Server CPU use between two samples, in percent of one core (200 = two cores busy).
pub fn cpu_percent(a: &Sample, b: &Sample) -> f64 {
    let wall = b.at.duration_since(a.at).as_secs_f64();
    if wall <= 0.0 {
        return 0.0;
    }
    // A worker that exited in between takes its ticks away: never report a negative use.
    ((b.cpu_s - a.cpu_s).max(0.0) / wall) * 100.0
}

/// Mebibytes, rounded for the report.
pub fn mib(bytes: u64) -> f64 {
    round3(bytes as f64 / (1024.0 * 1024.0))
}

/// The resources of a measurement window: CPU between `a` and `b`, memory at `b`.
pub fn window_json(a: Option<&Sample>, b: Option<&Sample>) -> Value {
    match (a, b) {
        (Some(a), Some(b)) => json!({
            "cpuPercent": round3(cpu_percent(a, b)),
            "cpuSeconds": round3((b.cpu_s - a.cpu_s).max(0.0)),
            "rssMiB": mib(b.rss),
            "pssMiB": mib(b.pss),
            "processes": b.processes,
        }),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_its_own_process() {
        let probe = Probe::new(Some(std::process::id()));
        if !probe.alive() {
            return; // no /proc here
        }
        let a = probe.sample().expect("own process");
        assert!(a.rss > 0 && a.processes >= 1);
        let mut x = 0u64;
        for i in 0..20_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        let b = probe.sample().expect("own process");
        assert!(cpu_percent(&a, &b) >= 0.0);
        assert!(Probe::new(None).sample().is_none());
    }
}
