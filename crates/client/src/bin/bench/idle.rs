//! Scenario (a): time to ready and idle footprint.
//!
//! With `--spawned-at-ms` (the epoch milliseconds at which the harness started the server), the
//! time to ready is measured up to the first `200` of `/api/v1/readyz` on the public port and,
//! with `--metrics-addr`, of `/readyz` on the metrics listener (every shard ready). Then, after
//! the warm-up, the server's CPU and memory are sampled over the window with no client at all.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use scacelith_client::Endpoint;
use scacelith_client::http::{HttpConnection, Request};
use serde_json::json;

use crate::ctx::{Ctx, epoch_ms, measure, progress};
use crate::procfs::mib;
use crate::report::Step;
use crate::stats::Stats;

/// How often readiness is polled.
const POLL: Duration = Duration::from_millis(10);

async fn ready_once(endpoint: &Endpoint, path: &str) -> bool {
    let attempt = async {
        let mut conn = HttpConnection::open(endpoint).await.ok()?;
        conn.send(&Request::get(path)).await.ok()
    };
    matches!(tokio::time::timeout(Duration::from_secs(2), attempt).await, Ok(Some(r)) if r.status == 200)
}

async fn wait_ready(endpoint: &Endpoint, path: &str, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if ready_once(endpoint, path).await {
            return true;
        }
        tokio::time::sleep(POLL).await;
    }
    false
}

/// Runs the scenario.
pub async fn run(ctx: &Ctx) -> Result<(serde_json::Value, Vec<Step>), String> {
    let opts = &ctx.opts;
    let deadline = Instant::now() + opts.ready_timeout;
    if !wait_ready(&ctx.endpoint, "/api/v1/readyz", deadline).await {
        return Err(format!("the server was not ready within {:?}", opts.ready_timeout));
    }
    if let Some(addr) = opts.metrics_addr {
        let metrics = Endpoint::plain(SocketAddr::new(addr.ip(), addr.port()));
        if !wait_ready(&metrics, "/readyz", deadline).await {
            return Err("the metrics listener never answered /readyz with 200".into());
        }
    }
    let ready_ms = opts.spawned_at_ms.map(|spawned| epoch_ms().saturating_sub(spawned));
    match ready_ms {
        Some(ms) => progress(format!("ready {ms} ms after the start")),
        None => progress("ready (no --spawned-at-ms: time to ready not measured)"),
    }
    let start = ctx.probe.sample();
    let stats = Stats::new();
    let window = measure(ctx, &stats, opts.warmup, opts.duration).await;
    let end = window.end_sample;
    let step = Step::new("idle")
        .figure("ready ms", ready_ms.map(|v| json!(v)).unwrap_or(json!(null)))
        .figure("idle CPU %", window.cpu())
        .figure("RSS MiB", window.rss())
        .figure("PSS MiB", end.map(|s| mib(s.pss)).unwrap_or(0.0))
        .figure("processes", end.map(|s| s.processes).unwrap_or(0))
        .detail(json!({
            "readyMs": ready_ms,
            "rssAtReadyMiB": start.map(|s| mib(s.rss)),
            "window": window.json(),
        }));
    Ok((json!({}), vec![step]))
}
