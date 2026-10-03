//! A full server (DESIGN 5.8, "Players first at MAX_CONNECTIONS"), on the real server with one
//! worker and `MAX_CONNECTIONS=2`, both places held by two signed-in players with a game in
//! progress. Newcomers are all refused: at `MAX_CONNECTIONS` itself each one completes TLS and the
//! upgrade and gets `ServerFull` at Hello; once the upgrade reserve (max(16, 2 %), so 16 here) is in
//! use as well, an upgrade may get HTTP 503 and the TLS gate may close connections before TLS. A
//! player of the game in progress who comes back while the server is full still gets Welcome and
//! the game, and the connection counts return to the players actually online.
//!
//! Port of the Node.js `test/integration/admission.test.js`.

#[macro_use]
mod support;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::time::Duration;

use scacelith_client::ClientError;
use scacelith_protocol::{Color, ErrorCode, GameEventKind, ServerMsg, close};
use support::*;

const HELLO_FULL: &str = "scacelith_ws_hello_total{result=\"server_full\"}";
const UPGRADE_FULL: &str = "scacelith_ws_handshakes_rejected_total{reason=\"server_full\"}";
const TLS_FULL: &str = "scacelith_tls_refused_total{reason=\"server_full\"}";

/// The full server: alice and bobby hold both places with a game in progress (two moves played);
/// carol is the newcomer (signed in, never let in); dave takes a place a player freed.
struct Full {
    srv: TestServer,
    alice: Player,
    bobby: Player,
    carol: Account,
    dave: Account,
    table: Table,
}

async fn full_server() -> Full {
    let srv = TestServer::options().env("MAX_CONNECTIONS", "2").env("SHUTDOWN_GRACE_MS", "0").start().await;
    let [a, b, carol, dave] = accounts(&srv, ["alice", "bobby", "carol", "dave"]).await;
    let alice_client = connect(&srv, &a.token).await.expect("alice connects");
    let bobby_client = connect(&srv, &b.token).await.expect("bobby connects");
    let mut alice = Player { acc: a, client: alice_client };
    let mut bobby = Player { acc: b, client: bobby_client };
    let id = challenge_game(&mut alice, &mut bobby, 180, 2, false).await;
    let mut table = Table::new(id);
    table.play_all(&mut alice, &mut bobby, &["e2e4", "e7e5"]).await;
    Full { srv, alice, bobby, carol, dave, table }
}

/// One connection attempt with `token`, classified by how it ended.
async fn attempt(srv: &TestServer, token: &str) -> String {
    match connect(srv, token).await {
        Ok(mut c) => {
            c.close().await;
            "welcome".into()
        }
        Err(ClientError::Refused { code: ErrorCode::ServerFull, close: Some(c) })
            if c.code == close::SERVER_FULL =>
        {
            "server_full_at_hello".into()
        }
        Err(ClientError::UpgradeRefused { status: 503, .. }) => "http_503".into(),
        Err(ClientError::Io(e))
            if matches!(
                e.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::BrokenPipe
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionAborted
            ) =>
        {
            "closed_before_tls".into()
        }
        Err(e) => format!("other: {e}"),
    }
}

/// `total` attempts, `concurrency` at a time; returns the count of each outcome.
async fn newcomers(
    srv: &TestServer,
    token: &str,
    total: usize,
    concurrency: usize,
) -> BTreeMap<String, usize> {
    let out = RefCell::new(BTreeMap::new());
    let started = Cell::new(0);
    let lane = || async {
        while started.get() < total {
            started.set(started.get() + 1);
            let k = attempt(srv, token).await;
            *out.borrow_mut().entry(k).or_insert(0) += 1;
        }
    };
    join_all((0..concurrency).map(|_| lane())).await;
    out.into_inner()
}

/// Waits until the server counts `n` players online and `n` WebSocket connections.
async fn presence_back_to(srv: &TestServer, n: f64) {
    let mut seen = (0.0, 0.0);
    let back = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let text = srv.metrics().await;
            seen = (
                metric_value(&text, "scacelith_presence_online"),
                metric_value(&text, "scacelith_presence_connections"),
            );
            if seen == (n, n) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(back.is_ok(), "players online and WebSocket connections: {seen:?}, expected {n}");
}

#[tokio::test]
async fn a_server_at_max_connections_refuses_every_newcomer_at_hello_while_the_upgrade_reserve_lasts() {
    let full = full_server().await;
    let srv = &full.srv;
    presence_back_to(srv, 2.0).await;
    let text = srv.metrics().await;
    let (hello0, upgrade0, tls0) =
        (metric_value(&text, HELLO_FULL), metric_value(&text, UPGRADE_FULL), metric_value(&text, TLS_FULL));

    // A few at a time, well inside the reserve: every newcomer completes TLS and the upgrade and
    // is refused at Hello. Nothing is closed before TLS (players first).
    let calm = newcomers(srv, &full.carol.token, 24, 4).await;
    assert_eq!(calm, BTreeMap::from([("server_full_at_hello".to_string(), 24)]));
    let text = srv.metrics().await;
    assert_eq!(metric_value(&text, HELLO_FULL) - hello0, 24.0, "the metric of a full server counts them");
    assert_eq!(metric_value(&text, UPGRADE_FULL) - upgrade0, 0.0, "no upgrade refused inside the reserve");
    assert_eq!(metric_value(&text, TLS_FULL) - tls0, 0.0, "no shedding before TLS at MAX_CONNECTIONS itself");
    presence_back_to(srv, 2.0).await;

    // More at once than the reserve: some upgrades may now be refused (503) and the gate may shed,
    // but nobody gets in, whatever the timing.
    let text = srv.metrics().await;
    let (hello1, upgrade1) = (metric_value(&text, HELLO_FULL), metric_value(&text, UPGRADE_FULL));
    let burst = newcomers(srv, &full.carol.token, 64, 32).await;
    for k in burst.keys() {
        assert!(
            ["server_full_at_hello", "http_503", "closed_before_tls"].contains(&k.as_str()),
            "unexpected outcome {k}: {burst:?}"
        );
    }
    assert_eq!(burst.values().sum::<usize>(), 64);
    let count = |k: &str| burst.get(k).copied().unwrap_or(0) as f64;
    let text = srv.metrics().await;
    assert_eq!(metric_value(&text, HELLO_FULL) - hello1, count("server_full_at_hello"));
    assert_eq!(metric_value(&text, UPGRADE_FULL) - upgrade1, count("http_503"));
    // The reserve's counts do not drift: only the two players remain.
    presence_back_to(srv, 2.0).await;
}

#[tokio::test]
async fn a_player_whose_game_is_in_progress_comes_back_to_a_full_server_and_gets_welcome_and_the_game() {
    let Full { srv, mut alice, mut bobby, carol, dave, mut table } = full_server().await;
    let id = table.id;

    // Alice loses her connection; a newcomer takes the place she freed, so the server is full again.
    let mb = bobby.client.mark();
    alice.client.conn().abort();
    let gone = wait_msg!(bobby.client, mb, ServerMsg::GameEvent(e) if e.kind == GameEventKind::PlayerDisconnected => e.clone());
    assert_eq!(gone.color, Color::White);
    let mut dave =
        Player { client: connect(&srv, &dave.token).await.expect("dave takes the place"), acc: dave };
    presence_back_to(&srv, 2.0).await;
    assert_eq!(attempt(&srv, &carol.token).await, "server_full_at_hello", "the server is full");

    // Alice comes back while it is full: admitted at Hello for her game, beyond MAX_CONNECTIONS.
    alice.reconnect(&srv).await;
    assert_eq!(alice.client.welcome().active_game, id);
    let snap = wait_msg!(alice.client, 0, ServerMsg::GameSnapshot(s) if s.game == id => s.clone());
    assert_eq!(snap.moves.len(), 2);
    wait_msg!(bobby.client, mb, ServerMsg::GameEvent(e) if e.kind == GameEventKind::PlayerReconnected => ());
    presence_back_to(&srv, 3.0).await;
    table.play(&mut alice, &mut bobby, "g1f3").await; // the game goes on
    assert_eq!(attempt(&srv, &carol.token).await, "server_full_at_hello", "still full for newcomers");

    dave.client.close().await;
    presence_back_to(&srv, 2.0).await;
}
