//! The two servers driven together. A scenario describes a step once, as a function of the side
//! (its saved tokens, links and game ids), and the step runs on both servers at the same moment;
//! the two answers are normalised and compared, and the differences recorded.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use scacelith_client::{Endpoint, TlsConfig};

use crate::diff::{self, Difference};
use crate::http::{Client, Req, Resp};
use crate::normalize::{self, Context};
use crate::realtime::{self, GameScript, Rt};
use crate::report::Report;
use crate::servers::{Kind, Mail, Server};

static NEXT_IP: AtomicU32 = AtomicU32::new(0);

/// A loopback source address no other simulated client uses (127.0.1.1, 127.0.1.2...).
pub fn fresh_ip() -> IpAddr {
    let n = NEXT_IP.fetch_add(1, Ordering::Relaxed);
    IpAddr::V4(Ipv4Addr::new(127, 0, (1 + n / 250) as u8, (1 + n % 250) as u8))
}

/// One server and what the scenarios learnt from it.
pub struct Side {
    /// Which implementation.
    pub kind: Kind,
    /// The running server.
    pub server: Server,
    /// Saved values (tokens, link tokens, ids, cursors).
    pub vars: HashMap<String, String>,
    /// Normalisation context (game ids and their labels).
    pub ctx: Context,
    clients: HashMap<IpAddr, Client>,
    mails_taken: HashMap<String, usize>,
    rt: HashMap<String, Rt>,
}

impl Side {
    /// A side around a started server.
    pub fn new(server: Server) -> Side {
        Side {
            kind: server.kind,
            server,
            vars: HashMap::new(),
            ctx: Context::default(),
            clients: HashMap::new(),
            mails_taken: HashMap::new(),
            rt: HashMap::new(),
        }
    }

    /// A saved value (`<missing:key>` when the step that should have saved it failed, which
    /// then shows in the comparison).
    pub fn v(&self, key: &str) -> String {
        self.vars.get(key).cloned().unwrap_or_else(|| format!("<missing:{key}>"))
    }

    /// The id of the game labelled `label` on this side.
    pub fn game(&self, label: &str) -> u64 {
        self.ctx.games.iter().find(|(_, l)| l == label).map(|(g, _)| *g).unwrap_or(0)
    }

    /// Sends a request from the simulated client at `ip`.
    pub async fn send(&mut self, ip: IpAddr, req: &Req) -> Resp {
        let target = self.server.target.clone();
        let client = self.clients.entry(ip).or_insert_with(|| Client::new(target, ip));
        client.send(req).await
    }

    fn endpoint(&self) -> Result<Endpoint, String> {
        let t = &self.server.target;
        Ok(match &t.tls {
            Some(cfg) => Endpoint::tls(t.addr, "localhost", TlsConfig::from_rustls(cfg.clone())),
            None => Endpoint::plain(t.addr),
        })
    }

    /// Waits for the next mail to `to` not taken yet (at most 5 s).
    async fn next_mail(&mut self, to: &str) -> Option<Mail> {
        let taken = self.mails_taken.get(to).copied().unwrap_or(0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mails: Vec<Mail> = self.server.mails_since(0).into_iter().filter(|m| m.to == to).collect();
            if let Some(m) = mails.get(taken) {
                self.mails_taken.insert(to.to_string(), taken + 1);
                return Some(m.clone());
            }
            if Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn mails_pending(&self, to: &str) -> usize {
        let taken = self.mails_taken.get(to).copied().unwrap_or(0);
        self.server.mails_since(0).iter().filter(|m| m.to == to).count().saturating_sub(taken)
    }
}

/// The two answers of a step.
pub struct Pair {
    /// The Node server's answer.
    pub node: Resp,
    /// The Rust server's answer.
    pub rust: Resp,
}

impl Pair {
    /// The answer of the side `kind`.
    pub fn of(&self, kind: Kind) -> &Resp {
        match kind {
            Kind::Node => &self.node,
            Kind::Rust => &self.rust,
        }
    }
}

/// Both servers and the record of the run.
pub struct Duo {
    /// The Node side.
    pub node: Side,
    /// The Rust side.
    pub rust: Side,
    /// The report the steps go into.
    pub report: Report,
    profile: String,
    scenario: String,
}

impl Duo {
    /// Both sides of a profile.
    pub fn new(profile: &str, node: Server, rust: Server, report: Report) -> Duo {
        Duo { node: Side::new(node), rust: Side::new(rust), report, profile: profile.into(), scenario: String::new() }
    }

    /// Starts a scenario (its name prefixes the step ids).
    pub fn scenario(&mut self, name: &str) {
        self.scenario = name.to_string();
    }

    fn step_id(&self, id: &str) -> String {
        format!("{}/{}/{}", self.profile, self.scenario, id)
    }

    /// Records a harness warning.
    pub fn warn(&mut self, msg: impl Into<String>) {
        let line = format!("[{}/{}] {}", self.profile, self.scenario, msg.into());
        self.report.warnings.push(line);
    }

    /// Runs a request on both servers from the client at `ip` and compares the answers.
    /// `expect` (0: none) is the status the scenario expects: when either server gives another,
    /// a warning says that the scenario may not test what it means to.
    pub async fn step(&mut self, id: &str, ip: IpAddr, expect: u16, build: impl Fn(&Side) -> Req) -> Pair {
        let (rn, rr) = (build(&self.node), build(&self.rust));
        let (node, rust) = (&mut self.node, &mut self.rust);
        let (a, b) = tokio::join!(node.send(ip, &rn), rust.send(ip, &rr));
        self.compare(id, &rn, &a, &b, expect);
        Pair { node: a, rust: b }
    }

    /// Compares two answers obtained by the scenario itself (`req`: the Node side's request,
    /// for the report) and records the step.
    pub fn compare(&mut self, id: &str, req: &Req, a: &Resp, b: &Resp, expect: u16) {
        let na = normalize::normalize(a, &self.node.ctx);
        let nb = normalize::normalize(b, &self.rust.ctx);
        let diffs = diff::compare(&na, &nb);
        if expect != 0 && (a.status != expect || b.status != expect) {
            let detail = |r: &Resp| match &r.error {
                Some(e) => format!("{} ({e})", r.status),
                None => format!("{} {}", r.status, diff::clip(&String::from_utf8_lossy(&r.body))),
            };
            self.warn(format!("{id}: expected status {expect}, node {}, rust {}", detail(a), detail(b)));
        }
        self.report.cover(&req.method, &req.target);
        let full = self.step_id(id);
        self.report.record(full, req.summary(), (a.status, b.status), diffs);
    }

    /// Saves the JSON value at `path` of each side's answer as `key`.
    pub fn save(&mut self, pair: &Pair, key: &str, path: &str) {
        for (side, resp) in [(&mut self.node, &pair.node), (&mut self.rust, &pair.rust)] {
            if let Some(v) = resp.text_at(path) {
                side.vars.insert(key.to_string(), v);
            }
        }
    }

    /// Saves a value computed from each side's answer as `key`.
    pub fn save_with(&mut self, pair: &Pair, key: &str, f: impl Fn(&Resp) -> Option<String>) {
        for (side, resp) in [(&mut self.node, &pair.node), (&mut self.rust, &pair.rust)] {
            if let Some(v) = f(resp) {
                side.vars.insert(key.to_string(), v);
            }
        }
    }

    /// Sets `key` on both sides to a value computed from the side.
    pub fn set(&mut self, key: &str, f: impl Fn(&Side) -> String) {
        let (a, b) = (f(&self.node), f(&self.rust));
        self.node.vars.insert(key.to_string(), a);
        self.rust.vars.insert(key.to_string(), b);
    }

    /// Takes the next mail to `to` on both servers, compares subject and text, and saves the
    /// `token` parameter of its first link as `link_var` (when given).
    pub async fn mail(&mut self, id: &str, to: &str, link_var: Option<&str>) -> (Option<Mail>, Option<Mail>) {
        let (node, rust) = (&mut self.node, &mut self.rust);
        let (a, b) = tokio::join!(node.next_mail(to), rust.next_mail(to));
        let mut diffs = Vec::new();
        match (&a, &b) {
            (Some(x), Some(y)) => {
                let (sx, sy) =
                    (normalize::normalize_text(&x.subject, &self.node.ctx), normalize::normalize_text(&y.subject, &self.rust.ctx));
                if sx != sy {
                    diffs.push(Difference { aspect: "mail subject".into(), node: sx, rust: sy });
                }
                let (tx, ty) =
                    (normalize::normalize_text(&x.text, &self.node.ctx), normalize::normalize_text(&y.text, &self.rust.ctx));
                if tx != ty {
                    let lx: Vec<&str> = tx.lines().collect();
                    let ly: Vec<&str> = ty.lines().collect();
                    let i = (0..lx.len().max(ly.len())).find(|&i| lx.get(i) != ly.get(i)).unwrap_or(0);
                    diffs.push(Difference {
                        aspect: format!("mail text line {}", i + 1),
                        node: lx.get(i).copied().unwrap_or("(end)").to_string(),
                        rust: ly.get(i).copied().unwrap_or("(end)").to_string(),
                    });
                }
            }
            (None, None) => self.warn(format!("{id}: no mail to {to} on either server")),
            _ => diffs.push(Difference {
                aspect: "mail".into(),
                node: a.as_ref().map_or("(no mail)".into(), |m| m.subject.clone()),
                rust: b.as_ref().map_or("(no mail)".into(), |m| m.subject.clone()),
            }),
        }
        let full = self.step_id(id);
        self.report.record(full, format!("mail to {to}"), (0, 0), diffs);
        if let Some(var) = link_var {
            for (side, m) in [(&mut self.node, &a), (&mut self.rust, &b)] {
                if let Some(t) = m.as_ref().and_then(|m| link_token(&m.text)) {
                    side.vars.insert(var.to_string(), t);
                }
            }
        }
        (a, b)
    }

    /// Waits `wait_ms` and compares how many new mails `to` received on each server (and warns
    /// when it is not `expected`); the mails are taken.
    pub async fn mail_count(&mut self, id: &str, to: &str, expected: usize, wait_ms: u64) {
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        let (a, b) = (self.node.mails_pending(to), self.rust.mails_pending(to));
        for side in [&mut self.node, &mut self.rust] {
            let n = side.mails_pending(to);
            *side.mails_taken.entry(to.to_string()).or_insert(0) += n;
        }
        let mut diffs = Vec::new();
        if a != b {
            diffs.push(Difference { aspect: "mail count".into(), node: a.to_string(), rust: b.to_string() });
        }
        if a != expected || b != expected {
            self.warn(format!("{id}: expected {expected} mails to {to}, node {a}, rust {b}"));
        }
        let full = self.step_id(id);
        self.report.record(full, format!("mails to {to}"), (0, 0), diffs);
    }

    /// Opens the realtime connection of the player `key` with the session token saved as
    /// `token_var`, on both servers.
    pub async fn connect(&mut self, key: &str, token_var: &str) -> bool {
        let mut ok = true;
        for side in [&mut self.node, &mut self.rust] {
            let token = side.v(token_var);
            let result = match side.endpoint() {
                Ok(ep) => Rt::connect(side.kind, &ep, &token).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(rt) => {
                    side.rt.insert(key.to_string(), rt);
                }
                Err(e) => {
                    ok = false;
                    let msg = format!("realtime connection of {key} on {}: {e}", side.kind.name());
                    let line = format!("[{}/{}] {msg}", self.profile, self.scenario);
                    self.report.warnings.push(line);
                }
            }
        }
        ok
    }

    /// Closes every realtime connection.
    pub async fn disconnect_all(&mut self) {
        for side in [&mut self.node, &mut self.rust] {
            for (_, rt) in side.rt.drain() {
                rt.close().await;
            }
        }
    }

    /// Plays `script` between the connected players `white` and `black` (whose user name is
    /// `black_name`) on both servers at once, labels the game `label` and waits until both
    /// servers serve it on `GET /games/:id`.
    pub async fn play(&mut self, label: &str, white: &str, black: &str, black_name: &str, script: &GameScript) {
        let (node, rust) = (&mut self.node, &mut self.rust);
        let (a, b) = tokio::join!(play_on(node, white, black, black_name, script), play_on(rust, white, black, black_name, script));
        for (side, result) in [(Kind::Node, a), (Kind::Rust, b)] {
            match result {
                Ok(game) => {
                    let s = if side == Kind::Node { &mut self.node } else { &mut self.rust };
                    s.ctx.games.push((game, label.to_string()));
                }
                Err(e) => self.warn(format!("game {label} on {}: {e}", side.name())),
            }
        }
        let ip = fresh_ip();
        for side in [&mut self.node, &mut self.rust] {
            let game = side.game(label);
            if game == 0 {
                continue;
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let r = side.send(ip, &Req::get(format!("/api/v1/games/{game}"))).await;
                if r.status == 200 || Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    /// Runs an administration command on both servers (not compared: the CLI is not part of
    /// the API); warns when either fails.
    pub async fn admin(&mut self, args: &[&str]) {
        let (a, b) = tokio::join!(self.node.server.admin(args), self.rust.server.admin(args));
        for (kind, (ok, out, err)) in [(Kind::Node, a), (Kind::Rust, b)] {
            if !ok {
                self.warn(format!("admin {} failed on {}: {}{}", args.join(" "), kind.name(), out, err));
            }
        }
    }

    /// Ends the profile: the report, and both servers to stop.
    pub fn finish(self) -> (Report, Server, Server) {
        (self.report, self.node.server, self.rust.server)
    }
}

async fn play_on(side: &mut Side, white: &str, black: &str, black_name: &str, script: &GameScript) -> Result<u64, String> {
    let mut w = side.rt.remove(white).ok_or_else(|| format!("{white} is not connected"))?;
    let mut b = match side.rt.remove(black) {
        Some(b) => b,
        None => {
            side.rt.insert(white.to_string(), w);
            return Err(format!("{black} is not connected"));
        }
    };
    let result = realtime::play(&mut w, &mut b, black_name, script).await;
    side.rt.insert(white.to_string(), w);
    side.rt.insert(black.to_string(), b);
    result
}

/// The `token` query parameter of the first link of a mail.
pub fn link_token(text: &str) -> Option<String> {
    let start = text.find("token=")? + "token=".len();
    let rest = &text[start..];
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '%')).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}
