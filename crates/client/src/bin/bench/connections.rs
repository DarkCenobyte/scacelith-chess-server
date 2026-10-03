//! Scenario (b): connection capacity.
//!
//! Authenticated WebSocket connections (TCP, TLS, upgrade, Hello/Welcome) are opened up to each
//! step's count, `--inflight` handshakes at a time, and held: each one answers the server's
//! heartbeat and reads what comes. For each step the report gives the ramp (handshakes per
//! second, handshake and Hello latency, failures by class), then, after the warm-up, the window
//! with every connection idle: server CPU, memory and memory per connection (RSS and PSS above
//! the footprint before the first connection), and connections dropped by the server.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::conn::Conn;
use crate::ctx::{Ctx, measure, progress};
use crate::procfs::{Sample, mib};
use crate::report::Step;
use crate::stats::{Stats, counter_delta, round3};

/// Holds a connection until `stop`, counting a close by the server as a drop.
async fn hold(mut conn: Conn, mut stop: watch::Receiver<bool>, stats: Arc<Stats>, alive: Arc<AtomicUsize>) {
    alive.fetch_add(1, Ordering::Relaxed);
    loop {
        tokio::select! {
            received = conn.recv() => {
                if received.is_err() {
                    stats.add(&format!("drop.{}", conn.close_code().unwrap_or(1006)), 1);
                    alive.fetch_sub(1, Ordering::Relaxed);
                    return;
                }
            }
            _ = stop.changed() => break,
        }
    }
    conn.close();
    conn.wait_closed(Duration::from_secs(5)).await;
    alive.fetch_sub(1, Ordering::Relaxed);
}

/// Memory above the baseline per connection, in KiB.
fn per_conn_kib(base: Option<&Sample>, now: Option<&Sample>, conns: usize, pick: fn(&Sample) -> u64) -> f64 {
    match (base, now) {
        (Some(b), Some(n)) if conns > 0 => round3((pick(n) as f64 - pick(b) as f64) / 1024.0 / conns as f64),
        _ => 0.0,
    }
}

/// Runs the scenario.
pub async fn run(ctx: &Arc<Ctx>) -> Result<(Value, Vec<Step>), String> {
    let max = ctx.opts.steps.iter().copied().max().unwrap_or(0);
    let accounts = ctx.need_accounts(max)?.to_vec();
    let ramp_stats = Stats::new();
    let hold_stats = Stats::new();
    let alive = Arc::new(AtomicUsize::new(0));
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut holders = JoinSet::new();
    let base = ctx.probe.sample();
    let mut opened = 0;
    let mut steps = Vec::new();

    for &target in &ctx.opts.steps {
        if target <= opened {
            continue;
        }
        progress(format!("connections: ramping to {target}"));
        ramp_stats.set_measuring(true);
        let before = ramp_stats.counters();
        let s0 = ctx.probe.sample();
        let started = Instant::now();
        let mut ramp = JoinSet::new();
        for account in &accounts[opened..target] {
            let (ctx, token, stats) = (ctx.clone(), account.token.clone(), ramp_stats.clone());
            ramp.spawn(async move { ctx.connect(&token, &stats).await });
        }
        while let Some(joined) = ramp.join_next().await {
            if let Ok(Some(conn)) = joined {
                holders.spawn(hold(conn, stop_rx.clone(), hold_stats.clone(), alive.clone()));
            }
        }
        let ramp_s = started.elapsed().as_secs_f64();
        let s1 = ctx.probe.sample();
        ramp_stats.set_measuring(false);
        let ramp_counts = counter_delta(&before, &ramp_stats.counters());
        let ramp_lat = ramp_stats.hists_json();
        opened = target;
        let connected = ramp_counts.get("connected").copied().unwrap_or(0);
        let failed = ramp_counts.get("failed").copied().unwrap_or(0);
        progress(format!("connections: {connected} opened, {failed} failed in {ramp_s:.1} s"));

        let window = measure(ctx, &hold_stats, ctx.opts.warmup, ctx.opts.duration).await;
        let now = window.end_sample;
        let conns = alive.load(Ordering::Relaxed);
        let lat = |name: &str, figure: &str| ramp_lat[name][figure].as_f64().unwrap_or(0.0);
        let failures: serde_json::Map<String, Value> = ramp_counts
            .iter()
            .filter(|(k, v)| k.starts_with("fail.") && **v > 0)
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        let drops: u64 = window.counters.iter().filter(|(k, _)| k.starts_with("drop.")).map(|(_, v)| v).sum();
        let step = Step::new(format!("{target} conns"))
            .figure("open", conns)
            .figure("handshakes/s", round3(connected as f64 / ramp_s.max(1e-9)))
            .figure("handshake p50 ms", lat("handshake", "p50"))
            .figure("handshake p99 ms", lat("handshake", "p99"))
            .figure("hello p99 ms", lat("hello", "p99"))
            .figure("failed", failed)
            .figure("dropped", drops)
            .figure("idle CPU %", window.cpu())
            .figure("RSS MiB", window.rss())
            .figure("KiB/conn RSS", per_conn_kib(base.as_ref(), now.as_ref(), conns, |s| s.rss))
            .figure("KiB/conn PSS", per_conn_kib(base.as_ref(), now.as_ref(), conns, |s| s.pss))
            .detail(json!({
                "ramp": {
                    "seconds": round3(ramp_s),
                    "connected": connected,
                    "failed": failed,
                    "failures": failures,
                    "latencyMs": ramp_lat,
                    "server": crate::procfs::window_json(s0.as_ref(), s1.as_ref()),
                },
                "baselineRssMiB": base.map(|s| mib(s.rss)),
                "baselinePssMiB": base.map(|s| mib(s.pss)),
                "hold": window.json(),
            }));
        steps.push(step);
    }

    progress("connections: closing");
    let _ = stop_tx.send(true);
    let _ =
        tokio::time::timeout(Duration::from_secs(60), async { while holders.join_next().await.is_some() {} })
            .await;
    let params = json!({ "steps": ctx.opts.steps, "inflight": ctx.opts.inflight });
    Ok((params, steps))
}
