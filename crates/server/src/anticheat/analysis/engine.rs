//! UCI engine driver (Stockfish or any UCI engine) for the post-game analysis.
//!
//! One [`UciEngine`] owns one engine process: spawned directly from `ANALYSIS_ENGINE_PATH`
//! (never through a shell), one search thread, a small hash, nice 19. Commands are strictly
//! sequential (`&mut self`: one search at a time per engine); each analysis loop of the worker
//! owns one engine. Every wait has a timeout: an engine that does not answer is killed, the caller
//! gets an [`EngineError`] it turns into a failed job, and the next call starts a fresh process.
//! A search that times out is first asked to `stop`: an engine that answers within the grace
//! period stays (the error says `recovered`), one that does not is killed.
//!
//! Scores are reported as the engine gives them: from the side to move's point of view. The
//! engine's identity is learnt when it starts: its `id name` and the evaluation network(s) it
//! reports, which are part of the analysis profile ([`super::analyzer::analysis_profile`]).
//! Where its network lives ([`UciEngine::net_memory`]) is learnt then too; it is operational
//! only (memory, never results). Stockfish 19 shares one copy of its network between the
//! processes of the same executable and user through unix sockets in `/tmp/stockfish-<uid>/`
//! (always `/tmp`: it does not read `TMPDIR`), so every engine is started with the server's own
//! environment and `/tmp`, never in a private namespace ([`SHARED_NETWORK_HINT`]).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{Instant, sleep_until, timeout};

use super::analyzer::AnalysisEngine;
use crate::log::Logger;
use crate::log_warn;

/// Output without a newline beyond this many bytes is dropped (a runaway engine).
pub const MAX_LINE_BYTES: usize = 1 << 20;
/// Default longest search before the engine is stopped (`ANALYSIS_POSITION_TIMEOUT_MS`).
pub const DEFAULT_SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
/// Default longest wait for the answers of the handshake and of `isready`.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a search asked to `stop` may take to answer before the engine is killed.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(2);
/// How long `quit` may take before the engine is killed.
pub const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// Nice value of the engine processes.
pub const ENGINE_NICE: i32 = 19;
/// What to tell an operator whose engines hold their own copy of the network.
pub const SHARED_NETWORK_HINT: &str =
    "the engines share it through /tmp/stockfish-<uid>: /tmp must be writable and the same for all of them";

/// Why an engine call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EngineErrorKind {
    /// The engine did not answer in time.
    Timeout,
    /// The engine process exited.
    Crashed,
    /// The engine could not be started.
    Spawn,
    /// The engine was closed.
    Closed,
}

impl EngineErrorKind {
    /// The former error code (`timeout`, `crashed`, `spawn`, `closed`).
    pub fn code(self) -> &'static str {
        match self {
            EngineErrorKind::Timeout => "timeout",
            EngineErrorKind::Crashed => "crashed",
            EngineErrorKind::Spawn => "spawn",
            EngineErrorKind::Closed => "closed",
        }
    }
}

/// Error raised by the engine driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineError {
    kind: EngineErrorKind,
    message: String,
    recovered: bool,
}

impl EngineError {
    /// An error of `kind` (other [`AnalysisEngine`] implementations report theirs with it).
    pub fn new(kind: EngineErrorKind, message: impl Into<String>) -> EngineError {
        EngineError { kind, message: message.into(), recovered: false }
    }

    /// The kind of failure.
    pub fn kind(&self) -> EngineErrorKind {
        self.kind
    }

    /// The former error code (`timeout`, `crashed`, `spawn`, `closed`).
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// What happened, without the code.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether the engine survived the failure (a search stopped after its timeout that answered
    /// `stop`): its partial result is refused but the process stays.
    pub fn recovered(&self) -> bool {
        self.recovered
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "engine {}: {}", self.code(), self.message)
    }
}

impl std::error::Error for EngineError {}

/// Bound of a score that is not exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bound {
    Lower,
    Upper,
}

impl Bound {
    /// The UCI token (`lowerbound`, `upperbound`).
    pub fn as_str(self) -> &'static str {
        match self {
            Bound::Lower => "lowerbound",
            Bound::Upper => "upperbound",
        }
    }
}

/// A scored UCI `info` line. `cp` and `mate` are exclusive in practice (one of them is set).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoLine {
    pub depth: i64,
    pub seldepth: i64,
    pub multipv: i64,
    pub cp: Option<i64>,
    pub mate: Option<i64>,
    pub bound: Option<Bound>,
    pub wdl: Option<[Option<i64>; 3]>,
    pub pv: Vec<String>,
}

/// One final line of a search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PvLine {
    pub multipv: i64,
    pub depth: i64,
    pub cp: Option<i64>,
    pub mate: Option<i64>,
    pub bound: Option<Bound>,
    /// First move of the line (UCI), `None` when the line has no move (mate or stalemate).
    pub mv: Option<String>,
    pub pv: Vec<String>,
}

/// What a search returned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResult {
    /// Final lines, by multipv.
    pub lines: Vec<PvLine>,
    /// `None` when the side to move has no legal move.
    pub bestmove: Option<String>,
    /// The node limit ended the search before the depth was complete (the lines are those of an
    /// unfinished search).
    pub node_limited: bool,
}

/// A search to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    pub depth: u32,
    pub multi_pv: u32,
    /// Start position (`None`: the initial position).
    pub fen: Option<String>,
    /// Node limit (`None` or 0: none).
    pub nodes: Option<u64>,
}

impl SearchOptions {
    /// A search of the given depth with one line and no node limit.
    pub fn depth(depth: u32) -> SearchOptions {
        SearchOptions { depth, multi_pv: 1, fen: None, nodes: None }
    }
}

// JavaScript's parseInt(s, 10): optional sign and leading digits.
fn parse_int(s: Option<&&str>) -> Option<i64> {
    let s = s?.trim_start();
    let (neg, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let v: i64 = rest[..digits].parse().ok()?;
    Some(if neg { -v } else { v })
}

const SKIP_ONE: [&str; 10] = [
    "seldepth",
    "nodes",
    "nps",
    "time",
    "hashfull",
    "tbhits",
    "currmovenumber",
    "cpuload",
    "currmove",
    "sbhits",
];

/// Parses a UCI `info` line. `None` for lines that carry no score (current move updates,
/// `info string ...`) and for any other line.
pub fn parse_info_line(line: &str) -> Option<InfoLine> {
    let t: Vec<&str> = line.split_whitespace().collect();
    if t.first() != Some(&"info") {
        return None;
    }
    let mut out = InfoLine {
        depth: 0,
        seldepth: 0,
        multipv: 1,
        cp: None,
        mate: None,
        bound: None,
        wdl: None,
        pv: Vec::new(),
    };
    let mut i = 1;
    while i < t.len() {
        match t[i] {
            "string" => return None,
            "depth" => {
                i += 1;
                out.depth = parse_int(t.get(i)).unwrap_or(0);
            }
            "seldepth" => {
                i += 1;
                out.seldepth = parse_int(t.get(i)).unwrap_or(0);
            }
            "multipv" => {
                i += 1;
                out.multipv = parse_int(t.get(i)).unwrap_or(1);
            }
            "score" => {
                let kind = t.get(i + 1).copied();
                let v = parse_int(t.get(i + 2))?;
                i += 2;
                match kind {
                    Some("cp") => out.cp = Some(v),
                    Some("mate") => out.mate = Some(v),
                    _ => return None,
                }
            }
            "lowerbound" => out.bound = Some(Bound::Lower),
            "upperbound" => out.bound = Some(Bound::Upper),
            "wdl" => {
                out.wdl = Some([parse_int(t.get(i + 1)), parse_int(t.get(i + 2)), parse_int(t.get(i + 3))]);
                i += 3;
            }
            "pv" => {
                out.pv = t[i + 1..].iter().map(|s| s.to_string()).collect();
                break;
            }
            "refutation" | "currline" => break,
            k if SKIP_ONE.contains(&k) => i += 1,
            _ => {} // engines add their own keys
        }
        i += 1;
    }
    (out.cp.is_some() || out.mate.is_some()).then_some(out)
}

/// Keeps, per multipv index, the most informative line of a search: the deepest one, and at the
/// same depth an exact score over a bound (a later exact line replaces an earlier one).
pub fn merge_info(acc: &mut BTreeMap<i64, InfoLine>, info: InfoLine) {
    let replace = match acc.get(&info.multipv) {
        None => true,
        Some(prev) => {
            info.depth > prev.depth
                || (info.depth == prev.depth && (info.bound.is_none() || prev.bound.is_some()))
        }
    };
    if replace {
        acc.insert(info.multipv, info);
    }
}

/// Final lines of a search, by multipv. Lines from an iteration older than the deepest one are
/// dropped (they were not searched again), except the first line, which always exists.
pub fn final_lines(acc: &BTreeMap<i64, InfoLine>) -> Vec<PvLine> {
    let Some(max_depth) = acc.values().map(|l| l.depth).max() else { return Vec::new() };
    acc.values()
        .enumerate()
        .filter(|(i, l)| *i == 0 || l.depth >= max_depth - 1)
        .map(|(_, l)| PvLine {
            multipv: l.multipv,
            depth: l.depth,
            cp: l.cp,
            mate: l.mate,
            bound: l.bound,
            mv: l.pv.first().cloned(),
            pv: l.pv.clone(),
        })
        .collect()
}

// Nodes searched so far, as an info line reports them (`\bnodes (\d+)`).
fn nodes_of(line: &str) -> Option<u64> {
    let bytes = line.as_bytes();
    for (pos, _) in line.match_indices("nodes ") {
        if pos > 0 && (bytes[pos - 1].is_ascii_alphanumeric() || bytes[pos - 1] == b'_') {
            continue;
        }
        let rest = &line[pos + 6..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            return Some(rest[..digits].parse().unwrap_or(u64::MAX));
        }
    }
    None
}

// The network named when a search starts: "info string NNUE evaluation using <file> ...".
fn net_of(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("info string NNUE evaluation using ")?;
    let name = rest.split(char::is_whitespace).next().unwrap_or("");
    (!name.is_empty()).then_some(name)
}

/// Where one replica of the network lives, as Stockfish 19 and later report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReplicaMemory {
    /// "Shared memory."
    Shared,
    /// "Local memory." (sharing failed: the process holds its own copy)
    Local,
    /// "No allocation." (a NUMA node without threads)
    NoAllocation,
    /// A status this parser does not know (a later wording), kept as the error text.
    Unknown,
}

/// One "Network replica" report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkReplica {
    pub replica: u32,
    pub memory: ReplicaMemory,
    /// The engine's explanation (the whole status when it is unknown).
    pub error: Option<String>,
}

/// Parses Stockfish's report of where a replica of its network lives:
/// `info string Network replica <n>: <Shared memory|Local memory|No allocation>. <why>`.
/// `None` for any other line.
pub fn parse_network_replica(line: &str) -> Option<NetworkReplica> {
    let rest = line.trim().strip_prefix("info string Network replica ")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let replica = rest[..digits].parse().unwrap_or(u32::MAX);
    let rest = rest[digits..].strip_prefix(": ")?;
    let known = [
        ("Shared memory.", ReplicaMemory::Shared),
        ("Local memory.", ReplicaMemory::Local),
        ("No allocation.", ReplicaMemory::NoAllocation),
    ];
    let (memory, text) = known
        .iter()
        .find_map(|(status, memory)| rest.strip_prefix(status).map(|text| (*memory, text.trim_start())))
        .unwrap_or((ReplicaMemory::Unknown, rest));
    Some(NetworkReplica { replica, memory, error: (!text.is_empty()).then(|| text.to_string()) })
}

/// Where a process keeps its network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NetworkMemory {
    /// In memory shared with the other engines.
    Shared,
    /// A copy of its own.
    Local,
}

impl NetworkMemory {
    /// The wording of the engine start log line (`shared memory`, `local memory`; `None` reads
    /// `not reported`).
    pub fn label(memory: Option<NetworkMemory>) -> &'static str {
        match memory {
            Some(NetworkMemory::Shared) => "shared memory",
            Some(NetworkMemory::Local) => "local memory",
            None => "not reported",
        }
    }
}

/// Where a process's network lives, from the replica reports of one search: shared when every
/// allocated replica is in shared memory, local when one is not (with its explanation), `None`
/// when the engine reported nothing.
pub fn network_memory(replicas: &[NetworkReplica]) -> (Option<NetworkMemory>, Option<String>) {
    if replicas.is_empty() {
        return (None, None);
    }
    if let Some(r) =
        replicas.iter().find(|r| r.memory != ReplicaMemory::Shared && r.memory != ReplicaMemory::NoAllocation)
    {
        return (Some(NetworkMemory::Local), r.error.clone());
    }
    if replicas.iter().any(|r| r.memory == ReplicaMemory::Shared) {
        return (Some(NetworkMemory::Shared), None);
    }
    (Some(NetworkMemory::Local), Some("no replica allocated".to_string()))
}

/// How to run an engine.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Engine executable (never run through a shell; no argument comes from users).
    pub path: PathBuf,
    pub args: Vec<String>,
    /// UCI `Threads` (1: reproducible analysis).
    pub threads: u32,
    /// UCI `Hash` in MiB (part of the analysis profile).
    pub hash_mb: u32,
    /// Longest search before it is stopped.
    pub timeout: Duration,
    /// Longest wait for the handshake and `isready` answers.
    pub handshake_timeout: Duration,
    /// How long a stopped search may take to answer.
    pub stop_grace: Duration,
    /// Run the engine at nice 19.
    pub low_priority: bool,
    /// Where restarts are reported.
    pub log: Option<Logger>,
}

impl EngineOptions {
    /// One thread, 16 MiB of hash, 60 s per search, 10 s for the handshake, low priority.
    pub fn new(path: impl Into<PathBuf>) -> EngineOptions {
        EngineOptions {
            path: path.into(),
            args: Vec::new(),
            threads: 1,
            hash_mb: 16,
            timeout: DEFAULT_SEARCH_TIMEOUT,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            stop_grace: DEFAULT_STOP_GRACE,
            low_priority: true,
            log: None,
        }
    }
}

// Lines of the engine's standard output. Cancel safe: a read interrupted by a dropped future
// loses nothing.
struct LineReader {
    stdout: ChildStdout,
    buf: Vec<u8>,
    scanned: usize,
    lines: VecDeque<String>,
}

impl LineReader {
    fn new(stdout: ChildStdout) -> LineReader {
        LineReader { stdout, buf: Vec::new(), scanned: 0, lines: VecDeque::new() }
    }

    // The next line without its newline (and one trailing '\r'); None at the end of the output.
    async fn next_line(&mut self) -> Option<String> {
        loop {
            if let Some(line) = self.lines.pop_front() {
                return Some(line);
            }
            self.buf.reserve(8192);
            match self.stdout.read_buf(&mut self.buf).await {
                Ok(0) | Err(_) => return None,
                Ok(_) => self.split(),
            }
        }
    }

    fn split(&mut self) {
        let mut start = 0;
        while let Some(pos) = self.buf[self.scanned..].iter().position(|&b| b == b'\n') {
            let end = self.scanned + pos;
            let line = &self.buf[start..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            self.lines.push_back(String::from_utf8_lossy(line).into_owned());
            start = end + 1;
            self.scanned = start;
        }
        self.buf.drain(..start);
        self.scanned = self.buf.len();
        if self.buf.len() > MAX_LINE_BYTES {
            self.buf.clear();
            self.scanned = 0;
        }
    }
}

struct Process {
    child: Child,
    stdin: ChildStdin,
    out: LineReader,
    // A command is in flight; still set at the next call when its future was dropped, which
    // leaves the engine in an unknown state: it is restarted.
    busy: bool,
}

fn first_word(cmd: &str) -> &str {
    cmd.split(' ').next().unwrap_or(cmd)
}

#[cfg(unix)]
fn exit_message(status: std::io::Result<std::process::ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Ok(s) => {
            let code = s.code().map_or_else(|| "null".to_string(), |c| c.to_string());
            let signal = s.signal().map_or_else(|| "null".to_string(), signal_name);
            format!("engine exited (code {code}, signal {signal})")
        }
        Err(e) => format!("engine exited ({e})"),
    }
}

#[cfg(not(unix))]
fn exit_message(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(s) => format!(
            "engine exited (code {}, signal null)",
            s.code().map_or_else(|| "null".to_string(), |c| c.to_string())
        ),
        Err(e) => format!("engine exited ({e})"),
    }
}

#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    let name = match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        24 => "SIGXCPU",
        _ => return format!("signal {sig}"),
    };
    name.to_string()
}

/// One engine process, started on first use and restarted after a crash or a timeout.
pub struct UciEngine {
    opts: EngineOptions,
    proc: Option<Process>,
    name: String,
    nets: Vec<String>,
    net_memory: Option<NetworkMemory>,
    net_memory_error: Option<String>,
    options: HashMap<&'static str, String>,
    closed: bool,
    restarts: u64,
    starts: u64,
    searches: u64,
    #[cfg(test)]
    sent: Vec<String>,
}

impl fmt::Debug for UciEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UciEngine")
            .field("path", &self.opts.path)
            .field("name", &self.name)
            .field("running", &self.proc.is_some())
            .field("starts", &self.starts)
            .field("restarts", &self.restarts)
            .finish()
    }
}

impl UciEngine {
    /// An engine that starts on first use.
    pub fn new(opts: EngineOptions) -> UciEngine {
        UciEngine {
            opts,
            proc: None,
            name: String::new(),
            nets: Vec::new(),
            net_memory: None,
            net_memory_error: None,
            options: HashMap::new(),
            closed: false,
            restarts: 0,
            starts: 0,
            searches: 0,
            #[cfg(test)]
            sent: Vec::new(),
        }
    }

    /// The engine's `id name` (`unknown engine` when it gave none; empty before it started).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The evaluation networks the running engine reported, in order.
    pub fn nets(&self) -> &[String] {
        &self.nets
    }

    /// The networks `+`-joined, `None` before the start or when the engine reports none.
    pub fn net(&self) -> Option<String> {
        (!self.nets.is_empty()).then(|| self.nets.join("+"))
    }

    /// Where the network lives (`None`: not reported).
    pub fn net_memory(&self) -> Option<NetworkMemory> {
        self.net_memory
    }

    /// Why the network is not shared, as the engine says.
    pub fn net_memory_error(&self) -> Option<&str> {
        self.net_memory_error.as_deref()
    }

    /// The hash size the engine runs with.
    pub fn hash_mb(&self) -> u32 {
        self.opts.hash_mb
    }

    /// Processes killed after a timeout or a crash during a search.
    pub fn restarts(&self) -> u64 {
        self.restarts
    }

    /// Processes started and past the handshake.
    pub fn starts(&self) -> u64 {
        self.starts
    }

    /// Searches sent.
    pub fn searches(&self) -> u64 {
        self.searches
    }

    /// Process id of the running engine.
    pub fn pid(&self) -> Option<u32> {
        self.proc.as_ref().and_then(|p| p.child.id())
    }

    /// Whether a process runs and is usable.
    pub fn is_alive(&mut self) -> bool {
        match &mut self.proc {
            Some(p) => !p.busy && matches!(p.child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Starts the process if needed, completes the UCI handshake and learns the engine's name and
    /// network. A process that died or was left in the middle of a command is replaced.
    pub async fn start(&mut self) -> Result<(), EngineError> {
        if self.closed {
            return Err(EngineError::new(EngineErrorKind::Closed, "engine closed"));
        }
        if self.is_alive() {
            return Ok(());
        }
        self.kill().await;
        self.spawn()?;
        if let Err(e) = self.handshake().await {
            self.kill().await;
            return Err(e);
        }
        self.starts += 1;
        Ok(())
    }

    fn spawn(&mut self) -> Result<(), EngineError> {
        if self.opts.path.as_os_str().is_empty() {
            return Err(EngineError::new(EngineErrorKind::Spawn, "engine path is empty"));
        }
        let mut child = Command::new(&self.opts.path)
            .args(&self.opts.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| EngineError::new(EngineErrorKind::Spawn, format!("cannot start engine: {e}")))?;
        if self.opts.low_priority
            && let Some(pid) = child.id()
        {
            // Not permitted (or not supported): the engine runs at the server's priority.
            let _ = crate::sys::set_process_nice(pid, ENGINE_NICE);
        }
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        self.proc = Some(Process { child, stdin, out: LineReader::new(stdout), busy: false });
        // A restarted engine may be another build: it reports anew.
        self.options.clear();
        self.nets.clear();
        self.net_memory = None;
        self.net_memory_error = None;
        Ok(())
    }

    async fn handshake(&mut self) -> Result<(), EngineError> {
        let wait = self.opts.handshake_timeout;
        let mut name = String::new();
        self.command("uci", wait, false, |line, _| {
            if let Some(rest) = line.strip_prefix("id name ") {
                name = rest.trim().to_string();
            }
            Ok((line == "uciok").then_some(()))
        })
        .await?;
        self.name = if name.is_empty() { "unknown engine".to_string() } else { name };
        self.set_option("Threads", self.opts.threads.to_string()).await?;
        self.set_option("Hash", self.opts.hash_mb.to_string()).await?;
        self.ready().await?;
        // The network, and where it lives, are only reported when a search starts: a one-ply
        // search of the initial position reports them before any analysis, so every record
        // names its full profile.
        self.send("position startpos").await?;
        let (mut nets, mut replicas) = (Vec::<String>::new(), Vec::new());
        self.command("go depth 1", wait, false, |line, _| {
            if line.starts_with("info string ") {
                if let Some(net) = net_of(line)
                    && !nets.iter().any(|n| n == net)
                {
                    nets.push(net.to_string());
                }
                replicas.extend(parse_network_replica(line));
                return Ok(None);
            }
            Ok(line.starts_with("bestmove").then_some(()))
        })
        .await?;
        self.nets = nets;
        (self.net_memory, self.net_memory_error) = network_memory(&replicas);
        Ok(())
    }

    // Writes one command line. A write error means the engine exited: the next read reports it.
    // The engine is busy until the answer of the last command of the call (every public call ends
    // with one): a call dropped before leaves it in an unknown state, and it is restarted.
    async fn send(&mut self, cmd: &str) -> Result<(), EngineError> {
        #[cfg(test)]
        self.sent.push(cmd.to_string());
        let wait = self.opts.handshake_timeout;
        let Some(p) = self.proc.as_mut() else {
            return Err(EngineError::new(EngineErrorKind::Crashed, "engine is not running"));
        };
        p.busy = true;
        let line = format!("{cmd}\n");
        let write = async {
            p.stdin.write_all(line.as_bytes()).await?;
            p.stdin.flush().await
        };
        if timeout(wait, write).await.is_err() {
            // The engine stopped reading its input.
            self.kill().await;
            return Err(EngineError::new(
                EngineErrorKind::Timeout,
                format!("engine did not read \"{}\"", first_word(cmd)),
            ));
        }
        Ok(())
    }

    async fn set_option(&mut self, name: &'static str, value: String) -> Result<(), EngineError> {
        if self.options.get(name) == Some(&value) {
            return Ok(());
        }
        self.send(&format!("setoption name {name} value {value}")).await?;
        self.options.insert(name, value);
        Ok(())
    }

    // Sends `cmd` and feeds every output line to `handler(line, timed_out)` until it returns a
    // value or an error. On timeout, a search is asked to `stop` (the handler then sees
    // `timed_out`) and given the stop grace; anything else, or a search still silent after the
    // grace, kills the process.
    async fn command<T>(
        &mut self,
        cmd: &str,
        wait: Duration,
        stop_on_timeout: bool,
        mut handler: impl FnMut(&str, bool) -> Result<Option<T>, EngineError>,
    ) -> Result<T, EngineError> {
        self.send(cmd).await?;
        let grace = self.opts.stop_grace;
        let p = self.proc.as_mut().expect("send succeeded: the process runs");
        let mut deadline = Instant::now() + wait;
        let mut timed_out = false;
        loop {
            let line = tokio::select! {
                line = p.out.next_line() => Some(line),
                () = sleep_until(deadline) => None,
            };
            match line {
                Some(Some(line)) => match handler(&line, timed_out) {
                    Ok(None) => {}
                    done => {
                        p.busy = false;
                        return done.map(|v| v.expect("a value or an error ends the command"));
                    }
                },
                Some(None) => {
                    let status = timeout(CLOSE_GRACE, p.child.wait()).await.unwrap_or_else(|_| {
                        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no exit status"))
                    });
                    self.proc = None;
                    return Err(EngineError::new(EngineErrorKind::Crashed, exit_message(status)));
                }
                None if stop_on_timeout && !timed_out => {
                    timed_out = true;
                    deadline = Instant::now() + grace;
                    #[cfg(test)]
                    self.sent.push("stop".to_string());
                    // A failed write: the engine is gone, the read reports it.
                    let _ = p.stdin.write_all(b"stop\n").await;
                    let _ = p.stdin.flush().await;
                }
                None => {
                    self.kill().await;
                    return Err(EngineError::new(
                        EngineErrorKind::Timeout,
                        format!("engine did not answer \"{}\"", first_word(cmd)),
                    ));
                }
            }
        }
    }

    async fn kill(&mut self) {
        if let Some(mut p) = self.proc.take() {
            let _ = p.child.start_kill();
            let _ = timeout(CLOSE_GRACE, p.child.wait()).await;
        }
    }

    /// Waits for `readyok`.
    pub async fn ready(&mut self) -> Result<(), EngineError> {
        let wait = self.opts.handshake_timeout;
        self.command("isready", wait, false, |line, _| Ok((line == "readyok").then_some(()))).await
    }

    /// `ucinewgame` and `isready` (clears the engine's game state).
    pub async fn new_game(&mut self) -> Result<(), EngineError> {
        self.start().await?;
        self.send("ucinewgame").await?;
        self.ready().await
    }

    /// Empties the transposition table, so a search does not profit from earlier (deeper) ones.
    pub async fn clear_hash(&mut self) -> Result<(), EngineError> {
        self.start().await?;
        self.send("setoption name Clear Hash").await?;
        self.ready().await
    }

    /// Searches a position (UCI `moves` from the initial position or from `opts.fen`) to a fixed
    /// depth, within `opts.nodes` nodes when given. After a timeout that the engine did not
    /// recover from, or a crash, the process is killed and the next call starts a new one.
    pub async fn analyse(
        &mut self,
        moves: &[String],
        opts: &SearchOptions,
    ) -> Result<SearchResult, EngineError> {
        self.start().await?;
        match self.search(moves, opts).await {
            Err(e)
                if matches!(e.kind, EngineErrorKind::Timeout | EngineErrorKind::Crashed) && !e.recovered =>
            {
                self.restarts += 1;
                if let Some(log) = &self.opts.log {
                    log_warn!(log, "engine killed, restarts on next use", { "reason": e.code(), "restarts": self.restarts });
                }
                self.kill().await;
                Err(e)
            }
            other => other,
        }
    }

    async fn search(&mut self, moves: &[String], opts: &SearchOptions) -> Result<SearchResult, EngineError> {
        self.set_option("MultiPV", opts.multi_pv.to_string()).await?;
        let mut pos = match &opts.fen {
            Some(fen) => format!("position fen {fen}"),
            None => "position startpos".to_string(),
        };
        if !moves.is_empty() {
            pos.push_str(" moves ");
            pos.push_str(&moves.join(" "));
        }
        self.send(&pos).await?;
        let limit = opts.nodes.unwrap_or(0);
        let depth = opts.depth;
        let go =
            if limit > 0 { format!("go depth {depth} nodes {limit}") } else { format!("go depth {depth}") };
        self.searches += 1;
        let timeout_ms = self.opts.timeout.as_millis();
        let (mut acc, mut reached, mut searched) = (BTreeMap::new(), 0i64, 0u64);
        let wait = self.opts.timeout;
        self.command(&go, wait, true, |line, timed_out| {
            if line.starts_with("info ") {
                if let Some(info) = parse_info_line(line) {
                    reached = reached.max(info.depth);
                    merge_info(&mut acc, info);
                }
                if let Some(n) = nodes_of(line) {
                    searched = searched.max(n);
                }
                return Ok(None);
            }
            if !line.starts_with("bestmove") {
                return Ok(None);
            }
            if timed_out {
                // Stopped early: the partial result is not the fixed-depth answer; refuse it but
                // keep the (responsive) engine.
                let mut e = EngineError::new(
                    EngineErrorKind::Timeout,
                    format!("search of depth {depth} exceeded {timeout_ms} ms"),
                );
                e.recovered = true;
                return Err(e);
            }
            let best =
                line.split_whitespace().nth(1).filter(|m| *m != "(none)" && *m != "0000").map(str::to_string);
            // Stopped by the limit: the last lines report the limit reached, or the last complete
            // iteration is shallower than the depth asked for.
            let node_limited =
                limit > 0 && best.is_some() && (searched >= limit || reached < i64::from(depth));
            Ok(Some(SearchResult { lines: final_lines(&acc), bestmove: best, node_limited }))
        })
        .await
    }

    /// Asks the engine to quit, then kills it if it lingers. A closed engine refuses every call.
    pub async fn close(&mut self) {
        self.closed = true;
        let Some(mut p) = self.proc.take() else { return };
        let quit = async {
            p.stdin.write_all(b"quit\n").await?;
            p.stdin.shutdown().await
        };
        let asked = timeout(CLOSE_GRACE, quit).await.is_ok_and(|r| r.is_ok());
        if !asked || timeout(CLOSE_GRACE, p.child.wait()).await.is_err() {
            let _ = p.child.start_kill();
            let _ = timeout(CLOSE_GRACE, p.child.wait()).await;
        }
    }
}

impl AnalysisEngine for UciEngine {
    fn name(&self) -> &str {
        UciEngine::name(self)
    }

    fn net(&self) -> Option<String> {
        UciEngine::net(self)
    }

    fn hash_mb(&self) -> Option<u32> {
        Some(self.opts.hash_mb)
    }

    fn new_game(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
        UciEngine::new_game(self)
    }

    fn clear_hash(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
        UciEngine::clear_hash(self)
    }

    fn analyse(
        &mut self,
        moves: &[String],
        opts: &SearchOptions,
    ) -> impl Future<Output = Result<SearchResult, EngineError>> + Send {
        UciEngine::analyse(self, moves, opts)
    }
}

#[cfg(test)]
mod tests;
