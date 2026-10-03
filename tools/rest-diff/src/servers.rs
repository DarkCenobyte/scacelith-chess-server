//! Starts and stops the two servers with equivalent settings: the same TLS certificate, secret,
//! names, public ports and limits, each on its own ports and data directory, with the `log`
//! mail transport so that the links of the e-mails can be read from their logs.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::http::{Client, Req, Target};

/// Which implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The former Node.js server (reference).
    Node,
    /// The Rust server.
    Rust,
}

impl Kind {
    /// Short name for reports.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Node => "node",
            Kind::Rust => "rust",
        }
    }
}

/// Where the programs are.
#[derive(Clone, Debug)]
pub struct Programs {
    /// The `node` executable.
    pub node: PathBuf,
    /// The Node server's tree (`bin/scacelith-server.js`, `bin/admin.js`).
    pub node_dir: PathBuf,
    /// The `scacelith-server` binary.
    pub rust: PathBuf,
}

/// The shared test certificate.
#[derive(Clone, Debug)]
pub struct Cert {
    /// PEM certificate file.
    pub cert: PathBuf,
    /// PEM key file.
    pub key: PathBuf,
    /// The certificate PEM (client trust anchor).
    pub pem: Vec<u8>,
}

impl Cert {
    /// A self-signed certificate for `localhost` and 127.0.0.1, written into `dir`.
    pub fn create(dir: &Path) -> Result<Cert, String> {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(|e| format!("certificate: {e}"))?;
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let pem = pem_block("CERTIFICATE", ck.cert.der());
        std::fs::write(&cert, &pem).map_err(|e| format!("{}: {e}", cert.display()))?;
        let key_pem = pem_block("PRIVATE KEY", &ck.signing_key.serialize_der());
        std::fs::write(&key, key_pem).map_err(|e| format!("{}: {e}", key.display()))?;
        Ok(Cert { cert, key, pem: pem.into_bytes() })
    }
}

/// A PEM block (RFC 7468) of `der`.
fn pem_block(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// One mail read from a server's log.
#[derive(Clone, Debug)]
pub struct Mail {
    /// Recipient.
    pub to: String,
    /// Subject.
    pub subject: String,
    /// Plain-text body.
    pub text: String,
}

/// A running server.
pub struct Server {
    /// Which implementation.
    pub kind: Kind,
    /// HTTPS (or plain, in proxy mode) API port.
    pub api_port: u16,
    /// Data directory.
    pub dir: PathBuf,
    /// How to reach the API.
    pub target: Target,
    /// The settings it runs with.
    pub env: Vec<(String, String)>,
    programs: Programs,
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

/// A fixed `SERVER_SECRET` (48 bytes), the same for both servers.
const SECRET: &str = "c2NhY2VsaXRoLXJlc3QtZGlmZi1zZWNyZXQtb2YtdGhlLXRlc3Qtc2VydmVycy0xMjM0";

fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("free port: {e}"))?;
    Ok(l.local_addr().map_err(|e| e.to_string())?.port())
}

/// The settings common to both servers in every profile (the profile adds or overrides).
fn base_env(dir: &Path, cert: &Cert, api_port: u16, metrics_port: u16) -> Vec<(String, String)> {
    let s = |k: &str, v: &str| (k.to_string(), v.to_string());
    vec![
        s("SERVER_NAME", "Scacelith Diff"),
        s("SERVER_PUBLIC_HOST", "localhost"),
        // Identical public ports: the links of the mails and /info do not depend on the real port.
        s("PUBLIC_API_PORT", "8443"),
        s("PUBLIC_WS_PORT", "8443"),
        s("SERVER_SECRET", SECRET),
        s("BIND_ADDRESS", "127.0.0.1"),
        s("API_PORT", &api_port.to_string()),
        s("METRICS_PORT", &metrics_port.to_string()),
        s("METRICS_BIND", "127.0.0.1"),
        s("TLS_MODE", "native"),
        s("TLS_CERT_FILE", &cert.cert.display().to_string()),
        s("TLS_KEY_FILE", &cert.key.display().to_string()),
        s("DATA_DIR", &dir.display().to_string()),
        s("WORKERS", "1"),
        s("MAIL_TRANSPORT", "log"),
        s("LOG_LEVEL", "info"),
        s("LOG_FORMAT", "json"),
        s("SCACELITH_ENV_FILE", ""),
    ]
}

impl Server {
    /// Starts a server of `kind` in `dir` with the base settings and `overrides`; returns when
    /// `GET /api/v1/info` answers.
    pub async fn start(
        kind: Kind,
        programs: &Programs,
        cert: &Cert,
        dir: &Path,
        overrides: &[(String, String)],
    ) -> Result<Server, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let api_port = free_port()?;
        let metrics_port = free_port()?;
        let mut env = base_env(dir, cert, api_port, metrics_port);
        for (k, v) in overrides {
            env.retain(|(ek, _)| ek != k);
            env.push((k.clone(), v.clone()));
        }
        let plain = env.iter().any(|(k, v)| k == "TLS_MODE" && v != "native");
        let tls = if plain {
            None
        } else {
            Some(
                scacelith_client::TlsConfig::with_root_pem(&cert.pem)
                    .map_err(|e| format!("client TLS: {e}"))?
                    .rustls()
                    .clone(),
            )
        };
        let target = Target {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), api_port),
            tls,
            host: format!("localhost:{api_port}"),
        };
        if kind == Kind::Rust {
            let out = command(kind, programs, dir, &env, &["migrate"])
                .output()
                .await
                .map_err(|e| format!("scacelith-server migrate: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "scacelith-server migrate failed: {}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
        }
        let mut child = command(kind, programs, dir, &env, &["start"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("{} start: {e}", kind.name()))?;
        let lines = Arc::new(Mutex::new(Vec::new()));
        if let Some(o) = child.stdout.take() {
            spawn_reader(o, lines.clone());
        }
        if let Some(e) = child.stderr.take() {
            spawn_reader(e, lines.clone());
        }
        let mut server = Server {
            kind,
            api_port,
            dir: dir.to_path_buf(),
            target,
            env,
            programs: programs.clone(),
            child,
            lines,
        };
        server.wait_ready().await?;
        Ok(server)
    }

    async fn wait_ready(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut client = Client::new(self.target.clone(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!(
                    "{} exited during start-up ({status}):\n{}",
                    self.kind.name(),
                    self.tail(30)
                ));
            }
            let r = client.send(&Req::get("/api/v1/info").fresh()).await;
            if r.status == 200 {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "{} did not answer /api/v1/info within 60 s:\n{}",
                    self.kind.name(),
                    self.tail(30)
                ));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// The last `n` log lines.
    pub fn tail(&self, n: usize) -> String {
        let lines = self.lines.lock().expect("log lines lock");
        lines[lines.len().saturating_sub(n)..].join("\n")
    }

    /// Every log line so far.
    pub fn log_text(&self) -> String {
        let mut text = self.lines.lock().expect("log lines lock").join("\n");
        text.push('\n');
        text
    }

    /// Number of log lines so far (a mark for [`Server::mails_since`]).
    pub fn log_mark(&self) -> usize {
        self.lines.lock().expect("log lines lock").len()
    }

    /// Every mail logged after the mark `since`.
    pub fn mails_since(&self, since: usize) -> Vec<Mail> {
        let lines = self.lines.lock().expect("log lines lock");
        lines[since.min(lines.len())..]
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v.get("msg").and_then(|m| m.as_str()) == Some("mail (log transport)"))
            .map(|v| {
                let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
                Mail { to: field("to"), subject: field("subject"), text: field("text") }
            })
            .collect()
    }

    /// Log records (parsed) after the mark `since` whose message is `msg`.
    pub fn log_records(&self, since: usize, msg: &str) -> Vec<serde_json::Value> {
        let lines = self.lines.lock().expect("log lines lock");
        lines[since.min(lines.len())..]
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v.get("msg").and_then(|m| m.as_str()) == Some(msg))
            .collect()
    }

    /// Runs an administration command (`bin/admin.js ...` or `scacelith-server admin ...`)
    /// against the server's database; returns (success, stdout, stderr).
    pub async fn admin(&self, args: &[&str]) -> (bool, String, String) {
        let mut full = vec!["admin"];
        full.extend_from_slice(args);
        let mut cmd = match self.kind {
            Kind::Node => {
                let mut c = Command::new(&self.programs.node);
                c.arg(self.programs.node_dir.join("bin/admin.js")).args(args);
                c
            }
            Kind::Rust => {
                let mut c = Command::new(&self.programs.rust);
                c.args(&full);
                c
            }
        };
        cmd.current_dir(&self.dir).env_clear().envs(base_process_env()).envs(self.env.iter().cloned());
        cmd.env("SCACELITH_MODERATOR", "rest-diff");
        match cmd.output().await {
            Ok(out) => (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).to_string(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            ),
            Err(e) => (false, String::new(), e.to_string()),
        }
    }

    /// Stops the server (SIGTERM, then SIGKILL after 15 s).
    pub async fn stop(mut self) {
        if let Some(pid) = self.child.id() {
            let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status().await;
            if tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await.is_ok() {
                return;
            }
        }
        let _ = self.child.kill().await;
    }
}

fn base_process_env() -> Vec<(String, String)> {
    ["PATH", "HOME"].iter().filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect()
}

fn command(kind: Kind, programs: &Programs, dir: &Path, env: &[(String, String)], args: &[&str]) -> Command {
    let mut cmd = match kind {
        Kind::Node => {
            let mut c = Command::new(&programs.node);
            c.arg(programs.node_dir.join("bin/scacelith-server.js")).args(args);
            c
        }
        Kind::Rust => {
            let mut c = Command::new(&programs.rust);
            c.args(args);
            c
        }
    };
    cmd.current_dir(dir).env_clear().envs(base_process_env()).envs(env.iter().cloned()).stdin(Stdio::null());
    cmd
}

fn spawn_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(r: R, lines: Arc<Mutex<Vec<String>>>) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(r).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            lines.lock().expect("log lines lock").push(line);
        }
    });
}
