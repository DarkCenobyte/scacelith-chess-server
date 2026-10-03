//! The host actors end to end: [`Hosts::start`] on a real in-memory store and a journal
//! directory, the system clock, and test connections in place of the realtime layer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use scacelith_chess::ChessGame;
use scacelith_protocol::{
    ClientGesture, ClientMsg, Color, EndReason as ER, GameStatus as GS, Message, Move, MsgType, ServerMsg,
};

use super::persistence::{real_store, shown};
use super::{Ep, TempDir, new_game, snapshot};
use crate::clock;
use crate::config::test_config;
use crate::events::NewGame;
use crate::game::host::{HostDeps, HostError, HostHandle, HostLoad, Hosts};
use crate::game::rules::Rules;
use crate::game::testing::RecordingEvents;
use crate::ids::{GameId, UserId};
use crate::store::Store;

/// Waits (5 s at most) until `done` holds.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let limit = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Whether a connection received a message of this type.
fn got(ep: &Ep, ty: MsgType) -> bool {
    ep.types().contains(&ty)
}

/// The dependencies of the hosts, journaling under `dir`.
fn deps(dir: &TempDir, store: &Store, events: &Arc<RecordingEvents>) -> HostDeps {
    let journal_dir = dir.path().to_string_lossy().into_owned();
    let config =
        test_config(&[("JOURNAL_DIR", &journal_dir), ("JOURNAL_FSYNC", "0")]).expect("configuration");
    HostDeps {
        config: Arc::new(config),
        clock: clock::system(),
        store: store.clone(),
        events: events.clone(),
        anomalies: events.clone(),
    }
}

/// Plays the moves (UCI) of a new game through a handle, mirroring the position to send its
/// digest.
fn play(host: &HostHandle, id: GameId, mirror: &mut ChessGame, moves: &[&str], players: [(UserId, &Ep); 2]) {
    for (ply, m) in moves.iter().enumerate() {
        let (user, ep) = players[ply & 1];
        let mv = scacelith_protocol::uci_to_move(m).expect("a UCI move");
        let msg = Move {
            seq: ply as u32 + 2,
            game: id,
            ply: ply as u16,
            r#move: mv,
            pos_hash: Rules::digest(mirror),
            think_ms: 0,
            draw_offer: false,
        };
        host.client(user, ClientMsg::Move(msg), ep.endpoint(), clock::mono_ms());
        assert!(Rules::play(mirror, mv).is_some(), "{m} is legal");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosts_play_commit_rate_and_restart_through_their_handles() {
    let dir = TempDir::new("e2e");
    let (store, users) = real_store(&["alice", "bob"]).await;
    let [alice, bob] = users[..] else { panic!("two accounts") };
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..2).await.expect("hosts started");
    assert_eq!(hosts.handles().iter().map(HostHandle::shard).collect::<Vec<_>>(), [0, 1]);

    // A rated game on the preferred shard.
    let host = hosts.pick(Some(1)).clone();
    assert_eq!(host.shard(), 1);
    let spec = NewGame { white: shown(alice, "alice"), black: shown(bob, "bob"), ..new_game(0, 0) };
    let id = host.create(spec).await.expect("created");
    assert_eq!(hosts.get(id).map(HostHandle::shard), Some(1));
    until("the load", || host.load() == HostLoad { games: 1, players: 2 }).await;
    assert_eq!(hosts.pick(None).shard(), 0, "the host with the fewest games");
    let (ew, eb) = (Ep::new(1, alice), Ep::new(2, bob));
    host.attach(id, alice, ew.endpoint());
    host.attach(id, bob, eb.endpoint());
    until("the snapshots", || got(&ew, MsgType::GameSnapshot) && got(&eb, MsgType::GameSnapshot)).await;
    assert_eq!(snapshot(eb.last()).you, Color::Black);

    // A gesture reaches the opponent only.
    let gesture = ClientGesture { seq: 1, game: id, ply: 0, touch: 12, aim: 28, ..ClientGesture::default() };
    host.gesture(id, alice, gesture.to_bytes().expect("a valid gesture"));
    until("the gesture", || got(&eb, MsgType::ServerGesture)).await;
    assert!(!got(&ew, MsgType::ServerGesture));

    // Fool's mate: the end, then the commit (journal flush and database) and the ratings.
    let mut mirror = ChessGame::default();
    play(&host, id, &mut mirror, &["f2f3", "e7e5", "g2g4", "d8h4"], [(alice, &ew), (bob, &eb)]);
    until("the end", || got(&ew, MsgType::GameEnd) && got(&eb, MsgType::GameEnd)).await;
    let end = eb.msgs().into_iter().find_map(|m| match m {
        ServerMsg::GameEnd(e) => Some((e.status, e.reason)),
        _ => None,
    });
    assert_eq!(end, Some((GS::BlackWins, ER::Checkmate)));
    until("the rating update", || got(&ew, MsgType::RatingUpdate) && got(&eb, MsgType::RatingUpdate)).await;
    let update = eb.msgs().into_iter().find_map(|m| match m {
        ServerMsg::RatingUpdate(u) => Some(u),
        _ => None,
    });
    let update = update.expect("a RatingUpdate");
    assert_eq!((update.white.after, update.black.after), (1480, 1520));
    // The lobby hears of the end after the RatingUpdate and the journal's commit record.
    until("the game_ended event", || !events.ended().is_empty()).await;
    assert_eq!(events.ended().iter().map(|e| (e.game, e.rated)).collect::<Vec<_>>(), [(id, true)]);
    let row = store.games().by_id(id).await.expect("read").expect("the committed game");
    assert_eq!((row.summary.status, row.summary.reason), (GS::BlackWins.to_u8(), ER::Checkmate.to_u8()));
    let stats = host.stats().await.expect("the host runs");
    assert_eq!(
        (stats.games, stats.active, stats.pending_commits),
        (1, 0, 0),
        "the room waits for the rematch window"
    );
    assert_eq!((stats.counters.moves, stats.counters.committed, stats.counters.gestures), (4, 1, 1));

    // A second game stays running through a restart.
    let second = NewGame { white: shown(bob, "bob"), black: shown(alice, "alice"), ..new_game(0, 0) };
    let g2 = hosts.handles()[0].create(second).await.expect("created");
    let mut mirror = ChessGame::default();
    let (bw, ab) = (Ep::new(3, bob), Ep::new(4, alice));
    hosts.handles()[0].attach(g2, bob, bw.endpoint());
    hosts.handles()[0].attach(g2, alice, ab.endpoint());
    play(&hosts.handles()[0], g2, &mut mirror, &["e2e4", "e7e5"], [(bob, &bw), (alice, &ab)]);
    until("the second game's moves", || ab.types().iter().filter(|t| **t == MsgType::MoveMade).count() == 2)
        .await;
    hosts.shutdown().await;
    assert_eq!(host.create(new_game(0, 0)).await, Err(scacelith_protocol::ErrorCode::ShuttingDown));
    assert!(host.stats().await.is_none());

    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..2).await.expect("hosts restarted");
    assert_eq!(events.recovered(), [(g2, bob, alice)], "only the running game comes back");
    let host = hosts.get(g2).expect("its host").clone();
    let stats = host.stats().await.expect("the host runs");
    assert_eq!((stats.games, stats.active, stats.counters.recovered), (1, 1, 1));
    // Its players come back; the game goes on.
    let bw = Ep::new(5, bob);
    host.attach(g2, bob, bw.endpoint());
    until("the snapshot", || got(&bw, MsgType::GameSnapshot)).await;
    let s = snapshot(bw.last());
    assert_eq!((s.moves.len(), s.you), (2, Color::White));
    hosts.shutdown().await;
    store.close().await;
}

#[tokio::test]
async fn hosts_refuse_a_bad_shard_range() {
    let dir = TempDir::new("e2e-bad");
    let (store, _) = real_store(&[]).await;
    let events = Arc::new(RecordingEvents::default());
    for shards in [0..0, 60..65] {
        let started = Hosts::start(deps(&dir, &store, &events), shards.clone()).await;
        assert!(matches!(started, Err(HostError::BadShards(r)) if r == shards));
    }
    store.close().await;
}
