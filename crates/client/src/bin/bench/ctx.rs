//! What every scenario shares: the server endpoint, the accounts, the process probe, the
//! measurement window and a pool of connection handshakes.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use scacelith_client::bot::Rng;
use scacelith_client::{ApiClient, Endpoint, TlsConfig};
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;

use crate::cli::{Options, TlsMode};
use crate::conn::{Conn, Target};
use crate::procfs::{Probe, Sample, window_json};
use crate::stats::{Stats, counter_delta, round3};

/// One account of the tokens file (its name comes with the `Welcome`).
#[derive(Clone, Debug)]
pub struct Account {
    /// A live session token.
    pub token: String,
}

/// Reads `username<TAB>token` or `token` lines (blank lines and `#` comments skipped).
pub fn read_accounts(path: &Path) -> Result<Vec<Account>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let accounts: Vec<Account> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| Account { token: l.rsplit('\t').next().unwrap_or(l).trim().to_string() })
        .collect();
    if accounts.is_empty() {
        return Err(format!("no account in {}", path.display()));
    }
    Ok(accounts)
}

/// The shared state of a run.
pub struct Ctx {
    /// The options.
    pub opts: Options,
    /// Where the server listens.
    pub endpoint: Endpoint,
    /// The REST client (a small keep-alive pool).
    pub api: ApiClient,
    /// The accounts of the tokens file (empty without one).
    pub accounts: Vec<Account>,
    /// The server's process tree.
    pub probe: Probe,
    /// Bounds the connection handshakes in flight.
    pub handshakes: Arc<Semaphore>,
}

impl Ctx {
    /// Builds the context of `opts`.
    pub fn new(opts: Options) -> Result<Arc<Ctx>, String> {
        let tls = match &opts.tls {
            TlsMode::Plain => None,
            TlsMode::Insecure => Some(TlsConfig::dangerous_accept_any_certificate()),
            TlsMode::Ca(path) => Some(TlsConfig::with_root_file(path).map_err(|e| e.to_string())?),
        };
        let tls = tls.map(|t| if opts.tls_resume { t } else { t.without_resumption() });
        let endpoint = match tls {
            Some(tls) => Endpoint::tls(opts.addr, opts.host.clone(), tls),
            None => Endpoint::plain(opts.addr),
        }
        .with_connect_timeout(opts.connect_timeout);
        let accounts = match &opts.tokens {
            Some(path) => read_accounts(path)?,
            None => Vec::new(),
        };
        Ok(Arc::new(Ctx {
            api: ApiClient::new(endpoint.clone()),
            endpoint,
            accounts,
            probe: Probe::new(opts.server_pid),
            handshakes: Arc::new(Semaphore::new(opts.inflight)),
            opts,
        }))
    }

    /// The protocol adapter.
    pub fn target(&self) -> Target {
        self.opts.target
    }

    /// A random generator: seeded when `--seed` was given (`salt` varies it per task).
    pub fn rng(&self, salt: u64) -> Rng {
        match self.opts.seed {
            Some(seed) => Rng::new(seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
            None => Rng::from_entropy(),
        }
    }

    /// The first `n` accounts, or an error naming what is missing.
    pub fn need_accounts(&self, n: usize) -> Result<&[Account], String> {
        if self.accounts.len() < n {
            return Err(format!(
                "{} accounts needed, the tokens file has {} (--tokens)",
                n,
                self.accounts.len()
            ));
        }
        Ok(&self.accounts[..n])
    }

    /// Opens an authenticated connection within the handshake pool, recording its timings
    /// (histograms `handshake` and `hello`) or its failure (counter `fail.<class>`).
    pub async fn connect(&self, token: &str, stats: &Stats) -> Option<Conn> {
        let _permit = self.handshakes.acquire().await.ok()?;
        match Conn::connect(self.target(), &self.endpoint, token, self.opts.connect_timeout).await {
            Ok(conn) => {
                let t = conn.timings();
                stats.latency("handshake", t.handshake);
                stats.latency("hello", t.hello);
                stats.add("connected", 1);
                Some(conn)
            }
            Err(failure) => {
                stats.add(&format!("fail.{}", failure.class), 1);
                stats.add("failed", 1);
                if stats.counter("failed").load(std::sync::atomic::Ordering::Relaxed) <= 3 {
                    eprintln!("connection failed: {} ({})", failure.class, failure.detail);
                }
                None
            }
        }
    }
}

/// One measurement window.
#[derive(Debug)]
pub struct Window {
    /// Its length.
    pub seconds: f64,
    /// Counter increments during the window.
    pub counters: std::collections::BTreeMap<String, u64>,
    /// Latency summaries of the window.
    pub latencies: Value,
    /// Server CPU and memory.
    pub server: Value,
    /// The server sample at the end.
    pub end_sample: Option<Sample>,
}

impl Window {
    /// The increment of counter `name` (0 when it never moved).
    pub fn count(&self, name: &str) -> u64 {
        self.counters.get(name).copied().unwrap_or(0)
    }

    /// The increment of counter `name` per second.
    pub fn rate(&self, name: &str) -> f64 {
        if self.seconds > 0.0 { round3(self.count(name) as f64 / self.seconds) } else { 0.0 }
    }

    /// A latency figure (`p50`, `p99`...) of histogram `name` in milliseconds, 0 when empty.
    pub fn latency(&self, name: &str, figure: &str) -> f64 {
        self.latencies.get(name).and_then(|h| h.get(figure)).and_then(Value::as_f64).unwrap_or(0.0)
    }

    /// The server CPU in percent of one core.
    pub fn cpu(&self) -> f64 {
        self.server.get("cpuPercent").and_then(Value::as_f64).unwrap_or(0.0)
    }

    /// The server RSS in MiB at the end.
    pub fn rss(&self) -> f64 {
        self.server.get("rssMiB").and_then(Value::as_f64).unwrap_or(0.0)
    }

    /// Counters whose name starts with `fail.`, `error.` or `drop.`, as JSON.
    pub fn errors_json(&self) -> Value {
        let map: Map<String, Value> = self
            .counters
            .iter()
            .filter(|(k, v)| {
                **v > 0 && (k.starts_with("fail.") || k.starts_with("error.") || k.starts_with("drop."))
            })
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        Value::Object(map)
    }

    /// Every detail of the window as JSON.
    pub fn json(&self) -> Value {
        json!({
            "seconds": round3(self.seconds),
            "counters": self.counters,
            "latencyMs": self.latencies,
            "server": self.server,
        })
    }
}

/// Waits `warmup`, then measures `stats` and the server for `duration`.
pub async fn measure(ctx: &Ctx, stats: &Stats, warmup: Duration, duration: Duration) -> Window {
    tokio::time::sleep(warmup).await;
    stats.set_measuring(true);
    let before = stats.counters();
    let s0 = ctx.probe.sample();
    let t0 = Instant::now();
    tokio::time::sleep(duration).await;
    let after = stats.counters();
    let s1 = ctx.probe.sample();
    let seconds = t0.elapsed().as_secs_f64();
    stats.set_measuring(false);
    Window {
        seconds,
        counters: counter_delta(&before, &after),
        latencies: stats.hists_json(),
        server: window_json(s0.as_ref(), s1.as_ref()),
        end_sample: s1,
    }
}

/// Milliseconds since the Unix epoch.
pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Prints a progress line on stderr.
pub fn progress(msg: impl AsRef<str>) {
    eprintln!("[bench] {}", msg.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_file() {
        let dir = std::env::temp_dir().join(format!("bench-accounts-{}", std::process::id()));
        std::fs::write(&dir, "# comment\nalice\tsct_a\n\nsct_b\n").unwrap();
        let accounts = read_accounts(&dir).unwrap();
        std::fs::remove_file(&dir).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!((accounts[0].token.as_str(), accounts[1].token.as_str()), ("sct_a", "sct_b"));
    }
}
