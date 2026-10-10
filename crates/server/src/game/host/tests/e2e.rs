//! The host actors end to end: [`Hosts::start`] on a real in-memory store and a journal
//! directory, the system clock, and test connections in place of the realtime layer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use scacelith_chess::ChessGame;
use scacelith_protocol::{
    ClientGesture, ClientMsg, ClientStance, Color, EndReason as ER, ErrorCode, GameStatus as GS, Message,
    Move, MsgType, ServerMsg, Stance,
};

use super::persistence::{real_store, shown};
use super::{Ep, TempDir, new_game, resign, snapshot};
use crate::clock;
use crate::config::test_config;
use crate::events::NewGame;
use crate::game::host::{
    GESTURE_INBOX_MAX, HostDeps, HostError, HostHandle, HostLoad, Hosts, INBOX_BUSY, JOURNAL_PENDING_BUSY,
};
use crate::game::rules::Rules;
use crate::game::testing::RecordingEvents;
use crate::ids::{self, GameId, UserId};
use crate::journal::owner::OWNER_FILE;
use crate::realtime::GameHosts;
use crate::store::{Store, StoreError, WRITE_BACKLOG_BUSY};

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
    let host = hosts.pick(Some(1)).expect("a host").clone();
    assert_eq!(host.shard(), 1);
    let spec = NewGame { white: shown(alice, "alice"), black: shown(bob, "bob"), ..new_game(0, 0) };
    let id = host.create(spec).await.expect("created");
    assert_eq!(hosts.get(id).map(HostHandle::shard), Some(1));
    until("the load", || host.load() == HostLoad { games: 1, players: 2 }).await;
    assert_eq!(hosts.pick(None).map(HostHandle::shard), Some(0), "the host with the fewest games");
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
    // So does a stance.
    let stance = ClientStance { seq: 2, game: id, stance: Stance::Standing };
    host.stance(id, alice, stance.to_bytes().expect("a valid stance"));
    until("the stance", || got(&eb, MsgType::ServerStance)).await;
    assert!(!got(&ew, MsgType::ServerStance));

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
    let c = &stats.counters;
    assert_eq!((c.moves, c.committed, c.gestures, c.stances), (4, 1, 1, 1));

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

/// The shards of the hosts, and which of them are draining.
fn shards(hosts: &Hosts) -> Vec<(u32, bool)> {
    hosts.handles().iter().map(|h| (h.shard(), h.draining())).collect()
}

/// Waits (5 s at most) until the database has the game.
async fn until_stored(store: &Store, id: GameId) {
    let limit = Instant::now() + Duration::from_secs(5);
    while store.games().by_id(id).await.expect("read").is_none() {
        assert!(Instant::now() < limit, "timed out waiting for the commit of {id}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A game between `white` and `black` on `shard`, two moves played.
async fn game_on(hosts: &Hosts, shard: u32, white: (UserId, &str), black: (UserId, &str)) -> GameId {
    let host = hosts.pick(Some(shard)).expect("a host").clone();
    assert_eq!(host.shard(), shard);
    let spec = NewGame { white: shown(white.0, white.1), black: shown(black.0, black.1), ..new_game(0, 0) };
    let id = host.create(spec).await.expect("created");
    let (ew, eb) = (Ep::new(1, white.0), Ep::new(2, black.0));
    host.attach(id, white.0, ew.endpoint());
    host.attach(id, black.0, eb.endpoint());
    let mut mirror = ChessGame::default();
    play(&host, id, &mut mirror, &["e2e4", "e7e5"], [(white.0, &ew), (black.0, &eb)]);
    until("the moves", || eb.types().iter().filter(|t| **t == MsgType::MoveMade).count() == 2).await;
    id
}

/// The audit's scenario (A03): a game and its journal on shard 3, then the hosts started on
/// shards 0..1 with the same journal directory (`WORKERS` from 4 to 1). The game comes back on a
/// draining host: routable, played to its end and committed, never given a new game; the next
/// start, with nothing left in shard 3, does not start it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_smaller_shard_range_serves_the_games_left_in_the_shards_it_leaves_out() {
    let dir = TempDir::new("e2e-shrink");
    let (store, users) = real_store(&["alice", "bob", "carol", "dave"]).await;
    let [alice, bob, carol, dave] = users[..] else { panic!("four accounts") };
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..4).await.expect("hosts started");
    let id = game_on(&hosts, 3, (alice, "alice"), (bob, "bob")).await;
    assert_eq!(ids::shard_of(id), 3);
    hosts.shutdown().await;

    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..1).await.expect("hosts restarted");
    assert_eq!(shards(&hosts), [(0, false), (3, true)]);
    assert_eq!(events.recovered(), [(id, alice, bob)], "the game is taken back");
    let host = hosts.get(id).expect("the game is routable").clone();
    assert_eq!((host.shard(), host.draining()), (3, true));
    // New games go to the range only, whatever the preference.
    assert_eq!(hosts.pick(Some(3)).map(HostHandle::shard), Some(0));
    assert_eq!(hosts.pick(None).map(HostHandle::shard), Some(0));
    let other = NewGame { white: shown(carol, "carol"), black: shown(dave, "dave"), ..new_game(0, 0) };
    let g2 = hosts.pick(None).expect("a host").create(other).await.expect("created");
    assert_eq!(ids::shard_of(g2), 0);
    // Its players come back and finish it: it is committed.
    let ew = Ep::new(5, alice);
    host.attach(id, alice, ew.endpoint());
    until("the snapshot", || got(&ew, MsgType::GameSnapshot)).await;
    assert_eq!(snapshot(ew.last()).moves.len(), 2);
    host.client(alice, resign(id, 3), ew.endpoint(), clock::mono_ms());
    until("the end", || got(&ew, MsgType::GameEnd)).await;
    until_stored(&store, id).await;
    hosts.shutdown().await;

    // Nothing left in shard 3: the next start leaves it out.
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..1).await.expect("hosts restarted");
    assert_eq!(shards(&hosts), [(0, false)]);
    assert_eq!(events.recovered(), [(g2, carol, dave)]);
    assert!(hosts.get(id).is_none());
    hosts.shutdown().await;
    store.close().await;
}

/// `SHARD_BASE` moves the range, then a larger range covers the former shards again: every game
/// stays on the shard of its id (routing never depends on the range), served by a draining host
/// while it is outside the range.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_moved_or_larger_shard_range_keeps_every_journalled_game_on_its_shard() {
    let dir = TempDir::new("e2e-move");
    let (store, users) = real_store(&["alice", "bob", "carol", "dave"]).await;
    let [alice, bob, carol, dave] = users[..] else { panic!("four accounts") };
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..2).await.expect("hosts started");
    let g0 = game_on(&hosts, 0, (alice, "alice"), (bob, "bob")).await;
    let g1 = game_on(&hosts, 1, (carol, "carol"), (dave, "dave")).await;
    hosts.shutdown().await;

    // SHARD_BASE=4, WORKERS=2.
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 4..6).await.expect("hosts restarted");
    assert_eq!(shards(&hosts), [(0, true), (1, true), (4, false), (5, false)]);
    assert_eq!(events.recovered(), [(g0, alice, bob), (g1, carol, dave)]);
    for id in [g0, g1] {
        assert_eq!(hosts.get(id).map(HostHandle::shard), Some(ids::shard_of(id)));
    }
    assert_eq!(hosts.pick(Some(1)).map(HostHandle::shard), Some(4));
    hosts.shutdown().await;

    // SHARD_BASE=0, WORKERS=4: the former shards are in the range again, shards 4 and 5 hold
    // nothing.
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..4).await.expect("hosts restarted");
    assert_eq!(shards(&hosts), [(0, false), (1, false), (2, false), (3, false)]);
    assert_eq!(events.recovered(), [(g0, alice, bob), (g1, carol, dave)]);
    assert_eq!(hosts.pick(Some(1)).map(HostHandle::shard), Some(1), "a shard of the range again");
    let ew = Ep::new(7, carol);
    hosts.get(g1).expect("its host").attach(g1, carol, ew.endpoint());
    until("the snapshot", || got(&ew, MsgType::GameSnapshot)).await;
    assert_eq!(snapshot(ew.last()).moves.len(), 2);
    hosts.shutdown().await;
    store.close().await;
}

/// A journal directory of 0.9.1 has no owner file: its shards are taken (inside or outside the
/// range) and marked with the database's server id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_journal_of_0_9_1_is_recovered_and_marked() {
    let dir = TempDir::new("e2e-upgrade");
    let (store, users) = real_store(&["alice", "bob", "carol", "dave"]).await;
    let [alice, bob, carol, dave] = users[..] else { panic!("four accounts") };
    let server_id = store.server_id().await.expect("read").expect("a server id");
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..3).await.expect("hosts started");
    let g0 = game_on(&hosts, 0, (alice, "alice"), (bob, "bob")).await;
    let g2 = game_on(&hosts, 2, (carol, "carol"), (dave, "dave")).await;
    hosts.shutdown().await;
    // What 0.9.1 left: the same segments, no owner file.
    for shard in 0..3 {
        let owner = dir.path().join(format!("shard-{shard}")).join(OWNER_FILE);
        assert_eq!(std::fs::read_to_string(&owner).expect("an owner file"), format!("{server_id}\n"));
        std::fs::remove_file(owner).expect("removed");
    }

    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..1).await.expect("hosts restarted");
    assert_eq!(shards(&hosts), [(0, false), (2, true)]);
    assert_eq!(events.recovered(), [(g0, alice, bob), (g2, carol, dave)]);
    for shard in [0, 2] {
        let owner = dir.path().join(format!("shard-{shard}")).join(OWNER_FILE);
        assert_eq!(std::fs::read_to_string(owner).expect("marked"), format!("{server_id}\n"));
    }
    assert!(!dir.path().join("shard-1").join(OWNER_FILE).exists(), "an empty shard left out stays as it was");
    hosts.shutdown().await;
    store.close().await;
}

/// The journal of another database is never replayed into this one: inside the range the start
/// is refused, outside it the shard is left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_journal_of_another_database_is_refused_inside_the_range_and_left_alone_outside() {
    let dir = TempDir::new("e2e-foreign");
    let (store, users) = real_store(&["alice", "bob"]).await;
    let [alice, bob] = users[..] else { panic!("two accounts") };
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..2).await.expect("hosts started");
    game_on(&hosts, 1, (alice, "alice"), (bob, "bob")).await;
    hosts.shutdown().await;
    let owner = dir.path().join("shard-1").join(OWNER_FILE);
    std::fs::write(&owner, "another-server\n").expect("written");

    let started = Hosts::start(deps(&dir, &store, &events), 0..2).await;
    let Err(e) = started else { panic!("the start goes on with another database's journal") };
    assert!(
        matches!(&e, HostError::ForeignJournal { shard: 1, owner, games: 1, .. } if owner == "another-server")
    );
    assert!(e.to_string().contains("move JOURNAL_DIR/shard-1 aside"), "{e}");

    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..1).await.expect("hosts started");
    assert_eq!(shards(&hosts), [(0, false)]);
    assert!(events.recovered().is_empty());
    assert_eq!(std::fs::read_to_string(&owner).expect("read"), "another-server\n", "left as it was");
    hosts.shutdown().await;
    store.close().await;
}

/// One process at a time per shard directory: a second server on the same `JOURNAL_DIR` cannot
/// start the shards the first one serves, and never takes over the games of the first one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shard_served_by_another_process_is_never_opened() {
    let dir = TempDir::new("e2e-lock");
    let (store, users) = real_store(&["alice", "bob"]).await;
    let [alice, bob] = users[..] else { panic!("two accounts") };
    let events = Arc::new(RecordingEvents::default());
    let first = Hosts::start(deps(&dir, &store, &events), 0..2).await.expect("hosts started");
    let id = game_on(&first, 1, (alice, "alice"), (bob, "bob")).await;

    let others = Arc::new(RecordingEvents::default());
    let started = Hosts::start(deps(&dir, &store, &others), 0..2).await;
    assert!(matches!(started, Err(HostError::ShardInUse(0))));
    let second = Hosts::start(deps(&dir, &store, &others), 2..3).await.expect("another range starts");
    assert_eq!(shards(&second), [(2, false)], "the first server's shards are left alone");
    assert!(others.recovered().is_empty());
    second.shutdown().await;
    let stats = first.get(id).expect("its host").stats().await.expect("the host runs");
    assert_eq!(stats.active, 1);
    first.shutdown().await;
    // Released at the shutdown.
    let again = Hosts::start(deps(&dir, &store, &others), 0..2).await.expect("hosts restarted");
    assert_eq!(others.recovered(), [(id, alice, bob)]);
    again.shutdown().await;
    store.close().await;
}

// ---- backlog ----------------------------------------------------------------------------------
// On one thread, the actors run only while the test awaits: what it posts meanwhile waits in
// their inboxes, as behind a host that fell behind.

#[tokio::test(flavor = "current_thread")]
async fn a_flood_of_gestures_is_bounded_and_the_messages_behind_it_are_all_handled() {
    let dir = TempDir::new("e2e");
    let (store, users) = real_store(&["alice", "bob"]).await;
    let [alice, bob] = users[..] else { panic!("two accounts") };
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..1).await.expect("hosts started");
    let host = hosts.handles()[0].clone();
    let spec = NewGame { white: shown(alice, "alice"), black: shown(bob, "bob"), ..new_game(0, 0) };
    let id = host.create(spec).await.expect("created");
    let (ew, eb) = (Ep::new(1, alice), Ep::new(2, bob));
    host.attach(id, alice, ew.endpoint());
    host.attach(id, bob, eb.endpoint());
    until("the snapshots", || got(&ew, MsgType::GameSnapshot) && got(&eb, MsgType::GameSnapshot)).await;

    // Far more gestures than the inbox keeps, then a stance, a move, Black's connection replaced
    // and a resignation: none of these is dropped, and they keep their order.
    let gesture = ClientGesture { seq: 1, game: id, ply: 0, touch: 12, aim: 28, ..ClientGesture::default() };
    let gesture = gesture.to_bytes().expect("a valid gesture");
    let extra = 1000;
    for _ in 0..GESTURE_INBOX_MAX + extra {
        host.gesture(id, alice, gesture.clone());
    }
    let stance = ClientStance { seq: 2, game: id, stance: Stance::SideLeft };
    host.stance(id, bob, stance.to_bytes().expect("a valid stance"));
    let mut mirror = ChessGame::default();
    play(&host, id, &mut mirror, &["e2e4"], [(alice, &ew), (bob, &eb)]);
    let eb2 = Ep::new(3, bob);
    host.detach(id, bob, 2);
    host.attach(id, bob, eb2.endpoint());
    host.client(bob, resign(id, 9), eb2.endpoint(), clock::mono_ms());
    assert_eq!(host.inbox(), (GESTURE_INBOX_MAX + 5, GESTURE_INBOX_MAX));
    assert!(!host.busy(), "gestures alone never make a host busy");
    assert_eq!(hosts.pick(None).map(HostHandle::shard), Some(0));

    let stats = host.stats().await.expect("the host runs");
    assert_eq!(host.inbox(), (0, 0));
    let drops = &stats.counters.gesture_drops;
    assert_eq!(drops.get("overload"), Some(&(extra as u64)));
    assert_eq!(
        stats.counters.gestures + drops.get("backlog").copied().unwrap_or(0),
        GESTURE_INBOX_MAX as u64,
        "every gesture let in was relayed (or met the opponent's backlog)"
    );
    assert_eq!((stats.counters.moves, stats.counters.stances), (1, 1));
    assert!(got(&ew, MsgType::ServerStance), "the stance behind the flood");
    let snap = eb2.msgs().into_iter().find_map(|m| match m {
        ServerMsg::GameSnapshot(s) => Some(s),
        _ => None,
    });
    assert_eq!(snap.expect("a snapshot for the new connection").moves.len(), 1, "after the move");
    until("the end", || got(&ew, MsgType::GameEnd) && got(&eb2, MsgType::GameEnd)).await;
    assert!(!got(&eb, MsgType::GameEnd), "the replaced connection is detached");
    until("the commit", || !events.ended().is_empty()).await;
    hosts.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn busy_hosts_take_no_new_game_and_a_saturated_server_refuses_one() {
    let dir = TempDir::new("e2e");
    let (store, users) = real_store(&["alice", "bob"]).await;
    let [alice, bob] = users[..] else { panic!("two accounts") };
    let events = Arc::new(RecordingEvents::default());
    let hosts = Hosts::start(deps(&dir, &store, &events), 0..2).await.expect("hosts started");
    let spec = || NewGame { white: shown(alice, "alice"), black: shown(bob, "bob"), ..new_game(0, 0) };
    // Cheap messages that must all be handled (no game: nothing happens).
    let flood = |h: &HostHandle| {
        for _ in 0..INBOX_BUSY {
            h.rtt(0, alice, 50);
        }
    };

    // Host 1 falls behind: the game it would have taken goes to host 0.
    flood(&hosts.handles()[1]);
    assert!(hosts.handles()[1].busy());
    assert_eq!(hosts.pick(Some(1)).map(HostHandle::shard), Some(0));
    assert!(!hosts.saturated());
    // Host 0 too: no new game, and the realtime layer answers RateLimited.
    flood(&hosts.handles()[0]);
    assert_eq!(hosts.pick(None).map(HostHandle::shard), None);
    assert!(hosts.saturated());
    assert_eq!(GameHosts::create(&hosts, None, spec()).await, Err(ErrorCode::RateLimited));
    for h in hosts.handles() {
        h.stats().await.expect("the host runs");
        assert_eq!(h.inbox(), (0, 0));
    }
    assert!(!hosts.saturated(), "caught up");

    // A journal whose records wait for a stuck disk makes its host busy too (until a beat
    // publishes the real figure).
    hosts.handles()[0].shared.backlog.set_journal_pending(JOURNAL_PENDING_BUSY);
    assert_eq!(hosts.pick(Some(0)).map(HostHandle::shard), Some(1));

    // A database writer far behind refuses new games on every host.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let blocker = store.write(move |_| {
        let _ = held.recv();
        Ok::<_, StoreError>(())
    });
    let jobs: Vec<_> = (0..WRITE_BACKLOG_BUSY).map(|_| store.write(|_| Ok::<_, StoreError>(()))).collect();
    assert!(store.write_backlog() >= WRITE_BACKLOG_BUSY && store.writes_backlogged());
    assert!(hosts.saturated());
    assert_eq!(hosts.place(None).map(HostHandle::shard), None);
    release.send(()).expect("the writer waits");
    blocker.await.expect("written");
    for job in jobs {
        job.await.expect("written");
    }
    assert_eq!(store.write_backlog(), 0);
    let id = GameHosts::create(&hosts, None, spec()).await.expect("created");
    assert!(ids::is_game_id(id));
    hosts.shutdown().await;
}
