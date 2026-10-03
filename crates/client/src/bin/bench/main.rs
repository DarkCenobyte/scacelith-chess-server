//! `scacelith-bench`: the load generator of the Scacelith server benchmark (docs/BENCHMARK.md).
//!
//! One tool drives either server with the same scenarios, the same REST calls and the same
//! measurements; only the realtime codec differs (`--target rust`: protocol v1 through the SDK,
//! `--target node`: protocol 3 through a small isolated codec). Every run prints a JSON report
//! (and writes it with `--out`); `scacelith-bench table` turns reports into Markdown tables.

mod cli;
mod conn;
mod connections;
mod ctx;
mod games;
mod idle;
mod matchmaking;
mod procfs;
mod proto3;
mod report;
mod rest;
mod stats;

use std::process::ExitCode;
use std::sync::Arc;

use cli::{Command, Options, Scenario, USAGE};
use ctx::{Ctx, epoch_ms, progress};

async fn run(opts: Options) -> Result<serde_json::Value, String> {
    let started = epoch_ms();
    let ctx: Arc<Ctx> = Ctx::new(opts)?;
    if ctx.opts.server_pid.is_some() && !ctx.probe.alive() {
        return Err(format!(
            "no process {} to sample (--server-pid)",
            ctx.opts.server_pid.unwrap_or_default()
        ));
    }
    progress(format!(
        "{} against {} ({}) at {}",
        ctx.opts.scenario.name(),
        ctx.opts.label,
        ctx.opts.target.name(),
        ctx.opts.addr
    ));
    let (params, steps) = match ctx.opts.scenario {
        Scenario::Idle => idle::run(&ctx).await?,
        Scenario::Connections => connections::run(&ctx).await?,
        Scenario::Games => games::run(&ctx).await?,
        Scenario::Matchmaking => matchmaking::run(&ctx).await?,
        Scenario::Rest => rest::run(&ctx).await?,
        Scenario::Login => rest::run_login(&ctx).await?,
    };
    Ok(report::build(&ctx.opts, started, params, &steps))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match Command::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("scacelith-bench: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let opts = match command {
        Command::Help => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Command::Table(files) => {
            return match report::table(&files) {
                Ok(md) => {
                    print!("{md}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("scacelith-bench: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Run(opts) => *opts,
    };
    let out = opts.out.clone();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("scacelith-bench: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(opts));
    // Connections still closing must not hold the process.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    match result {
        Ok(report) => {
            let text = serde_json::to_string_pretty(&report).unwrap_or_default();
            println!("{text}");
            if let Some(path) = out
                && let Err(e) = std::fs::write(&path, format!("{text}\n"))
            {
                eprintln!("scacelith-bench: cannot write {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("scacelith-bench: {e}");
            ExitCode::FAILURE
        }
    }
}
