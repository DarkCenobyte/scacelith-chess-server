//! `rest-diff`: runs the former Node.js server and the Rust server side by side with equivalent
//! settings, replays the same scripted scenarios against both, compares every answer (status,
//! headers, body after normalisation of the volatile values) and prints a report of every
//! difference. See README.md.

mod crypto;
mod diff;
mod duo;
mod http;
mod json;
mod normalize;
mod realtime;
mod report;
mod scenarios;
mod servers;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::duo::Duo;
use crate::report::Report;
use crate::servers::{Cert, Kind, Programs, Server};

const USAGE: &str = "Usage: rest-diff [options]

Options:
  --node PATH        node executable (default: $NODE, else /opt/nvm/versions/node/v24.21.0/bin/node)
  --node-dir PATH    the Node server tree (default: $SCACELITH_NODE_DIR, else
                     /home/user/rsw/node-ref/dedicated-server)
  --rust PATH        scacelith-server binary (default: target/debug/scacelith-server of the workspace)
  --profile NAME     run only this profile (repeatable)
  --scenario NAME    run only this scenario (repeatable)
  --accepted FILE    accepted deviations (default: tools/rest-diff/accepted.txt)
  --report FILE      also write the report to FILE
  --work DIR         directory for the servers' data (default: a new directory under the temp dir)
  --keep             keep the data directories, with the logs of both servers
  --list             list the profiles and their scenarios
  -h, --help         this help

Exit status: 0 no open difference, 1 open differences, 2 usage or start-up error.
";

struct Options {
    programs: Programs,
    profiles: Vec<String>,
    scenarios: Vec<String>,
    accepted: PathBuf,
    report: Option<PathBuf>,
    work: Option<PathBuf>,
    keep: bool,
    list: bool,
}

fn workspace_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn parse_args() -> Result<Options, String> {
    let mut o = Options {
        programs: Programs {
            node: std::env::var_os("NODE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/opt/nvm/versions/node/v24.21.0/bin/node")),
            node_dir: std::env::var_os("SCACELITH_NODE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/home/user/rsw/node-ref/dedicated-server")),
            rust: workspace_dir().join("target/debug/scacelith-server"),
        },
        profiles: Vec::new(),
        scenarios: Vec::new(),
        accepted: Path::new(env!("CARGO_MANIFEST_DIR")).join("accepted.txt"),
        report: None,
        work: None,
        keep: false,
        list: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--node" => o.programs.node = value("--node")?.into(),
            "--node-dir" => o.programs.node_dir = value("--node-dir")?.into(),
            "--rust" => o.programs.rust = value("--rust")?.into(),
            "--profile" => o.profiles.push(value("--profile")?),
            "--scenario" => o.scenarios.push(value("--scenario")?),
            "--accepted" => o.accepted = value("--accepted")?.into(),
            "--report" => o.report = Some(value("--report")?.into()),
            "--work" => o.work = Some(value("--work")?.into()),
            "--keep" => o.keep = true,
            "--list" => o.list = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(o)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("rest-diff: {e}\n");
            }
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let profiles = scenarios::profiles();
    if opts.list {
        for p in &profiles {
            println!("{}: {}", p.name, p.about);
            for (name, _) in &p.scenarios {
                println!("  {name}");
            }
        }
        return ExitCode::SUCCESS;
    }
    for name in opts.profiles.iter() {
        if !profiles.iter().any(|p| p.name == name) {
            eprintln!("rest-diff: unknown profile {name}");
            return ExitCode::from(2);
        }
    }
    let accepted = match std::fs::read_to_string(&opts.accepted)
        .map_err(|e| e.to_string())
        .and_then(|t| report::parse_accepted(&t))
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rest-diff: {}: {e}", opts.accepted.display());
            return ExitCode::from(2);
        }
    };
    for p in [&opts.programs.node, &opts.programs.rust] {
        if !p.exists() {
            eprintln!(
                "rest-diff: {} not found (build the server with `cargo build -p scacelith-server`)",
                p.display()
            );
            return ExitCode::from(2);
        }
    }
    let work = opts
        .work
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("rest-diff-{}", std::process::id())));
    if let Err(e) = std::fs::create_dir_all(&work) {
        eprintln!("rest-diff: {}: {e}", work.display());
        return ExitCode::from(2);
    }
    let cert = match Cert::create(&work) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rest-diff: {e}");
            return ExitCode::from(2);
        }
    };

    let mut report = Report { accepted, ..Report::default() };
    for profile in
        profiles.iter().filter(|p| opts.profiles.is_empty() || opts.profiles.iter().any(|n| n == p.name))
    {
        let chosen: Vec<_> = profile
            .scenarios
            .iter()
            .filter(|(name, _)| opts.scenarios.is_empty() || opts.scenarios.iter().any(|s| s == name))
            .collect();
        if chosen.is_empty() {
            continue;
        }
        eprintln!("== profile {}: starting the servers", profile.name);
        let env: Vec<(String, String)> =
            profile.env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let dir = work.join(profile.name);
        let (node_dir, rust_dir) = (dir.join("node"), dir.join("rust"));
        let (node, rust) = tokio::join!(
            Server::start(Kind::Node, &opts.programs, &cert, &node_dir, &env),
            Server::start(Kind::Rust, &opts.programs, &cert, &rust_dir, &env),
        );
        let (node, rust) = match (node, rust) {
            (Ok(n), Ok(r)) => (n, r),
            (n, r) => {
                for s in [n, r] {
                    match s {
                        Ok(s) => s.stop().await,
                        Err(e) => eprintln!("rest-diff: {e}"),
                    }
                }
                return ExitCode::from(2);
            }
        };
        report.profiles.push(profile.name.to_string());
        let mut duo = Duo::new(profile.name, node, rust, report);
        for (name, run) in chosen {
            eprintln!("   scenario {name}");
            duo.scenario(name);
            run(&mut duo).await;
        }
        duo.disconnect_all().await;
        let (r, node, rust) = duo.finish();
        report = r;
        if opts.keep {
            let _ = std::fs::write(dir.join("node.log"), node.log_text());
            let _ = std::fs::write(dir.join("rust.log"), rust.log_text());
        }
        tokio::join!(node.stop(), rust.stop());
    }
    if !opts.keep {
        let _ = std::fs::remove_dir_all(&work);
    }
    let text = report.render();
    print!("{text}");
    if let Some(path) = &opts.report
        && let Err(e) = std::fs::write(path, &text)
    {
        eprintln!("rest-diff: {}: {e}", path.display());
    }
    if report.open_count() == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) }
}
