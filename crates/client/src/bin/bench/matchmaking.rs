//! Scenario (d): matchmaking burst.
//!
//! M connected players join the casual queue of `--category` at the same instant; each one
//! measures the time from its `QueueJoin` to the `GameSnapshot` of the game it was matched into
//! (`match` histogram), and the burst's makespan runs from the start signal to the last
//! snapshot. White then aborts the game (casual games: no conduct penalty) and the next burst
//! starts once everybody is back. One warm-up burst, then `--rounds` measured bursts.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::conn::{Cmd, Conn, Event};
use crate::ctx::{Ctx, progress};
use crate::procfs::window_json;
use crate::report::Step;
use crate::stats::{Hist, Stats, round3};

/// Signal value that ends the player tasks.
const STOP: u64 = u64::MAX;
/// Deadline of the abort after a match.
const ABORT_TIMEOUT: Duration = Duration::from_secs(5);

/// What a player reports after a burst: its round, and when it was matched.
struct Matched {
    round: u64,
    /// `QueueJoin` sent to `GameSnapshot` read, and the read instant.
    at: Option<(Duration, Instant)>,
}

/// Runs one burst for a player; `Err` when its connection is gone.
async fn burst(
    conn: &mut Conn,
    category: &str,
    timeout: Duration,
    stats: &Stats,
) -> Result<Option<(Duration, Instant)>, ()> {
    let sent = Instant::now();
    let seq = conn.send(Cmd::QueueJoin { category, rated: false }).map_err(drop)?;
    let deadline = tokio::time::Instant::now() + timeout;
    let snapshot = loop {
        match tokio::time::timeout_at(deadline, conn.recv()).await {
            Err(_) => {
                stats.add("error.match_timeout", 1);
                let _ = conn.send(Cmd::QueueLeave);
                return Ok(None);
            }
            Ok(Err(_)) => return Err(()),
            Ok(Ok((Event::Snapshot(s), at))) if !s.over => break (s, at),
            Ok(Ok((Event::Error { r#ref, code }, _))) if r#ref == seq => {
                stats.add(&format!("error.queue_{code}"), 1);
                return Ok(None);
            }
            Ok(Ok(_)) => {}
        }
    };
    let (snap, at) = snapshot;
    if snap.you_white {
        conn.send(Cmd::Abort { game: snap.game }).map_err(drop)?;
    }
    let deadline = tokio::time::Instant::now() + ABORT_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, conn.recv()).await {
            Err(_) => {
                stats.add("error.abort_timeout", 1);
                break;
            }
            Ok(Err(_)) => return Err(()),
            Ok(Ok((Event::GameEnd { game }, _))) if game == snap.game => break,
            Ok(Ok(_)) => {}
        }
    }
    Ok(Some((at.duration_since(sent), at)))
}

async fn player(
    mut conn: Conn,
    category: String,
    timeout: Duration,
    mut go: watch::Receiver<u64>,
    results: mpsc::UnboundedSender<Matched>,
    stats: Arc<Stats>,
) {
    loop {
        if go.changed().await.is_err() {
            break;
        }
        let round = *go.borrow_and_update();
        if round == STOP {
            break;
        }
        match burst(&mut conn, &category, timeout, &stats).await {
            Ok(at) => {
                let _ = results.send(Matched { round, at });
            }
            Err(()) => {
                stats.add(&format!("drop.{}", conn.close_code().unwrap_or(1006)), 1);
                let _ = results.send(Matched { round, at: None });
                return;
            }
        }
    }
    conn.close();
    conn.wait_closed(Duration::from_secs(2)).await;
}

/// Runs the scenario.
pub async fn run(ctx: &Arc<Ctx>) -> Result<(Value, Vec<Step>), String> {
    let max = ctx.opts.steps.iter().copied().max().unwrap_or(0);
    let accounts = ctx.need_accounts(max)?.to_vec();
    let mut steps = Vec::new();
    for &m in &ctx.opts.steps {
        progress(format!("matchmaking: connecting {m} players"));
        let stats = Stats::new();
        stats.set_measuring(true);
        let mut connecting = JoinSet::new();
        for account in &accounts[..m] {
            let (ctx, token, stats) = (ctx.clone(), account.token.clone(), stats.clone());
            connecting.spawn(async move { ctx.connect(&token, &stats).await });
        }
        let (go_tx, go_rx) = watch::channel(0u64);
        let (res_tx, mut res_rx) = mpsc::unbounded_channel();
        let mut players = JoinSet::new();
        while let Some(joined) = connecting.join_next().await {
            if let Ok(Some(conn)) = joined {
                players.spawn(player(
                    conn,
                    ctx.opts.category.clone(),
                    ctx.opts.match_timeout,
                    go_rx.clone(),
                    res_tx.clone(),
                    stats.clone(),
                ));
            }
        }
        let connected = players.len();
        if connected < 2 {
            return Err(format!("matchmaking: only {connected} of {m} players connected"));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;

        let mut hist = Hist::default();
        let mut makespans = Vec::new();
        let mut unmatched = 0u64;
        let mut s0 = None;
        let total_rounds = 1 + ctx.opts.rounds as u64;
        for round in 1..=total_rounds {
            let measured = round > 1;
            if round == 2 {
                s0 = ctx.probe.sample();
            }
            let go_at = Instant::now();
            let _ = go_tx.send(round);
            let deadline =
                tokio::time::Instant::now() + ctx.opts.match_timeout + ABORT_TIMEOUT + Duration::from_secs(5);
            let mut answered = 0;
            let mut last: Option<Instant> = None;
            while answered < connected {
                match tokio::time::timeout_at(deadline, res_rx.recv()).await {
                    Ok(Some(r)) if r.round == round => {
                        answered += 1;
                        match r.at {
                            Some((wait, at)) => {
                                if measured {
                                    hist.record(wait);
                                }
                                last = Some(last.map_or(at, |l| l.max(at)));
                            }
                            None if measured => unmatched += 1,
                            None => {}
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            if measured {
                unmatched += (connected - answered) as u64;
                if let Some(last) = last {
                    makespans.push(last.duration_since(go_at).as_secs_f64() * 1000.0);
                }
            }
            progress(format!(
                "matchmaking: burst {round}/{total_rounds}{} done in {:.0} ms",
                if measured { "" } else { " (warm-up)" },
                last.map_or(0.0, |l| l.duration_since(go_at).as_secs_f64() * 1000.0)
            ));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let s1 = ctx.probe.sample();
        let _ = go_tx.send(STOP);
        let _ = tokio::time::timeout(Duration::from_secs(30), async {
            while players.join_next().await.is_some() {}
        })
        .await;

        let summary = hist.summary_ms();
        let figure = |k: &str| summary[k].as_f64().unwrap_or(0.0);
        let mean_makespan = if makespans.is_empty() {
            0.0
        } else {
            round3(makespans.iter().sum::<f64>() / makespans.len() as f64)
        };
        let server = window_json(s0.as_ref(), s1.as_ref());
        let step = Step::new(format!("{m} players"))
            .figure("connected", connected)
            .figure("match p50 ms", figure("p50"))
            .figure("match p90 ms", figure("p90"))
            .figure("match p99 ms", figure("p99"))
            .figure("match max ms", figure("max"))
            .figure("makespan ms", mean_makespan)
            .figure("unmatched", unmatched)
            .figure("CPU %", server.get("cpuPercent").cloned().unwrap_or(Value::Null))
            .detail(json!({
                "matchMs": summary,
                "makespansMs": makespans.iter().map(|v| round3(*v)).collect::<Vec<_>>(),
                "counters": stats.counters_json(),
                "connectLatencyMs": stats.hists_json(),
                "server": server,
            }));
        steps.push(step);
    }
    let params = json!({
        "steps": ctx.opts.steps,
        "category": ctx.opts.category,
        "rated": false,
        "rounds": ctx.opts.rounds,
        "warmupRounds": 1,
    });
    Ok((params, steps))
}
