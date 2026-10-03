//! Protection per address on the real server (the TLS gate, the HTTP guard and the abuse tracker),
//! with two workers. 127.0.0.1 is in `ABUSE_EXEMPT` (the harness's own requests, and alice);
//! 127.0.0.2 is an ordinary address (Linux routes the whole 127.0.0.0/8 to the loopback). bobby
//! plays from 127.0.0.2 over a WebSocket opened before the flood, as a player who shares a school's
//! or a mobile operator's address with an abuser would.
//!
//! * A client of 127.0.0.2 flooding 404s over keep-alive connections is refused (429), then
//!   blocked: its new TCP connections get a reset before TLS within seconds, while 127.0.0.1 keeps
//!   working and bobby's game goes on over his open WebSocket. Once the block ends 127.0.0.2
//!   connects again, and a second flood is blocked 4 times longer.
//! * An exempt address is never refused nor blocked.
//!
//! Port of the Node.js `test/integration/abuse.test.js`. Its first two tests depend on each other
//! (the second block is the second of the address): they are one test here, each test running its
//! own server. The Rust server is one process whose workers share one guard, so the block is one
//! gauge (`scacelith_abuse_blocked_keys`) where the Node.js server had one per worker process.

#[macro_use]
mod support;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use scacelith_client::http::{HttpConnection, Request};
use scacelith_client::{ClientError, Endpoint};
use serde_json::json;
use support::*;

const FLOODER: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
const EXEMPT: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
/// The first block, in seconds: long enough for the checks made while it lasts on a loaded
/// machine.
const BASE_SEC: u64 = 5;
const TLS_BLOCKED: &str = "scacelith_tls_refused_total{reason=\"blocked\"}";

async fn server() -> TestServer {
    TestServer::options()
        .workers(2)
        .env("ABUSE_EXEMPT", "127.0.0.1")
        .env("HTTP_RATE_PER_IP", "60") // a burst of 30
        .env("ABUSE_BLOCK_REFUSALS_PER_MIN", "200")
        .env("ABUSE_BLOCK_BASE_SEC", &BASE_SEC.to_string())
        .env("ABUSE_BLOCK_MAX_SEC", "600")
        .env("LOG_IP", "full") // tell 127.0.0.1 from 127.0.0.2 in the log
        .start()
        .await
}

/// How a request or a connection failed, as a short name (`reset`, `timeout`...).
fn failure(e: &ClientError) -> String {
    match e {
        ClientError::Io(io) => match io.kind() {
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => "reset".into(),
            ErrorKind::BrokenPipe => "broken_pipe".into(),
            ErrorKind::UnexpectedEof => "eof".into(),
            kind => format!("io: {kind:?}"),
        },
        ClientError::Timeout(_) => "timeout".into(),
        other => format!("{other}"),
    }
}

/// One GET from `from` on a new connection: its status, or how the connection failed.
async fn get_from(srv: &TestServer, from: IpAddr) -> String {
    let fut = async {
        let mut conn = HttpConnection::open(&srv.endpoint_from(from)).await?;
        conn.send(&Request::get("/api/v1/info")).await
    };
    match tokio::time::timeout(Duration::from_secs(3), fut).await {
        Ok(Ok(res)) => res.status.to_string(),
        Ok(Err(e)) => failure(&e),
        Err(_) => "timeout".into(),
    }
}

/// A TLS connection from `from`: `tls` when the handshake completed, else how it failed.
async fn tls_from(srv: &TestServer, from: IpAddr) -> String {
    match tokio::time::timeout(Duration::from_secs(3), srv.endpoint_from(from).connect()).await {
        Ok(Ok(_)) => "tls".into(),
        Ok(Err(e)) => failure(&e),
        Err(_) => "timeout".into(),
    }
}

/// What a flood met: the count of each outcome and when the first 429 came.
#[derive(Debug, Default)]
struct Flood {
    counts: BTreeMap<String, usize>,
    first_429: Option<Instant>,
}

impl Flood {
    fn count(&self, k: &str) -> usize {
        self.counts.get(k).copied().unwrap_or(0)
    }
}

/// Floods 404s from `endpoint` over 8 keep-alive connections until `stop()` is true or `limit`
/// passed.
async fn flood(endpoint: &Endpoint, limit: Duration, stop: impl Fn(&Flood) -> bool) -> Flood {
    let out = RefCell::new(Flood::default());
    let next = Cell::new(0u64);
    let t0 = Instant::now();
    let lane = || async {
        let mut conn: Option<HttpConnection> = None;
        while t0.elapsed() < limit && !stop(&out.borrow()) {
            let i = next.get();
            next.set(i + 1);
            let path = format!("/api/v1/nothing-{i}");
            let answer = tokio::time::timeout(Duration::from_secs(3), async {
                let c = match conn.as_mut() {
                    Some(c) => c,
                    None => conn.insert(HttpConnection::open(endpoint).await?),
                };
                c.send(&Request::get(&path)).await
            })
            .await;
            let outcome = match answer {
                Ok(Ok(res)) => {
                    if !conn.as_ref().is_some_and(HttpConnection::is_reusable) {
                        conn = None;
                    }
                    res.status.to_string()
                }
                Ok(Err(e)) => {
                    conn = None;
                    failure(&e)
                }
                Err(_) => {
                    conn = None;
                    "timeout".into()
                }
            };
            let failed = !outcome.chars().all(|c| c.is_ascii_digit());
            {
                let mut out = out.borrow_mut();
                if outcome == "429" && out.first_429.is_none() {
                    out.first_429 = Some(Instant::now());
                }
                *out.counts.entry(outcome).or_insert(0) += 1;
            }
            if failed {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    join_all((0..8).map(|_| lane())).await;
    out.into_inner()
}

/// When `n` new connections at once from `from` were all reset before TLS, trying for `limit`.
async fn blocked_everywhere(srv: &TestServer, from: IpAddr, n: usize, limit: Duration) -> Option<Instant> {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        let all = join_all((0..n).map(|_| tls_from(srv, from))).await;
        if all.iter().all(|r| r == "reset") {
            return Some(Instant::now());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// Floods from 127.0.0.2 until its new connections are all reset before TLS; returns the flood
/// and when the block was seen.
async fn flood_until_blocked(srv: &TestServer) -> (Flood, Option<Instant>) {
    let blocked_at: Cell<Option<Instant>> = Cell::new(None);
    let watcher = async {
        let t0 = Instant::now();
        while blocked_at.get().is_none() && t0.elapsed() < Duration::from_secs(20) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            blocked_at.set(blocked_everywhere(srv, FLOODER, 6, Duration::from_millis(300)).await);
        }
    };
    let endpoint = srv.endpoint_from(FLOODER);
    let (f, ()) =
        tokio::join!(flood(&endpoint, Duration::from_secs(15), |_| blocked_at.get().is_some()), watcher);
    (f, blocked_at.get())
}

#[tokio::test]
async fn a_flooding_address_is_refused_then_blocked_before_tls_others_keep_working_and_its_next_block_is_4_times_longer()
 {
    let srv = server().await;
    let [a, b] = accounts(&srv, ["alice", "bobby"]).await;
    let alice_client = connect(&srv, &a.token).await.expect("alice connects");
    let bobby_client = Client::connect(&srv.endpoint_from(FLOODER), &b.token).await.expect("bobby connects");
    let (mut alice, mut bobby) =
        (Player { acc: a, client: alice_client }, Player { acc: b, client: bobby_client });
    let mut table = Table::new(challenge_game(&mut alice, &mut bobby, 600, 5, false).await);
    table.play_all(&mut alice, &mut bobby, &["e2e4", "e7e5"]).await;

    // Refused, then blocked before TLS within seconds.
    let refused_before = srv.metric(TLS_BLOCKED).await;
    let (f, blocked_at) = flood_until_blocked(&srv).await;
    assert!(f.count("404") > 0 && f.count("429") > 0, "refused before being blocked: {:?}", f.counts);
    let blocked_at = blocked_at.expect("blocked");
    let delay = blocked_at.duration_since(f.first_429.expect("a first 429"));
    println!("flood: {:?}; blocked {delay:?} after the first 429", f.counts);
    assert!(delay < Duration::from_secs(4), "blocked within seconds ({delay:?})");

    // The block is known and turned the connections away before TLS.
    let text = srv.metrics().await;
    assert!(metric_value(&text, "scacelith_abuse_blocked_keys") >= 1.0, "a blocked address");
    assert!(metric_value(&text, TLS_BLOCKED) - refused_before >= 6.0, "refused before TLS");
    let flooder = FLOODER.to_string();
    let log = srv
        .logs
        .wait(WAIT, |l| l["msg"] == "ip blocked" && l["ip"] == flooder.as_str())
        .await
        .unwrap_or_else(|| {
            panic!("the block of {flooder} is logged: {:?}", srv.logs.matching(|l| l["msg"] == "ip blocked"))
        });
    assert_eq!(
        (&log["level"], &log["scope"], &log["blockLevel"], &log["ttlSec"]),
        (&json!("warn"), &json!("ip"), &json!(1), &json!(BASE_SEC))
    );

    // The exempt address is served; the WebSocket that 127.0.0.2 opened before is not cut.
    assert_eq!(get_from(&srv, EXEMPT).await, "200");
    assert_eq!(tls_from(&srv, FLOODER).await, "reset");
    table.play_all(&mut alice, &mut bobby, &["g1f3", "b8c6"]).await;
    assert!(bobby.client.is_open());

    // The first block lasts BASE_SEC: the address connects again afterwards.
    let t0 = Instant::now();
    let served = eventually(Duration::from_secs(4 * BASE_SEC), "the end of the first block", || async {
        let status = get_from(&srv, FLOODER).await;
        if status == "200" {
            Some(())
        } else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            None
        }
    });
    served.await;
    assert!(t0.elapsed() <= Duration::from_secs(BASE_SEC + 2), "served again after {:?}", t0.elapsed());

    // A second flood is blocked 4 times longer.
    let (_, blocked_again) = flood_until_blocked(&srv).await;
    assert!(blocked_again.is_some(), "blocked again");
    let log = srv
        .logs
        .wait(WAIT, |l| l["msg"] == "ip blocked" && l["ip"] == flooder.as_str() && l["blockLevel"] == 2)
        .await
        .expect("the second block is logged");
    assert_eq!(log["ttlSec"], 4 * BASE_SEC);
    // Time passes beyond the first block's duration (the scenario, not a wait for a state).
    tokio::time::sleep(Duration::from_secs(BASE_SEC + 1)).await;
    assert_eq!(tls_from(&srv, FLOODER).await, "reset", "still blocked after the first duration");
    table.play_all(&mut alice, &mut bobby, &["f1c4", "g8f6"]).await;
}

#[tokio::test]
async fn an_exempt_address_is_never_refused_nor_blocked() {
    let srv = server().await;
    // Well beyond HTTP_RATE_PER_IP (60 a minute, a burst of 30).
    let f = flood(&srv.endpoint_from(EXEMPT), Duration::from_secs(20), |f| f.count("404") > 250).await;
    assert_eq!(f.counts.keys().collect::<Vec<_>>(), ["404"], "{:?}", f.counts);
    assert!(f.count("404") > 250, "{:?}", f.counts);
    assert!(srv.logs.matching(|l| l["msg"] == "ip blocked").is_empty(), "nothing blocked");
    assert_eq!(get_from(&srv, EXEMPT).await, "200");
    assert_eq!(srv.metric("scacelith_abuse_blocked_keys").await, 0.0);
}
