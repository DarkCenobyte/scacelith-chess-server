//! Command-line options.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::conn::Target;

/// The usage text.
pub const USAGE: &str = "\
Usage: scacelith-bench <scenario> [options]
       scacelith-bench table <result.json>...

Load generator of the Scacelith server: one tool, two protocol adapters (docs/BENCHMARK.md).
Each scenario warms up, then measures for a fixed window, and prints a JSON report.

Scenarios
  idle          time to ready (with --spawned-at-ms), then idle CPU and memory
  connections   ramp authenticated WSS connections to each --steps count and hold them:
                handshake rate and latency, memory per connection, failures
  games         --steps N games (2N bots) playing legal moves at --move-interval-ms, with head
                gestures at --gesture-hz per player: move relay and confirmation latency, gesture
                relay latency, server CPU
  matchmaking   --steps M players join the queue at once: time from QueueJoin to GameSnapshot
  rest          GET /info, /leaderboard, /games/:id/pgn, /games/:id/gif (cold, cached) at
                --concurrency keep-alive connections, one window per endpoint
  login         register --accounts accounts, then log in repeatedly at --concurrency
  table         Markdown tables of result files

Server
  --target rust|node      protocol: rust = v1 (scacelith.rt1), node = 3 (scacelith.v1)   [rust]
  --addr IP:PORT          API and WebSocket address                         [127.0.0.1:18443]
  --host NAME             TLS server name and Host header                   [localhost]
  --ca FILE               trust only this PEM certificate (the server's self-signed one)
  --insecure              accept any certificate (test machines only)
  --plain                 no TLS
  --tls-resume            resume TLS sessions (default: a full handshake per connection)
  --tokens FILE           accounts: username<TAB>token (or a token) per line
  --server-pid PID        root of the server's process tree, for CPU and memory (/proc)
  --metrics-addr IP:PORT  the server's metrics listener, for /readyz         [none]

Measurement
  --warmup-s S            before each measurement window                     [5]
  --duration-s S          measurement window                                 [20]
  --steps A,B,...         connections [1000,5000,10000], games [100,500,1000],
                          matchmaking [200,1000]
  --inflight N            connection handshakes in flight                    [200]
  --connect-timeout-ms N  TCP + TLS + upgrade + Hello deadline               [30000]
  --seed N                random seed                                        [random]

idle
  --spawned-at-ms MS      epoch milliseconds when the server was started (time to ready)
  --ready-timeout-s S                                                        [120]
games
  --move-interval-ms N    think time per move (uniform +-jitter)             [1000]
  --jitter F              think time spread, 0..1                            [0.5]
  --gesture-hz N          head gestures per second per player (0 = none)     [10]
  --max-plies N           resign at this ply                                 [80]
  --tc M+I                time control                                       [3+2]
  --rated true|false      rated games                                        [true]
  --between-games-ms N    pause between two games of a pair                  [1000]
  --start-rate N          games started per second                           [200]
matchmaking
  --category M+I          queue                                              [3+2]
  --rounds N              measured bursts per step (after one warm-up burst) [3]
  --match-timeout-s S     deadline of one burst                              [60]
rest
  --concurrency N         keep-alive connections (login: parallel logins)    [32, login 8]
  --gif-cold-concurrency N  connections of gif-cold (renders are slow)       [4]
  --endpoints LIST        info,leaderboard,pgn,gif-cold,gif-cached           [all]
  --setup-games N         games played first, for /pgn and /gif              [16]
  --setup-plies N         plies of those games                               [40]
login
  --accounts N            accounts registered for the logins                 [16]

Output
  --out FILE              write the JSON report there (always printed on stdout)
  --label TEXT            run name stored in the report (node24, node26, rust)
  --meta K=V              extra fact stored in the report (repeatable)
";

/// A scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Time to ready and idle footprint.
    Idle,
    /// Connection capacity.
    Connections,
    /// Live games.
    Games,
    /// Matchmaking burst.
    Matchmaking,
    /// REST endpoints.
    Rest,
    /// Login throughput.
    Login,
}

impl Scenario {
    /// The command-line name.
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Idle => "idle",
            Scenario::Connections => "connections",
            Scenario::Games => "games",
            Scenario::Matchmaking => "matchmaking",
            Scenario::Rest => "rest",
            Scenario::Login => "login",
        }
    }

    fn parse(s: &str) -> Option<Scenario> {
        [
            Scenario::Idle,
            Scenario::Connections,
            Scenario::Games,
            Scenario::Matchmaking,
            Scenario::Rest,
            Scenario::Login,
        ]
        .into_iter()
        .find(|sc| sc.name() == s)
    }
}

/// How the bench trusts the server's certificate.
#[derive(Clone, Debug)]
pub enum TlsMode {
    /// Only this PEM certificate.
    Ca(PathBuf),
    /// Any certificate.
    Insecure,
    /// No TLS.
    Plain,
}

/// Every option of a scenario run.
#[derive(Clone, Debug)]
pub struct Options {
    /// The scenario to run.
    pub scenario: Scenario,
    /// The protocol adapter.
    pub target: Target,
    /// API and WebSocket address.
    pub addr: SocketAddr,
    /// TLS server name and `Host` header.
    pub host: String,
    /// Certificate trust.
    pub tls: TlsMode,
    /// Resume TLS sessions.
    pub tls_resume: bool,
    /// Accounts file.
    pub tokens: Option<PathBuf>,
    /// Root of the server's process tree.
    pub server_pid: Option<u32>,
    /// The server's metrics listener.
    pub metrics_addr: Option<SocketAddr>,
    /// Warm-up before each measurement window.
    pub warmup: Duration,
    /// Measurement window.
    pub duration: Duration,
    /// Sizes measured one after the other (connections, games, players).
    pub steps: Vec<usize>,
    /// Connection handshakes in flight.
    pub inflight: usize,
    /// Deadline of one connection, Hello included.
    pub connect_timeout: Duration,
    /// Random seed.
    pub seed: Option<u64>,
    /// When the server was started (epoch milliseconds).
    pub spawned_at_ms: Option<u64>,
    /// Deadline of the readiness wait.
    pub ready_timeout: Duration,
    /// Think time per move.
    pub move_interval: Duration,
    /// Think time spread (0..1).
    pub jitter: f64,
    /// Head gestures per second per player.
    pub gesture_hz: f64,
    /// Resign at this ply.
    pub max_plies: u16,
    /// Time control of the games: base seconds, increment seconds.
    pub tc: (u16, u8),
    /// Rated games.
    pub rated: bool,
    /// Pause between two games of a pair.
    pub between_games: Duration,
    /// Games started per second.
    pub start_rate: f64,
    /// Matchmaking queue.
    pub category: String,
    /// Measured matchmaking bursts per step.
    pub rounds: usize,
    /// Deadline of one matchmaking burst.
    pub match_timeout: Duration,
    /// REST connections, or parallel logins.
    pub concurrency: usize,
    /// Connections of the gif-cold endpoint.
    pub gif_cold_concurrency: usize,
    /// REST endpoints measured.
    pub endpoints: Vec<String>,
    /// Games played before the REST measurements.
    pub setup_games: usize,
    /// Plies of those games.
    pub setup_plies: u16,
    /// Accounts registered for the login scenario.
    pub accounts: usize,
    /// Report file.
    pub out: Option<PathBuf>,
    /// Run name.
    pub label: String,
    /// Extra facts for the report.
    pub meta: Vec<(String, String)>,
}

/// What the command line asks for.
#[derive(Debug)]
pub enum Command {
    /// Print the usage.
    Help,
    /// Run a scenario.
    Run(Box<Options>),
    /// Markdown tables of result files.
    Table(Vec<PathBuf>),
}

/// The REST endpoints, in run order.
pub const ENDPOINTS: [&str; 5] = ["info", "leaderboard", "pgn", "gif-cold", "gif-cached"];

fn secs(v: &str, name: &str) -> Result<Duration, String> {
    let s: f64 = v.parse().map_err(|_| format!("--{name}: not a number: {v}"))?;
    if !(0.0..=86_400.0).contains(&s) {
        return Err(format!("--{name}: out of range: {v}"));
    }
    Ok(Duration::from_secs_f64(s))
}

fn millis(v: &str, name: &str) -> Result<Duration, String> {
    let ms: u64 = v.parse().map_err(|_| format!("--{name}: not an integer: {v}"))?;
    Ok(Duration::from_millis(ms))
}

fn int<T: std::str::FromStr>(v: &str, name: &str) -> Result<T, String> {
    v.parse().map_err(|_| format!("--{name}: invalid value {v}"))
}

fn time_control(v: &str) -> Result<(u16, u8), String> {
    let (m, i) = v.split_once('+').ok_or_else(|| format!("--tc: expected M+I, got {v}"))?;
    let minutes: u16 = int(m, "tc")?;
    let inc: u8 = int(i, "tc")?;
    let base =
        minutes.checked_mul(60).filter(|b| (15..=10_800).contains(b)).ok_or("--tc: base out of range")?;
    Ok((base, inc))
}

impl Command {
    /// Parses the arguments after the program name.
    pub fn parse(args: &[String]) -> Result<Command, String> {
        let Some(first) = args.first() else { return Ok(Command::Help) };
        if first == "--help" || first == "-h" || first == "help" {
            return Ok(Command::Help);
        }
        if first == "table" {
            if args.len() < 2 {
                return Err("table: give at least one result file".into());
            }
            return Ok(Command::Table(args[1..].iter().map(PathBuf::from).collect()));
        }
        let scenario = Scenario::parse(first).ok_or_else(|| format!("unknown scenario {first}"))?;
        let mut o = Options::defaults(scenario);
        let mut steps_given = false;
        let mut concurrency_given = false;
        let mut i = 1;
        while i < args.len() {
            let arg = &args[i];
            let key = arg.strip_prefix("--").ok_or_else(|| format!("unexpected argument {arg}"))?;
            let (key, inline) = match key.split_once('=') {
                Some((k, v)) => (k, Some(v.to_string())),
                None => (key, None),
            };
            match key {
                "help" => return Ok(Command::Help),
                "plain" => o.tls = TlsMode::Plain,
                "insecure" => o.tls = TlsMode::Insecure,
                "tls-resume" => o.tls_resume = true,
                _ => {
                    let value = match inline {
                        Some(v) => v,
                        None => {
                            i += 1;
                            args.get(i).cloned().ok_or_else(|| format!("--{key} needs a value"))?
                        }
                    };
                    let v = value.as_str();
                    match key {
                        "target" => {
                            o.target =
                                Target::parse(v).ok_or_else(|| format!("--target: rust or node, not {v}"))?
                        }
                        "addr" => o.addr = int(v, key)?,
                        "host" => o.host = v.to_string(),
                        "ca" => o.tls = TlsMode::Ca(PathBuf::from(v)),
                        "tokens" => o.tokens = Some(PathBuf::from(v)),
                        "server-pid" => o.server_pid = Some(int(v, key)?),
                        "metrics-addr" => {
                            o.metrics_addr = if v == "none" { None } else { Some(int(v, key)?) }
                        }
                        "warmup-s" => o.warmup = secs(v, key)?,
                        "duration-s" => o.duration = secs(v, key)?,
                        "steps" => {
                            o.steps = v.split(',').map(|s| int(s.trim(), key)).collect::<Result<_, _>>()?;
                            steps_given = true;
                        }
                        "inflight" => o.inflight = int(v, key)?,
                        "connect-timeout-ms" => o.connect_timeout = millis(v, key)?,
                        "seed" => o.seed = Some(int(v, key)?),
                        "spawned-at-ms" => o.spawned_at_ms = Some(int(v, key)?),
                        "ready-timeout-s" => o.ready_timeout = secs(v, key)?,
                        "move-interval-ms" => o.move_interval = millis(v, key)?,
                        "jitter" => o.jitter = int::<f64>(v, key)?.clamp(0.0, 1.0),
                        "gesture-hz" => o.gesture_hz = int::<f64>(v, key)?.clamp(0.0, 60.0),
                        "max-plies" => o.max_plies = int(v, key)?,
                        "tc" => o.tc = time_control(v)?,
                        "category" => {
                            time_control(v)?;
                            o.category = v.to_string();
                        }
                        "rated" => o.rated = matches!(v, "true" | "1" | "yes"),
                        "between-games-ms" => o.between_games = millis(v, key)?,
                        "start-rate" => o.start_rate = int(v, key)?,
                        "rounds" => o.rounds = int(v, key)?,
                        "match-timeout-s" => o.match_timeout = secs(v, key)?,
                        "concurrency" => {
                            o.concurrency = int(v, key)?;
                            concurrency_given = true;
                        }
                        "endpoints" => {
                            let list: Vec<String> = v.split(',').map(|s| s.trim().to_string()).collect();
                            if let Some(bad) = list.iter().find(|e| !ENDPOINTS.contains(&e.as_str())) {
                                return Err(format!("--endpoints: unknown endpoint {bad}"));
                            }
                            o.endpoints = list;
                        }
                        "gif-cold-concurrency" => o.gif_cold_concurrency = int(v, key)?,
                        "setup-games" => o.setup_games = int(v, key)?,
                        "setup-plies" => o.setup_plies = int(v, key)?,
                        "accounts" => o.accounts = int(v, key)?,
                        "out" => o.out = Some(PathBuf::from(v)),
                        "label" => o.label = v.to_string(),
                        "meta" => {
                            let (k, val) =
                                v.split_once('=').ok_or_else(|| format!("--meta: expected K=V, got {v}"))?;
                            o.meta.push((k.to_string(), val.to_string()));
                        }
                        _ => return Err(format!("unknown option --{key}")),
                    }
                }
            }
            i += 1;
        }
        if !steps_given {
            o.steps = match scenario {
                Scenario::Connections => vec![1000, 5000, 10000],
                Scenario::Games => vec![100, 500, 1000],
                Scenario::Matchmaking => vec![200, 1000],
                _ => Vec::new(),
            };
        }
        if scenario == Scenario::Login && !concurrency_given {
            o.concurrency = 8;
        }
        if o.steps.contains(&0)
            || o.inflight == 0
            || o.concurrency == 0
            || o.gif_cold_concurrency == 0
            || o.rounds == 0
        {
            return Err("--steps, --inflight, the concurrencies and --rounds must be positive".into());
        }
        if scenario == Scenario::Matchmaking && o.steps.iter().any(|m| m % 2 == 1) {
            return Err("matchmaking: the player counts must be even".into());
        }
        if o.label.is_empty() {
            o.label = o.target.name().to_string();
        }
        Ok(Command::Run(Box::new(o)))
    }
}

impl Options {
    fn defaults(scenario: Scenario) -> Options {
        Options {
            scenario,
            target: Target::Rust,
            addr: SocketAddr::from(([127, 0, 0, 1], 18443)),
            host: "localhost".into(),
            tls: TlsMode::Insecure,
            tls_resume: false,
            tokens: None,
            server_pid: None,
            metrics_addr: None,
            warmup: Duration::from_secs(5),
            duration: Duration::from_secs(20),
            steps: Vec::new(),
            inflight: 200,
            connect_timeout: Duration::from_secs(30),
            seed: None,
            spawned_at_ms: None,
            ready_timeout: Duration::from_secs(120),
            move_interval: Duration::from_millis(1000),
            jitter: 0.5,
            gesture_hz: 10.0,
            max_plies: 80,
            tc: (180, 2),
            rated: true,
            between_games: Duration::from_millis(1000),
            start_rate: 200.0,
            category: "3+2".into(),
            rounds: 3,
            match_timeout: Duration::from_secs(60),
            concurrency: 32,
            gif_cold_concurrency: 4,
            endpoints: ENDPOINTS.iter().map(|s| s.to_string()).collect(),
            setup_games: 16,
            setup_plies: 40,
            accounts: 16,
            out: None,
            label: String::new(),
            meta: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Command, String> {
        Command::parse(&line.split_whitespace().map(str::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn options() {
        let Command::Run(o) =
            parse("games --target node --steps 10,20 --tc 5+3 --ca c.pem --meta a=b --rated=false").unwrap()
        else {
            panic!("not a run")
        };
        assert_eq!((o.scenario, o.target, o.steps.clone()), (Scenario::Games, Target::Node, vec![10, 20]));
        assert_eq!((o.tc, o.rated, o.label.as_str()), ((300, 3), false, "node"));
        assert!(matches!(o.tls, TlsMode::Ca(_)));
        assert_eq!(o.meta, vec![("a".to_string(), "b".to_string())]);
        let Command::Run(o) = parse("login").unwrap() else { panic!("not a run") };
        assert_eq!(o.concurrency, 8);
        assert!(matches!(parse("table a.json b.json").unwrap(), Command::Table(f) if f.len() == 2));
        assert!(parse("games --steps 0").is_err());
        assert!(parse("matchmaking --steps 3").is_err());
        assert!(parse("rest --endpoints info,nope").is_err());
        assert!(parse("dance").is_err());
        assert!(matches!(parse("").unwrap(), Command::Help));
    }
}
