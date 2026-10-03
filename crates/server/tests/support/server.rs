//! The server of the black-box tests: the `scacelith-server` binary of this package (`start`) in a
//! child process, with a temporary data directory, a throw-away self-signed certificate
//! (`TLS_MODE=native`), the API and the WebSocket on one port of the system's choice, a metrics
//! port, and its JSON log lines collected for the assertions.
//!
//! ```ignore
//! let srv = TestServer::options().workers(2).env("FIRST_MOVE_TIMEOUT_MS", "20000").start().await;
//! let alice = players::player(&srv, "alice").await;   // registered, signed in, connected
//! ...
//! srv.stop().await;
//! ```
//!
//! The defaults are those of the former Node.js harness: e-mails written to the log, no proof of
//! work, the per-address limits of the auth family raised (every test client comes from
//! 127.0.0.1), the loopback in `ABUSE_EXEMPT`, no engine analysis. `KEEP_TEST_SERVER=1` keeps the
//! data directories.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use scacelith_client::http::{HttpConnection, Request};
use scacelith_client::{ApiClient, Endpoint, TlsConfig};
use scacelith_server::journal::RecordKind;
use scacelith_server::journal::format::parse_segment;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// The server binary built with these tests.
pub const SERVER_BIN: &str = env!("CARGO_BIN_EXE_scacelith-server");

/// The longest start-up (migrations, journal replay, listeners) on a loaded machine.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a graceful stop may take before the server is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// A temporary directory, removed when the last handle is dropped (unless `KEEP_TEST_SERVER` is
/// set).
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A new empty directory named after `tag`.
    pub fn new(tag: &str) -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("scacelith-it-{tag}-{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("a temporary directory");
        TempDir { path }
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path in the directory.
    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("KEEP_TEST_SERVER").is_none() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// A PEM block of `der`.
fn pem(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// The certificate of the data directory `dir` (made the first time: a restart keeps it, so the
/// clients keep trusting the server): `(certificate file, key file, certificate PEM)`.
fn certificate(dir: &TempDir) -> (PathBuf, PathBuf, Vec<u8>) {
    let (cert, key) = (dir.file("cert.pem"), dir.file("key.pem"));
    if !cert.exists() {
        let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("a test certificate");
        std::fs::write(&cert, pem("CERTIFICATE", made.cert.der())).expect("certificate written");
        std::fs::write(&key, pem("PRIVATE KEY", &made.signing_key.serialize_der())).expect("key written");
    }
    let pem = std::fs::read(&cert).expect("certificate read");
    (cert, key, pem)
}

/// A TCP port free right now on 127.0.0.1 (the metrics port: 0 would turn it off).
pub fn free_port() -> u16 {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a free port");
    l.local_addr().expect("its address").port()
}

/// The log lines of a server, as JSON values (a line that is not JSON is `{"raw": line}`).
#[derive(Clone)]
pub struct Logs {
    lines: Arc<Mutex<Vec<Value>>>,
    count: watch::Sender<usize>,
}

impl Default for Logs {
    fn default() -> Logs {
        Logs { lines: Arc::new(Mutex::new(Vec::new())), count: watch::channel(0).0 }
    }
}

impl Logs {
    fn push(&self, line: &str) {
        let rec = serde_json::from_str::<Value>(line)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({ "raw": line }));
        let n = {
            let mut lines = self.lines.lock().expect("log lines");
            lines.push(rec);
            lines.len()
        };
        self.count.send_replace(n);
    }

    /// Reads the lines of `stream` until it ends.
    pub fn follow(&self, stream: impl AsyncRead + Unpin + Send + 'static) {
        let logs = self.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                logs.push(&line);
            }
        });
    }

    /// Every line so far.
    pub fn all(&self) -> Vec<Value> {
        self.lines.lock().expect("log lines").clone()
    }

    /// The lines so far that match `pred`.
    pub fn matching(&self, pred: impl Fn(&Value) -> bool) -> Vec<Value> {
        self.lines.lock().expect("log lines").iter().filter(|l| pred(l)).cloned().collect()
    }

    /// The first line matching `pred`, already logged or logged within `limit`.
    pub async fn wait(&self, limit: Duration, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        let mut seen = self.count.subscribe();
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if let Some(found) = self.lines.lock().expect("log lines").iter().find(|l| pred(l)) {
                return Some(found.clone());
            }
            match tokio::time::timeout_at(deadline, seen.changed()).await {
                Ok(Ok(())) => {}
                // The deadline passed, or the server is gone and nothing more will come.
                _ => return self.lines.lock().expect("log lines").iter().find(|l| pred(l)).cloned(),
            }
        }
    }

    /// Waits until a new line is logged, for at most `limit`.
    pub async fn changed(&self, limit: Duration) {
        let mut seen = self.count.subscribe();
        let _ = tokio::time::timeout(limit, seen.changed()).await;
    }

    /// The last `n` lines, one per line of text (failure messages).
    pub fn tail(&self, n: usize) -> String {
        let lines = self.lines.lock().expect("log lines");
        let from = lines.len().saturating_sub(n);
        lines[from..].iter().map(Value::to_string).collect::<Vec<_>>().join("\n")
    }
}

/// How to start a test server.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    workers: u32,
    env: Vec<(String, String)>,
    dir: Option<Arc<TempDir>>,
}

impl ServerOptions {
    /// `WORKERS` (game shards and runtime threads).
    pub fn workers(mut self, n: u32) -> ServerOptions {
        self.workers = n;
        self
    }

    /// One setting (replaces a default of the harness).
    pub fn env(mut self, key: &str, value: &str) -> ServerOptions {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    /// Starts on the data directory of an earlier server (a restart, a recovery).
    pub fn dir(mut self, dir: Arc<TempDir>) -> ServerOptions {
        self.dir = Some(dir);
        self
    }

    /// Starts the server and waits until it is ready.
    pub async fn start(self) -> TestServer {
        TestServer::launch(self).await
    }
}

/// A running test server.
pub struct TestServer {
    /// The data directory (database, journal, certificate).
    pub dir: Arc<TempDir>,
    /// The API and WebSocket port, on 127.0.0.1.
    pub addr: SocketAddr,
    /// The metrics port (plain HTTP), on 127.0.0.1.
    pub metrics_addr: SocketAddr,
    /// The certificate of the server (the trust anchor of its clients).
    pub cert_pem: Vec<u8>,
    /// The environment of the server process (also the admin CLI's).
    pub env: Vec<(String, String)>,
    /// What the server logged.
    pub logs: Logs,
    child: Child,
    exit: Option<ExitStatus>,
}

impl std::fmt::Debug for TestServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestServer").field("addr", &self.addr).field("dir", &self.dir.path()).finish()
    }
}

/// The settings of every test server before its options.
fn base_env(dir: &TempDir, workers: u32, metrics_port: u16) -> Vec<(String, String)> {
    let (cert, key, _) = certificate(dir);
    let secret = base64::engine::general_purpose::STANDARD.encode([42u8; 48]);
    let pairs: Vec<(&str, String)> = vec![
        ("SERVER_NAME", "Scacelith Test Server".into()),
        ("SERVER_PUBLIC_HOST", "localhost".into()),
        ("SERVER_SECRET", secret),
        ("BIND_ADDRESS", "127.0.0.1".into()),
        ("API_PORT", "0".into()),
        ("METRICS_PORT", metrics_port.to_string()),
        ("METRICS_BIND", "127.0.0.1".into()),
        ("TLS_MODE", "native".into()),
        ("TLS_CERT_FILE", cert.display().to_string()),
        ("TLS_KEY_FILE", key.display().to_string()),
        ("DATA_DIR", dir.path().display().to_string()),
        ("WORKERS", workers.to_string()),
        ("MAIL_TRANSPORT", "log".into()),
        ("REQUIRE_EMAIL_VERIFICATION", "false".into()),
        ("POW_REGISTER_BITS", "0".into()),
        ("POW_LOGIN_BITS", "0".into()),
        ("AUTH_RATE_PER_IP", "100000".into()),
        ("HTTP_RATE_PER_IP", "100000".into()),
        // Every test client comes from 127.0.0.1: the per-address and per-account limits of the
        // auth family and the account budget are raised like AUTH_RATE_PER_IP.
        ("AUTH_REGISTER_PER_HOUR", "100000".into()),
        ("AUTH_MAIL_PER_HOUR", "100000".into()),
        ("AUTH_FORGOT_PER_HOUR", "100000".into()),
        ("AUTH_FORGOT_PER_DAY", "100000".into()),
        ("AUTH_RESET_PER_HOUR", "100000".into()),
        ("AUTH_MFA_PER_ACCOUNT", "100000".into()),
        ("AUTH_REAUTH_PER_USER", "100000".into()),
        ("USER_RATE_PER_MIN", "100000".into()),
        ("MAX_CONNECTIONS_PER_IP", "100000".into()),
        // Every test client is on the loopback: outside the protection per address. The abuse
        // tests narrow it to 127.0.0.1 and flood from 127.0.0.2.
        ("ABUSE_EXEMPT", "127.0.0.0/8,::1".into()),
        // The TLS gate's per-group handshake cap is not part of that protection: it applies to
        // ABUSE_EXEMPT addresses too. A test that opens many connections at once from one address
        // would see some of them reset before TLS: one below MAX_PENDING_HANDSHAKES (128 per
        // worker by default), the highest value the configuration accepts.
        ("MAX_PENDING_HANDSHAKES_PER_IP", (128 * workers - 1).to_string()),
        // The password hashes of the accounts a test makes together run side by side.
        ("PASSWORD_HASH_CONCURRENCY", "4".into()),
        ("ANALYSIS_WORKERS", "0".into()),
        ("LOG_LEVEL", "info".into()),
        ("LOG_FORMAT", "json".into()),
        ("SCACELITH_ENV_FILE", String::new()),
    ];
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

impl TestServer {
    /// The default options: one worker, the harness's settings.
    pub fn options() -> ServerOptions {
        ServerOptions { workers: 1, env: Vec::new(), dir: None }
    }

    /// A server with the default options.
    pub async fn start() -> TestServer {
        Self::options().start().await
    }

    async fn launch(opts: ServerOptions) -> TestServer {
        let dir = opts.dir.clone().unwrap_or_else(|| Arc::new(TempDir::new("srv")));
        // The metrics port is taken free a moment before the server binds it: another process
        // may take it meanwhile, in which case the start is tried again on another one.
        let mut attempt = 0;
        loop {
            attempt += 1;
            match Self::try_launch(&opts, dir.clone()).await {
                Ok(server) => return server,
                Err(failure) if attempt < 3 && failure.contains("address already in use") => continue,
                Err(failure) => panic!("the test server did not start: {failure}"),
            }
        }
    }

    async fn try_launch(opts: &ServerOptions, dir: Arc<TempDir>) -> Result<TestServer, String> {
        let metrics_port = free_port();
        let mut env = base_env(&dir, opts.workers, metrics_port);
        for (k, v) in &opts.env {
            env.retain(|(key, _)| key != k);
            env.push((k.clone(), v.clone()));
        }
        let (_, _, cert_pem) = certificate(&dir);
        let mut child = Command::new(SERVER_BIN)
            .arg("start")
            .env_clear()
            .envs(passthrough_env())
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot run {SERVER_BIN}: {e}"))?;
        let logs = Logs::default();
        logs.follow(child.stdout.take().expect("piped stdout"));
        logs.follow(child.stderr.take().expect("piped stderr"));

        let deadline = Instant::now() + START_TIMEOUT;
        let started = loop {
            let found = logs.wait(Duration::from_millis(100), |l| l["msg"] == "started").await;
            if let Some(line) = found {
                break line;
            }
            if let Ok(Some(status)) = child.try_wait() {
                // The last lines may still be in the pipes.
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Err(format!("exited during the start-up ({status}):\n{}", logs.tail(30)));
            }
            if Instant::now() > deadline {
                let _ = child.start_kill();
                return Err(format!("not started within {START_TIMEOUT:?}:\n{}", logs.tail(30)));
            }
        };
        let address = |kind: &str| -> Option<SocketAddr> {
            started["listeners"]
                .as_array()?
                .iter()
                .find(|l| l["kind"] == kind)
                .and_then(|l| l["address"].as_str()?.parse().ok())
        };
        let addr = address("api+ws").ok_or_else(|| format!("no api+ws listener in {started}"))?;
        let metrics_addr = address("metrics").ok_or_else(|| format!("no metrics listener in {started}"))?;
        if logs.wait(START_TIMEOUT, |l| l["msg"] == "ready").await.is_none() {
            return Err(format!("not ready:\n{}", logs.tail(30)));
        }
        Ok(TestServer { dir, addr, metrics_addr, cert_pem, env, logs, child, exit: None })
    }

    /// The server's address for the client SDK: TLS, the certificate's name, its certificate.
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::tls(self.addr, "localhost", self.tls())
    }

    /// The same endpoint, its connections opened from the local address `ip` (127.0.0.2...).
    pub fn endpoint_from(&self, ip: IpAddr) -> Endpoint {
        self.endpoint().with_local_addr(ip)
    }

    /// The client TLS settings that trust this server's certificate.
    pub fn tls(&self) -> TlsConfig {
        TlsConfig::with_root_pem(&self.cert_pem).expect("the test certificate")
    }

    /// An API client of this server.
    pub fn api(&self) -> ApiClient {
        ApiClient::new(self.endpoint())
    }

    /// The text of `GET /metrics` on the metrics port.
    pub async fn metrics(&self) -> String {
        let mut conn = HttpConnection::open(&Endpoint::plain(self.metrics_addr)).await.expect("metrics port");
        let res = conn.send(&Request::get("/metrics")).await.expect("metrics answer");
        assert_eq!(res.status, 200, "GET /metrics");
        String::from_utf8(res.body.to_vec()).expect("UTF-8 metrics")
    }

    /// The value of one sample of `/metrics` (its name with its labels, as printed), 0 when
    /// absent.
    pub async fn metric(&self, sample: &str) -> f64 {
        metric_value(&self.metrics().await, sample)
    }

    /// Sends `signal` (`TERM`, `INT`, `HUP`) to the server process.
    pub fn signal(&self, signal: &str) {
        let pid = self.child.id().expect("a running server");
        let status = std::process::Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(pid.to_string())
            .status()
            .expect("kill runs");
        assert!(status.success(), "kill -{signal} {pid}");
    }

    /// The graceful stop (SIGTERM), killed after 30 s; returns the exit status.
    pub async fn stop(&mut self) -> ExitStatus {
        if let Some(status) = self.exit {
            return status;
        }
        if self.child.id().is_some() {
            self.signal("TERM");
        }
        let status = match tokio::time::timeout(STOP_TIMEOUT, self.child.wait()).await {
            Ok(status) => status.expect("the server's exit status"),
            Err(_) => {
                let _ = self.child.start_kill();
                self.child.wait().await.expect("the killed server's exit status")
            }
        };
        self.exit = Some(status);
        status
    }

    /// A hard crash (SIGKILL), the data directory kept for a restart.
    pub async fn crash(&mut self) {
        let _ = self.child.start_kill();
        self.exit = Some(self.child.wait().await.expect("the killed server's exit status"));
    }

    /// Whether the process has exited.
    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.exit.or_else(|| self.child.try_wait().ok().flatten())
    }

    /// Runs `scacelith-server admin <args>` on this server's data directory (another process, as
    /// on a server host) with `extra` settings; returns its standard output, or its error output.
    pub async fn admin(&self, args: &[&str], extra: &[(&str, &str)]) -> Result<String, String> {
        let out = Command::new(SERVER_BIN)
            .arg("admin")
            .args(args)
            .env_clear()
            .envs(passthrough_env())
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .envs(extra.iter().copied())
            .current_dir(self.dir.path())
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("cannot run the admin command: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            Ok(stdout)
        } else {
            Err(format!("{}\n{stdout}\n{}", out.status, String::from_utf8_lossy(&out.stderr)))
        }
    }

    /// The mails the log transport wrote to `to`, oldest first.
    pub fn mails_to(&self, to: &str) -> Vec<Value> {
        self.logs.matching(|l| l["msg"] == "mail (log transport)" && l["to"] == to)
    }

    /// The latest mail to `to` whose subject contains `subject`, once more than `after` of them
    /// were written (within 10 s).
    pub async fn mail_to(&self, to: &str, subject: &str, after: usize) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let all: Vec<Value> = self
                .mails_to(to)
                .into_iter()
                .filter(|m| m["subject"].as_str().is_some_and(|s| s.contains(subject)))
                .collect();
            if all.len() > after {
                return all.last().cloned().expect("a mail");
            }
            assert!(Instant::now() < deadline, "no mail to {to} with \"{subject}\":\n{}", self.logs.tail(20));
            self.logs.changed(Duration::from_millis(200)).await;
        }
    }

    /// The move records of game `game` that are on disk in the journal (a crash after they are
    /// keeps them).
    pub fn journaled_moves(&self, game: u64) -> usize {
        let mut count = 0;
        let Ok(shards) = std::fs::read_dir(self.dir.file("journal")) else { return 0 };
        for shard in shards.flatten() {
            let Ok(segments) = std::fs::read_dir(shard.path()) else { continue };
            for segment in segments.flatten() {
                let Ok(bytes) = std::fs::read(segment.path()) else { continue };
                parse_segment(&bytes, |r| {
                    if r.kind == RecordKind::Move && r.game == game {
                        count += 1;
                    }
                });
            }
        }
        count
    }

    /// A connection to the database of this server (the server keeps running: WAL, busy wait).
    pub fn db(&self) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.dir.file("scacelith.db")).expect("the database");
        db.busy_timeout(Duration::from_secs(5)).expect("busy timeout");
        db
    }
}

/// The value of one sample in a Prometheus text (its name with its labels, as printed), 0 when
/// absent.
pub fn metric_value(text: &str, sample: &str) -> f64 {
    text.lines().find_map(|l| l.strip_prefix(sample)?.strip_prefix(' ')?.trim().parse().ok()).unwrap_or(0.0)
}

/// What the child processes keep of the test's environment.
fn passthrough_env() -> Vec<(String, String)> {
    ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Polls `check` every 20 ms until it returns `Some`, for at most `limit`; panics with `what`
/// otherwise (the state the test waits for is not a message nor a log line: a metric, a row).
pub async fn eventually<T, F, Fut>(limit: Duration, what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + limit;
    loop {
        if let Some(v) = check().await {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
