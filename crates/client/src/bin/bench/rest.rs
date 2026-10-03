//! Scenarios (e): the REST endpoints, and login throughput.
//!
//! Each endpoint gets its own window: `--concurrency` workers, each on its own keep-alive HTTPS
//! connection (opened again only when the server closes it), send requests back to back. The
//! per-account limits of the servers (`public_read` 60 per minute for the game routes, `gif` 30
//! per minute) are fixed in their code, so the authenticated requests rotate over every account
//! of the tokens file; a `429` shows up as `error.429` when there are too few.
//!
//! * `info`: `GET /api/v1/info`;
//! * `leaderboard`: `GET /api/v1/leaderboard?category=3+2`;
//! * `pgn`: `GET /api/v1/games/:id/pgn` over the set-up games;
//! * `gif-cold`: `GET /api/v1/games/:id/gif` with a new `delay` and `orientation` for every
//!   request, so that every one is rendered (`--gif-cold-concurrency` workers);
//! * `gif-cached`: the same with the default options over at most 8 games, so that after the
//!   warm-up every request is served from the cache.
//!
//! The login scenario registers `--accounts` accounts through the API, then logs in with them
//! in turn at `--concurrency`: its cost is the password hash, whose parameters the harness
//! records in the report (`--meta`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use scacelith_client::http::{HttpConnection, Request};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use crate::ctx::{Ctx, measure, progress};
use crate::games::{Pair, PlayConfig};
use crate::report::Step;
use crate::stats::{Stats, round3};

/// One request to send.
struct Planned {
    path: String,
    token: Option<String>,
    body: Option<Vec<u8>>,
}

/// Plans the `n`-th request of an endpoint.
type Planner = Arc<dyn Fn(u64) -> Planned + Send + Sync>;

/// Sends planned requests on one keep-alive connection until `stop`.
async fn worker(
    ctx: Arc<Ctx>,
    stats: Arc<Stats>,
    stop: Arc<AtomicBool>,
    counter: Arc<AtomicU64>,
    plan: Planner,
) {
    let mut conn = None;
    while !stop.load(Ordering::Relaxed) {
        let http = match &mut conn {
            Some(c) => c,
            None => match HttpConnection::open(&ctx.endpoint).await {
                Ok(c) => {
                    stats.add("connections_opened", 1);
                    conn.insert(c)
                }
                Err(_) => {
                    stats.add("error.connect", 1);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let planned = plan(counter.fetch_add(1, Ordering::Relaxed));
        let mut req = match &planned.body {
            Some(body) => Request::post_json(&planned.path, body),
            None => Request::get(&planned.path),
        };
        if let Some(token) = &planned.token {
            req = req.with_bearer(token);
        }
        let started = Instant::now();
        match http.send(&req).await {
            Ok(resp) => {
                stats.latency("request", started.elapsed());
                if resp.is_success() {
                    stats.add("ok", 1);
                    stats.add("bytes", resp.body.len() as u64);
                } else {
                    stats.add(&format!("error.{}", resp.status), 1);
                }
                if !http.is_reusable() {
                    conn = None;
                }
            }
            Err(_) => {
                stats.add("error.transport", 1);
                conn = None;
            }
        }
    }
}

/// Runs `concurrency` workers through a warm-up and a window; returns the step.
async fn measure_endpoint(ctx: &Arc<Ctx>, name: &str, concurrency: usize, plan: Planner) -> Step {
    progress(format!("{name}: {concurrency} connections"));
    let stats = Stats::new();
    let stop = Arc::new(AtomicBool::new(false));
    let counter = Arc::new(AtomicU64::new(0));
    let mut set = JoinSet::new();
    for _ in 0..concurrency {
        set.spawn(worker(ctx.clone(), stats.clone(), stop.clone(), counter.clone(), plan.clone()));
    }
    let window = measure(ctx, &stats, ctx.opts.warmup, ctx.opts.duration).await;
    stop.store(true, Ordering::Relaxed);
    let _ = tokio::time::timeout(Duration::from_secs(60), async { while set.join_next().await.is_some() {} })
        .await;
    let errors: u64 = window.counters.iter().filter(|(k, _)| k.starts_with("error.")).map(|(_, v)| v).sum();
    Step::new(name)
        .figure("conns", concurrency)
        .figure("req/s", window.rate("ok"))
        .figure("p50 ms", window.latency("request", "p50"))
        .figure("p90 ms", window.latency("request", "p90"))
        .figure("p99 ms", window.latency("request", "p99"))
        .figure("max ms", window.latency("request", "max"))
        .figure("errors", errors)
        .figure("CPU %", window.cpu())
        .figure("RSS MiB", window.rss())
        .figure("load CPU %", window.load_cpu())
        .detail(json!({
            "errors": window.errors_json(),
            "bytesPerRequest": if window.count("ok") > 0 { window.count("bytes") / window.count("ok") } else { 0 },
            "window": window.json(),
        }))
}

/// Plays the set-up games (fast moves, then a resignation) and returns their ids.
async fn setup_games(ctx: &Arc<Ctx>) -> Result<Vec<u64>, String> {
    let n = ctx.opts.setup_games.max(1);
    let accounts = ctx.need_accounts(2 * n)?.to_vec();
    progress(format!("rest: playing {n} set-up games of {} plies", ctx.opts.setup_plies));
    let cfg = PlayConfig {
        move_interval: Duration::ZERO,
        jitter: 0.0,
        gesture_period: None,
        max_plies: ctx.opts.setup_plies,
        tc: ctx.opts.tc,
        rated: false,
    };
    let stats = Stats::new();
    let mut set = JoinSet::new();
    for i in 0..n {
        let (ctx, a, b, stats) =
            (ctx.clone(), accounts[2 * i].clone(), accounts[2 * i + 1].clone(), stats.clone());
        set.spawn(async move {
            let mut pair = Pair::connect(&ctx, &a, &b, stats, ctx.rng(i as u64)).await?;
            // The sender lives as long as the game: a dropped sender reads as a stop.
            let (_stop_tx, mut stop) = watch::channel(false);
            let mut game = None;
            if pair.start(&cfg, &mut stop).await.is_ok()
                && pair.play(&cfg, &mut stop).await == crate::games::Outcome::Finished
            {
                game = Some(pair.game());
            }
            pair.close().await;
            game
        });
    }
    let mut ids = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(Some(id)) = joined {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err(format!("no set-up game finished: {}", stats.counters_json()));
    }
    ids.sort_unstable();
    Ok(ids)
}

/// A token of the tokens file, in turn.
fn rotating_token(ctx: &Ctx, n: u64) -> Option<String> {
    let accounts = &ctx.accounts;
    (!accounts.is_empty()).then(|| accounts[(n % accounts.len() as u64) as usize].token.clone())
}

/// Runs the REST scenario.
pub async fn run(ctx: &Arc<Ctx>) -> Result<(Value, Vec<Step>), String> {
    let needs_games = ctx.opts.endpoints.iter().any(|e| e != "info" && e != "leaderboard");
    let ids: Arc<Vec<u64>> = Arc::new(if needs_games { setup_games(ctx).await? } else { Vec::new() });
    let mut steps = Vec::new();
    for endpoint in &ctx.opts.endpoints {
        let ids = ids.clone();
        let c = ctx.clone();
        let (concurrency, plan): (usize, Planner) = match endpoint.as_str() {
            "info" => (
                ctx.opts.concurrency,
                Arc::new(|_| Planned { path: "/api/v1/info".into(), token: None, body: None }),
            ),
            "leaderboard" => (
                ctx.opts.concurrency,
                Arc::new(|_| Planned {
                    path: "/api/v1/leaderboard?category=3%2B2".into(),
                    token: None,
                    body: None,
                }),
            ),
            "pgn" => (
                ctx.opts.concurrency,
                Arc::new(move |n| Planned {
                    path: format!("/api/v1/games/{}/pgn", ids[(n % ids.len() as u64) as usize]),
                    token: rotating_token(&c, n),
                    body: None,
                }),
            ),
            "gif-cold" => (
                ctx.opts.gif_cold_concurrency,
                Arc::new(move |n| {
                    // Every (game, delay, orientation) is a distinct picture: 2 x 2901 per game.
                    let games = ids.len() as u64;
                    let delay = 100 + (n / games) % 2901;
                    let orientation = if (n / (games * 2901)).is_multiple_of(2) { "white" } else { "black" };
                    Planned {
                        path: format!(
                            "/api/v1/games/{}/gif?delay={delay}&orientation={orientation}",
                            ids[(n % games) as usize]
                        ),
                        token: rotating_token(&c, n),
                        body: None,
                    }
                }),
            ),
            "gif-cached" => (
                ctx.opts.concurrency,
                Arc::new(move |n| {
                    let keys = ids.len().min(8) as u64;
                    Planned {
                        path: format!("/api/v1/games/{}/gif", ids[(n % keys) as usize]),
                        token: rotating_token(&c, n),
                        body: None,
                    }
                }),
            ),
            other => return Err(format!("unknown endpoint {other}")),
        };
        steps.push(measure_endpoint(ctx, endpoint, concurrency, plan).await);
    }
    let params = json!({
        "endpoints": ctx.opts.endpoints,
        "concurrency": ctx.opts.concurrency,
        "gifColdConcurrency": ctx.opts.gif_cold_concurrency,
        "setupGames": ids.len(),
        "setupPlies": ctx.opts.setup_plies,
        "tokens": ctx.accounts.len(),
    });
    Ok((params, steps))
}

/// Runs the login scenario.
pub async fn run_login(ctx: &Arc<Ctx>) -> Result<(Value, Vec<Step>), String> {
    let mut rng = ctx.rng(0x6c_6f67_696e);
    let tag = format!("{:08x}", rng.next_u64() as u32);
    let password = format!("Bench-{tag}-{:016x}", rng.next_u64());
    let names: Vec<String> = (0..ctx.opts.accounts.max(1)).map(|i| format!("lg{tag}{i:04}")).collect();
    progress(format!("login: registering {} accounts", names.len()));
    let started = Instant::now();
    let slots = Arc::new(Semaphore::new(ctx.opts.concurrency));
    let mut set = JoinSet::new();
    for name in &names {
        let (ctx, slots, name, password) = (ctx.clone(), slots.clone(), name.clone(), password.clone());
        set.spawn(async move {
            let _slot = slots.acquire().await.ok()?;
            ctx.api
                .register(&name, &format!("{name}@bench.invalid"), &password)
                .await
                .err()
                .map(|e| e.to_string())
        });
    }
    let mut failures = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(Some(error)) = joined {
            failures.push(error);
        }
    }
    let register_s = started.elapsed().as_secs_f64();
    if failures.len() == names.len() {
        return Err(format!("login: every registration failed, first: {}", failures[0]));
    }
    let names = Arc::new(names);
    let plan: Planner = {
        let names = names.clone();
        Arc::new(move |n| {
            let name = &names[(n % names.len() as u64) as usize];
            let body = json!({ "login": name, "password": password, "clientLabel": "bench" });
            Planned {
                path: "/api/v1/auth/login".into(),
                token: None,
                body: Some(body.to_string().into_bytes()),
            }
        })
    };
    let step = measure_endpoint(ctx, "login", ctx.opts.concurrency, plan).await;
    let step = step.figure("register s", round3(register_s));
    let params = json!({
        "accounts": names.len(),
        "registrationFailures": failures.len(),
        "concurrency": ctx.opts.concurrency,
    });
    Ok((params, vec![step]))
}
