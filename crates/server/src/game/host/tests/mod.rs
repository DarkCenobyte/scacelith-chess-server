//! Host tests, ported from the reference `game.host` suite (the stall, persistence, recovery and
//! end-to-end ones are in the submodules), and the rig they share: a [`Shard`] driven by hand
//! (no actor, no beat unless a test calls [`Shard::heartbeat`]) on a manual clock, the scripted
//! rules, a recording lobby and anti-cheat, the store double and a real journal in a temporary
//! directory that writes only when flushed.

mod e2e;
mod persistence;
mod recovery;
mod stall;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use parking_lot::Mutex;
use scacelith_protocol::{
    Abort, ClientGesture, ClientMsg, Color, DrawOffer, EndReason as ER, ErrorCode as EC, GameEventKind as EV,
    GameSnapshot, GameStatus as GS, Message, Move, MsgType, PlayerInfo, QueueJoin, Rematch, Resign, Resync,
    ServerMsg, close,
};

use super::{GameStore, RulesFactory, Shard, ShardDeps, ShardSettings};
use crate::clock::{Clock, ManualClock};
use crate::config::{Config, test_config};
use crate::events::{GameEnded, IncidentKind, NewGame};
use crate::game::room::{GameRoom, NEVER, REMATCH_WINDOW_MS, RecordKind, RoomSettings, record_flag};
use crate::game::rules::{Played, Rules, Side};
use crate::game::testing::{FakeRules, FakeStore, RecordingEvents, RematchPolicy, Script, fake_move};
use crate::ids::{self, ConnId, GameId, UserId};
use crate::journal::{Journal, JournalOptions};
use crate::log::Logger;
use crate::realtime::Endpoint;
use crate::realtime::endpoint::{CloseRequest, Outbound};

pub(super) const T0: i64 = 1_800_000_000_000;
pub(super) const SHARD: u32 = 3;
pub(super) const W: Side = Side::White;
pub(super) const B: Side = Side::Black;

/// A directory removed when dropped.
#[derive(Debug)]
pub(super) struct TempDir(PathBuf);

impl TempDir {
    pub(super) fn new(tag: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "scacelith-host-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temporary directory");
        TempDir(dir)
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copies a directory tree (a crash copy of a journal).
pub(super) fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("copy target");
    for entry in std::fs::read_dir(from).expect("readable directory") {
        let entry = entry.expect("directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// A player of the tests: `user<id>`, rated 1500 + id.
pub(super) fn player(id: UserId) -> PlayerInfo {
    PlayerInfo { user_id: id, name: format!("user{id}"), rating: 1500 + id as u16, provisional: false }
}

/// A 3+2 rated game between two players.
pub(super) fn new_game(white: UserId, black: UserId) -> NewGame {
    NewGame {
        category: "3+2".to_owned(),
        base_ms: 180_000,
        inc_ms: 2000,
        rated: true,
        white: player(white),
        black: player(black),
        created_at: 0,
        rematch_of: None,
        auto_press: true,
    }
}

/// The journal options of the tests: only explicit flushes write, no fsync.
pub(super) fn journal_options(dir: &Path, shard: u32, clock: &Arc<ManualClock>) -> JournalOptions {
    let mut o = JournalOptions::new(dir, shard);
    o.flush_ms = 3_600_000;
    o.fsync = false;
    o.clock = clock.clone();
    o
}

/// Where a rig's journal lives.
pub(super) enum JournalAt {
    /// No journal: nothing journaled, commits do not wait.
    None,
    /// A new temporary directory.
    Temp,
    /// An existing directory (a restart).
    Dir(PathBuf),
}

/// How to build a rig.
pub(super) struct Opts {
    pub config: Vec<(&'static str, &'static str)>,
    pub script: Script,
    pub journal: JournalAt,
    /// Changes the journal options (segment size, compaction).
    pub journal_tweak: fn(&mut JournalOptions),
    pub t: i64,
    pub last_game_id: GameId,
    pub shard: u32,
    pub rules: Option<RulesFactory>,
    pub store: Option<Arc<dyn GameStore>>,
    pub events: Option<Arc<RecordingEvents>>,
    pub logger: Option<Logger>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            config: Vec::new(),
            script: Script::default(),
            journal: JournalAt::Temp,
            journal_tweak: |_| {},
            t: T0,
            last_game_id: 0,
            shard: SHARD,
            rules: None,
            store: None,
            events: None,
            logger: None,
        }
    }
}

/// A host driven by hand (see the module documentation).
pub(super) struct Rig {
    pub shard: Shard,
    pub clock: Arc<ManualClock>,
    pub events: Arc<RecordingEvents>,
    /// The store double (unused when the options gave another store).
    pub store: Arc<FakeStore>,
    pub config: Config,
    /// The journal's root directory (`shard-<n>` is under it).
    pub dir: Option<PathBuf>,
    temp: Option<TempDir>,
}

impl Rig {
    pub(super) async fn new() -> Rig {
        Rig::with(Opts::default()).await
    }

    pub(super) async fn with(o: Opts) -> Rig {
        let config = test_config(&o.config).expect("test configuration");
        let clock = ManualClock::new(o.t as f64, o.t);
        let (temp, dir) = match o.journal {
            JournalAt::None => (None, None),
            JournalAt::Temp => {
                let temp = TempDir::new("journal");
                let dir = temp.path().to_owned();
                (Some(temp), Some(dir))
            }
            JournalAt::Dir(dir) => (None, Some(dir)),
        };
        let journal = match &dir {
            Some(dir) => {
                let mut options = journal_options(dir, o.shard, &clock);
                (o.journal_tweak)(&mut options);
                Some(Journal::open(options).await.expect("journal"))
            }
            None => None,
        };
        let fake = Arc::new(FakeStore::new());
        let store: Arc<dyn GameStore> = o.store.unwrap_or_else(|| fake.clone());
        let events = o.events.unwrap_or_default();
        let script = o.script;
        let rules: RulesFactory =
            o.rules.unwrap_or_else(|| Arc::new(move || FakeRules::boxed(script.clone())));
        let shard = Shard::new(ShardDeps {
            shard: o.shard,
            settings: ShardSettings::from_config(&config),
            clock: clock.clone(),
            store,
            journal,
            events: events.clone(),
            anomalies: events.clone(),
            rules,
            logger: o.logger.unwrap_or_else(|| Logger::root().child("game")),
            last_game_id: o.last_game_id,
        });
        Rig { shard, clock, events, store: fake, config, dir, temp }
    }

    /// The clock (monotonic ms).
    pub(super) fn t(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Sets both clocks.
    pub(super) fn set(&self, t: i64) {
        self.clock.set_mono(t as f64);
        self.clock.set_wall(t);
    }

    pub(super) fn advance(&self, dt: i64) {
        self.set(self.t() + dt);
    }

    pub(super) fn journal(&self) -> &Journal {
        self.shard.journal().expect("a journal")
    }

    pub(super) fn settings(&self) -> RoomSettings {
        RoomSettings::from_config(&self.config)
    }

    pub(super) fn create(&mut self, game: NewGame) -> GameId {
        self.shard.create(game).expect("game created")
    }

    pub(super) fn new_game(&mut self, white: UserId, black: UserId) -> GameId {
        self.create(new_game(white, black))
    }

    pub(super) fn room(&self, game: GameId) -> &GameRoom {
        self.shard.room(game).expect("hosted game")
    }

    pub(super) fn user(&self, game: GameId, side: Side) -> UserId {
        self.room(game).player(side).user_id
    }

    /// A request of `user`, read now.
    pub(super) fn send(&mut self, user: UserId, msg: ClientMsg, ep: Option<&Ep>) {
        let t = self.t() as f64;
        self.shard.client(user, &msg, ep.map(Ep::endpoint), t);
    }

    /// The side to move plays the next scripted move now.
    pub(super) fn move_now(&mut self, game: GameId, ep: Option<&Ep>) {
        let side = self.room(game).side_to_move();
        let msg = ClientMsg::Move(move_msg(self.room(game), 5));
        self.send(self.user(game, side), msg, ep);
    }

    /// `dt` later, the side to move plays the next scripted move (no endpoint).
    pub(super) fn play(&mut self, game: GameId, dt: i64) {
        self.advance(dt);
        self.move_now(game, None);
    }

    /// White resigns now.
    pub(super) fn resign_white(&mut self, game: GameId) {
        let white = self.user(game, W);
        self.send(white, resign(game, 2), None);
    }

    /// Fires the timers due at `t`.
    pub(super) fn run_timers(&mut self, t: i64) -> usize {
        self.shard.run_timers(t, NEVER, t)
    }

    /// At `t`, starts the commit due then and waits for its result (`None`: none was due).
    pub(super) async fn poll(&mut self, t: i64) -> Option<bool> {
        self.set(t);
        if !self.shard.poll_commits(t) {
            return None;
        }
        Some(self.shard.settle_commit().await)
    }

    /// The stall credit of a request handled at `t`.
    pub(super) fn credit(&self, t: i64) -> i64 {
        self.shard.stall_credit(t, self.shard.stall_start(t))
    }

    /// One beat at `t` (the clock is set to `t`).
    pub(super) fn beat(&mut self, t: i64) -> bool {
        self.set(t);
        self.shard.heartbeat(t)
    }

    /// What the actor does once it read its inbox after a detected stall.
    pub(super) fn drain(&mut self) {
        if self.shard.stall_pending() {
            self.shard.after_stall();
        }
    }

    /// The deadline of a game's timer.
    pub(super) fn deadline(&self, game: GameId) -> i64 {
        self.shard.deadline_of(game).expect("a deadline")
    }

    /// The kinds of the records appended for a game.
    pub(super) fn journal_kinds(&self, game: GameId) -> Vec<RecordKind> {
        self.shard.journal_log.iter().filter(|(g, _)| *g == game).map(|(_, r)| r.kind).collect()
    }

    /// Whether the game's `committed` record was appended.
    pub(super) fn journal_committed(&self, game: GameId) -> bool {
        self.journal_kinds(game).contains(&RecordKind::Committed)
    }

    /// The process dies: the journal is written and closed (nothing else is), and its directory is
    /// handed over with the guard of the temporary one.
    pub(super) async fn crash(mut self) -> (PathBuf, Option<TempDir>) {
        if let Some(journal) = self.shard.journal.take() {
            journal.flush().await.expect("journal flushed");
            journal.close().await.expect("journal closed");
        }
        self.shard.tasks.abort_all();
        (self.dir.take().expect("a journal directory"), self.temp.take())
    }
}

/// A player's connection for the tests: the frames queued are kept until cleared.
#[derive(Clone)]
pub(super) struct Ep {
    ep: Endpoint,
    out: Arc<Outbound>,
    sent: Arc<Mutex<Vec<Bytes>>>,
    closed: Arc<Mutex<Option<CloseRequest>>>,
}

impl Ep {
    pub(super) fn new(conn: ConnId, user: UserId) -> Ep {
        Ep::with_limit(conn, user, 1 << 20)
    }

    /// A connection whose queue holds `limit` bytes (droppable frames are refused above a quarter
    /// of it).
    pub(super) fn with_limit(conn: ConnId, user: UserId, limit: usize) -> Ep {
        let out = Outbound::new(limit);
        Ep {
            ep: Endpoint::new(conn, user, out.clone()),
            out,
            sent: Arc::new(Mutex::new(Vec::new())),
            closed: Arc::new(Mutex::new(None)),
        }
    }

    pub(super) fn endpoint(&self) -> Endpoint {
        self.ep.clone()
    }

    fn pull(&self) {
        let batch = self.out.take();
        self.out.written(batch.frames.iter().map(Bytes::len).sum());
        self.sent.lock().extend(batch.frames);
        if let Some(c) = batch.close {
            *self.closed.lock() = Some(c);
        }
    }

    /// The writer stops taking frames: `bytes` wait in the queue until [`Ep::unblock`].
    pub(super) fn block(&self, bytes: usize) {
        self.pull();
        self.out.send(Bytes::from(vec![0u8; bytes]));
    }

    /// The writer takes the queue again (the filler of [`Ep::block`] is not kept).
    pub(super) fn unblock(&self) {
        let batch = self.out.take();
        self.out.written(batch.frames.iter().map(Bytes::len).sum());
        let frames = batch.frames.into_iter().filter(|f| f.iter().any(|&b| b != 0));
        self.sent.lock().extend(frames);
    }

    pub(super) fn sent(&self) -> Vec<Bytes> {
        self.pull();
        self.sent.lock().clone()
    }

    pub(super) fn msgs(&self) -> Vec<ServerMsg> {
        self.sent().iter().map(|f| ServerMsg::decode_exact(f).expect("a valid server frame")).collect()
    }

    pub(super) fn types(&self) -> Vec<MsgType> {
        self.msgs().iter().map(ServerMsg::msg_type).collect()
    }

    pub(super) fn last(&self) -> ServerMsg {
        self.msgs().pop().expect("a frame")
    }

    pub(super) fn clear(&self) {
        self.pull();
        self.sent.lock().clear();
    }

    pub(super) fn closed(&self) -> Option<CloseRequest> {
        self.pull();
        self.closed.lock().clone()
    }
}

/// The next scripted move of a game.
pub(super) fn move_msg(room: &GameRoom, seq: u32) -> Move {
    Move {
        seq,
        game: room.id(),
        ply: room.ply() as u16,
        r#move: fake_move(room.ply(), 0),
        pos_hash: room.digest(),
        think_ms: 0,
        draw_offer: false,
    }
}

pub(super) fn resign(game: GameId, seq: u32) -> ClientMsg {
    ClientMsg::Resign(Resign { seq, game })
}

pub(super) fn abort(game: GameId, seq: u32) -> ClientMsg {
    ClientMsg::Abort(Abort { seq, game })
}

pub(super) fn resync(game: GameId, seq: u32) -> ClientMsg {
    ClientMsg::Resync(Resync { seq, game })
}

pub(super) fn rematch(game: GameId, seq: u32, accept: bool) -> ClientMsg {
    ClientMsg::Rematch(Rematch { seq, game, accept })
}

pub(super) fn snapshot(msg: ServerMsg) -> GameSnapshot {
    match msg {
        ServerMsg::GameSnapshot(s) => s,
        other => panic!("not a GameSnapshot: {other:?}"),
    }
}

pub(super) fn error(msg: &ServerMsg) -> (EC, bool, u32) {
    match msg {
        ServerMsg::Error(e) => (e.code, e.fatal, e.r#ref),
        other => panic!("not an Error: {other:?}"),
    }
}

pub(super) fn game_event(msg: &ServerMsg) -> (EV, Color, u32) {
    match msg {
        ServerMsg::GameEvent(e) => (e.kind, e.color, e.arg),
        other => panic!("not a GameEvent: {other:?}"),
    }
}

pub(super) fn result(room: &GameRoom) -> (GS, ER) {
    room.result().map(|r| (r.status, r.reason)).expect("a finished game")
}

pub(super) fn ended_at(room: &GameRoom) -> i64 {
    room.result().map(|r| r.ended_at).expect("a finished game")
}

/// An I/O error for the fault injection.
pub(super) fn eio() -> io::Error {
    io::Error::other("EIO: i/o error, write")
}

/// A `C_Gesture` frame.
fn gesture(seq: u32, game: GameId, yaw: i32) -> Bytes {
    ClientGesture { seq, game, ply: 2, touch: 12, aim: 28, placed: 0, flags: 5, yaw, pitch: 321, lean: 40 }
        .to_bytes()
        .expect("a valid gesture")
}

#[tokio::test]
async fn create_attach_snapshot_moves_broadcast_as_one_buffer_journal_appends_and_counters() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    assert_eq!(ids::shard_of(id), SHARD);
    assert_eq!(h.shard.active_game_of(1), Some(id));
    let (ew, eb) = (Ep::new(10, 1), Ep::new(20, 2));
    assert!(h.shard.attach(id, 1, ew.endpoint()));
    assert!(h.shard.attach(id, 2, eb.endpoint()));
    let s = snapshot(ew.last());
    assert_eq!((s.you, s.category.as_str(), s.rated, s.first_move_ms), (Color::White, "3+2", true, 30000));
    assert_eq!(snapshot(eb.last()).you, Color::Black);
    h.advance(1500);
    h.move_now(id, Some(&ew));
    let (fw, fb) = (ew.sent().pop().expect("a frame"), eb.sent().pop().expect("a frame"));
    assert_eq!(fw.as_ptr(), fb.as_ptr(), "the same buffer for both players");
    assert_eq!(ew.last().msg_type(), MsgType::MoveMade);
    assert_eq!(h.journal_kinds(id), [RecordKind::Created, RecordKind::Move]);
    let c = h.shard.counters();
    assert_eq!((c.moves, c.moves_timed, c.created), (1, 1, 1));
    assert_eq!(h.shard.active(), 1);
    assert_eq!(h.deadline(id), h.t() + 30000 + 150);
}

#[tokio::test]
async fn only_an_official_category_is_rated() {
    let mut u = 0;
    for (overrides, official) in [(vec![], "3+2"), (vec![("RATED_CATEGORIES", "4+4,1+0")], "4+4")] {
        let mut h = Rig::with(Opts { config: overrides, ..Opts::default() }).await;
        for (category, rated) in [(official, true), ("custom", false), ("9+9", false), ("", false)] {
            u += 2;
            let id = h.create(NewGame { category: category.to_owned(), ..new_game(u, u + 1) });
            assert_eq!(h.room(id).rated(), rated, "{category}");
        }
        assert_eq!(h.room(h.shard.active_game_of(u).expect("a game")).category(), "custom");
    }
}

#[tokio::test]
async fn unknown_games_and_unroutable_messages_get_not_in_game() {
    let mut h = Rig::new().await;
    let ep = Ep::new(1, 1);
    h.send(1, resign(424242, 8), Some(&ep));
    assert_eq!(error(&ep.last()), (EC::NotInGame, false, 8));
    assert!(h.events.anomalies().is_empty());
    h.new_game(1, 2);
    h.send(1, ClientMsg::QueueJoin(QueueJoin { seq: 9, category: "3+2".into(), rated: true }), Some(&ep));
    assert_eq!(error(&ep.last()), (EC::NotInGame, false, 9));
    assert!(!h.shard.attach(424242, 1, ep.endpoint()));
    assert_eq!(error(&ep.last()), (EC::NotInGame, false, 0));
    assert_eq!(h.shard.counters().rejects.get("NotInGame"), Some(&2));
}

#[tokio::test]
async fn a_game_message_from_a_non_player_is_foreign_game_sanctioned_and_the_game_goes_on() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let ep = Ep::new(77, 99);
    h.send(99, resign(id, 3), Some(&ep));
    let errors: Vec<_> = ep.msgs().iter().map(error).collect();
    assert_eq!(errors, [(EC::NotInGame, false, 3), (EC::CheatDetected, true, 3)]);
    let a = h.events.anomalies();
    assert_eq!(a.iter().map(|a| (a.user, a.game, a.kind)).collect::<Vec<_>>(), [(99, id, "foreign_game")]);
    assert_eq!(
        a[0].detail,
        serde_json::json!({ "info": format!("message type {}", MsgType::Resign.to_u8()) })
    );
    assert_eq!(h.events.sanctions(), [(99, id, "foreign_game", 77)], "the connection it came from");
    assert_eq!(ep.closed().map(|c| c.code), Some(close::CHEAT_DETECTED));
    assert!(!h.room(id).is_over());
}

#[tokio::test]
async fn a_certain_cheat_forfeits_the_game_when_auto_sanction_is_on() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let (ew, eb) = (Ep::new(10, 1), Ep::new(20, 2));
    h.shard.attach(id, 1, ew.endpoint());
    h.shard.attach(id, 2, eb.endpoint());
    h.play(id, 1000);
    ew.clear();
    eb.clear();
    h.advance(100);
    // White again: out of turn, in the position the server has.
    let msg = ClientMsg::Move(move_msg(h.room(id), 31));
    h.send(1, msg, Some(&ew));
    assert_eq!(ew.types(), [MsgType::MoveRejected, MsgType::GameSnapshot, MsgType::GameEnd, MsgType::Error]);
    assert_eq!(error(&ew.last()), (EC::CheatDetected, true, 31));
    assert_eq!(eb.types(), [MsgType::GameEnd]);
    assert_eq!(result(h.room(id)), (GS::BlackWins, ER::Forfeit));
    assert_eq!(ew.closed().map(|c| c.code), Some(close::CHEAT_DETECTED));
    assert_eq!(h.events.sanctions(), [(1, id, "out_of_turn", 10)]);
    let c = h.shard.counters();
    assert_eq!(c.rejects.get("NotYourTurn"), Some(&1));
    assert_eq!(c.ended_by_reason.get("Forfeit"), Some(&1));
    assert_eq!(h.shard.active_game_of(1), None);
}

#[tokio::test]
async fn without_auto_sanction_or_for_uncertain_anomalies_the_anomaly_is_only_recorded() {
    let mut h =
        Rig::with(Opts { config: vec![("AUTO_SANCTION_CERTAIN_CHEATS", "0")], ..Opts::default() }).await;
    let id = h.new_game(1, 2);
    let ew = Ep::new(10, 1);
    h.shard.attach(id, 1, ew.endpoint());
    h.play(id, 1000);
    let msg = ClientMsg::Move(move_msg(h.room(id), 5));
    h.send(1, msg, Some(&ew));
    let stale = Move { ply: 0, r#move: fake_move(9, 0), ..move_msg(h.room(id), 6) };
    h.send(1, ClientMsg::Move(stale), Some(&ew));
    let kinds: Vec<_> = h.events.anomalies().iter().map(|a| a.kind).collect();
    assert_eq!(kinds, ["out_of_turn", "stale_ply"]);
    assert!(h.events.sanctions().is_empty());
    assert!(!h.room(id).is_over());
    assert_eq!(ew.closed(), None);
}

#[tokio::test]
async fn detach_and_attach_disconnection_events_stale_detach_ignored_snapshot_on_reconnection() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let (ew, eb) = (Ep::new(10, 1), Ep::new(20, 2));
    h.shard.attach(id, 1, ew.endpoint());
    h.shard.attach(id, 2, eb.endpoint());
    h.play(id, 1000);
    h.play(id, 1000);
    eb.clear();
    let ew2 = Ep::new(11, 1);
    h.shard.attach(id, 1, ew2.endpoint()); // a new connection replaced the old one
    assert!(!h.shard.detach(id, 1, Some(10)), "the old connection closing is ignored");
    assert!(eb.sent().is_empty());
    h.advance(1000);
    assert!(h.shard.detach(id, 1, Some(11)));
    assert_eq!(game_event(&eb.last()), (EV::PlayerDisconnected, Color::White, 18000));
    h.advance(5000);
    let ew3 = Ep::new(12, 1);
    h.shard.attach(id, 1, ew3.endpoint());
    let ServerMsg::GameEvent(back) = eb.last() else { panic!("not a GameEvent: {:?}", eb.last()) };
    assert_eq!(back.kind, EV::PlayerReconnected);
    // The returning player gets its snapshot alone: it holds the PlayerReconnected (its gseq).
    let m = ew3.msgs();
    assert_eq!(m.len(), 1, "{m:?}");
    let s = snapshot(m[0].clone());
    assert_eq!((s.white_connected, s.running, s.white_ms), (true, Color::White, 180000 - 6000));
    assert_eq!(s.gseq, back.gseq);
    assert_eq!(s.gseq, h.room(id).gseq());
}

#[tokio::test]
async fn grace_expiry_through_the_timers_is_an_abandonment_and_a_conduct_incident() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let eb = Ep::new(20, 2);
    h.shard.attach(id, 2, eb.endpoint());
    h.shard.attach(id, 1, Ep::new(10, 1).endpoint());
    h.play(id, 1000);
    h.play(id, 1000);
    h.shard.detach(id, 1, None);
    let t = h.t();
    h.run_timers(t + 18000 - 1);
    assert!(!h.room(id).is_over());
    h.run_timers(t + 18000 + 9);
    assert_eq!(result(h.room(id)), (GS::BlackWins, ER::Abandonment));
    assert_eq!(ended_at(h.room(id)), t + 18000 + 9);
    assert_eq!(eb.last().msg_type(), MsgType::GameEnd);
    assert_eq!(h.events.conduct(), [(1, IncidentKind::Abandon)]);
}

#[tokio::test]
async fn timers_under_10k_rooms_every_deadline_fires_once_never_early() {
    let mut h = Rig::with(Opts { journal: JournalAt::None, ..Opts::default() }).await;
    const N: usize = 10_000;
    let mut expected = Vec::with_capacity(N);
    for i in 0..N {
        h.set(T0 + i as i64 * 3);
        let base = 15000 + (i as u32 % 7) * 1000;
        let u = 2 * i as UserId;
        let id = h.create(NewGame { base_ms: base, inc_ms: 0, rated: false, ..new_game(u + 1, u + 2) });
        match i % 3 {
            0 => {
                // Both first moves: White's clock runs out.
                h.play(id, 1);
                h.play(id, 1);
                expected.push((id, h.t() + i64::from(base) + 150, ER::Timeout));
            }
            // White never moves (first-move margin: 150).
            1 => expected.push((id, h.t() + 30000 + 150, ER::NoShow)),
            _ => {
                // Black never moves.
                h.play(id, 1);
                expected.push((id, h.t() + 30000 + 150, ER::NoShow));
            }
        }
    }
    assert_eq!(h.shard.timers(), N);
    let mut fired = 0;
    let mut at = T0;
    while at <= T0 + 61000 {
        h.set(at);
        fired += h.run_timers(at);
        at += 10;
    }
    assert_eq!(fired, N, "one firing per room");
    for (id, deadline, reason) in expected {
        let room = h.room(id);
        assert_eq!(result(room).1, reason);
        let late = ended_at(room) - deadline;
        assert!((0..10).contains(&late), "game {id} ended {late} ms after its deadline");
    }
    assert_eq!(h.shard.active(), 0);
    // Commit everything, then let the rematch windows expire: every room goes.
    while h.shard.pending_commits() > 0 {
        let t = h.t() + 100;
        assert_eq!(h.poll(t).await, Some(true));
    }
    let end = h.t() + REMATCH_WINDOW_MS + 100;
    while at <= end {
        h.set(at);
        h.run_timers(at);
        at += 10;
    }
    assert_eq!(h.shard.games(), 0);
    assert_eq!(h.shard.timers(), 0);
}

#[tokio::test]
async fn moves_push_the_deadline_and_round_trips_move_it() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    h.play(id, 1000);
    h.play(id, 1000);
    let t1 = h.t();
    assert_eq!(h.deadline(id), t1 + 180000 + 150);
    h.shard.rtt(id, 1, 600);
    assert_eq!(h.deadline(id), t1 + 180000 + 650);
    h.play(id, 500);
    assert_eq!(h.deadline(id), h.t() + 180000 + 150);
}

#[tokio::test]
async fn finished_games_are_committed_in_one_batch_then_rating_update_committed_record_and_game_ended() {
    let mut h = Rig::new().await;
    let mut games = Vec::new();
    for i in 0..5 {
        let (w, b) = (10 + 2 * i, 11 + 2 * i);
        let id = h.new_game(w, b);
        let (ew, eb) = (Ep::new(100 + i, w), Ep::new(200 + i, b));
        h.shard.attach(id, w, ew.endpoint());
        h.shard.attach(id, b, eb.endpoint());
        h.play(id, 10);
        h.play(id, 10);
        games.push((id, w, ew, eb));
    }
    let t_end = h.t();
    for (i, (id, w, ew, _)) in games.iter().enumerate() {
        h.set(t_end + i as i64 * 5);
        h.send(*w, resign(*id, 2), Some(ew));
    }
    let ids: Vec<GameId> = games.iter().map(|g| g.0).collect();
    assert_eq!(h.shard.pending_commits(), 5);
    assert_eq!(h.poll(t_end + 49).await, None);
    assert!(h.store.batches().is_empty());
    assert_eq!(h.poll(t_end + 50).await, Some(true));
    let batches = h.store.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].iter().map(|r| r.id).collect::<Vec<_>>(), ids);
    let rec = &batches[0][0];
    assert!(rec.rated);
    let lens = (rec.moves.len(), rec.spent_ms.as_ref().map(Vec::len), rec.clock_ms.as_ref().map(Vec::len));
    assert_eq!(lens, (2, Some(2), Some(2)));
    for (_, _, ew, eb) in &games {
        let (fw, fb) = (ew.sent().pop().expect("a frame"), eb.sent().pop().expect("a frame"));
        assert_eq!(fw.as_ptr(), fb.as_ptr(), "the same buffer for both players");
        match ew.last() {
            ServerMsg::RatingUpdate(u) => {
                assert_eq!((u.white.after, u.category.as_str()), (u.white.before + 8, "3+2"));
            }
            other => panic!("not a RatingUpdate: {other:?}"),
        }
    }
    for id in &ids {
        assert!(h.journal_committed(*id));
    }
    let ended = h.events.ended();
    assert_eq!(ended.len(), 5);
    let first = GameEnded {
        game: ids[0],
        white: 10,
        black: 11,
        status: GS::BlackWins,
        reason: ER::Resignation,
        rated: true,
        category: "3+2".into(),
    };
    assert_eq!(ended[0], first);
    assert_eq!(h.shard.counters().commit_batches, 1);
    assert_eq!(h.shard.pending_commits(), 0);
    // The rooms stay until the rematch window closes.
    assert!(h.shard.room(ids[0]).is_some());
    h.run_timers(t_end + REMATCH_WINDOW_MS + 30);
    assert!(h.shard.room(ids[0]).is_none());
}

#[tokio::test]
async fn a_failed_commit_is_retried_with_backoff_while_the_journal_keeps_the_game() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    h.play(id, 1000);
    h.play(id, 1000);
    h.send(2, resign(id, 2), None);
    let t = h.t();
    h.store.fail_next(2);
    assert_eq!(h.poll(t + 50).await, Some(false)); // retry in 100 ms
    assert!(!h.journal_committed(id));
    assert_eq!(h.poll(t + 149).await, None);
    assert_eq!(h.store.failures_left(), 1, "no attempt during the backoff");
    assert_eq!(h.poll(t + 150).await, Some(false)); // retry in 200 ms
    assert_eq!(h.poll(t + 349).await, None);
    assert!(h.store.batches().is_empty());
    assert_eq!(h.poll(t + 350).await, Some(true));
    assert_eq!(h.store.committed_ids(), [id]);
    assert!(h.journal_committed(id));
    assert_eq!(h.shard.counters().commit_errors, 2);
    assert_eq!(h.shard.backoff_ms(), 0);
}

#[tokio::test]
async fn one_commit_in_flight_at_a_time() {
    let mut h = Rig::new().await;
    h.store.fail_next(1);
    let (a, b) = (h.new_game(1, 2), h.new_game(3, 4));
    h.send(1, abort(a, 2), None);
    let t = h.t();
    assert_eq!(h.poll(t + 50).await, Some(false)); // retry in 100 ms
    h.send(3, abort(b, 2), None);
    assert_eq!(h.poll(t + 100).await, None, "still backing off");
    h.set(t + 150);
    assert!(h.shard.poll_commits(t + 150));
    assert!(!h.shard.poll_commits(t + 151), "in flight");
    assert!(h.shard.settle_commit().await);
    let mut ids = h.store.committed_ids();
    ids.sort_unstable();
    assert_eq!(ids, [a, b]);
    assert!(h.journal_committed(a) && h.journal_committed(b));
    assert!(!h.store.batches()[0][0].rated, "aborted games are unrated");
}

#[tokio::test]
async fn one_bad_record_does_not_block_the_batch_the_others_are_committed_one_by_one() {
    let mut h = Rig::new().await;
    let ids = [h.new_game(1, 2), h.new_game(3, 4), h.new_game(5, 6)];
    for id in ids {
        h.play(id, 1000);
        h.play(id, 1000);
        let black = h.user(id, B);
        h.send(black, resign(id, 2), None);
    }
    h.store.refuse(ids[1]);
    let t = h.t() + 100;
    assert_eq!(h.poll(t).await, Some(false));
    assert!(!h.shard.commit_in_flight());
    let mut committed = h.store.committed_ids();
    committed.sort_unstable();
    assert_eq!(committed, [ids[0], ids[2]]);
    assert!(h.journal_committed(ids[0]) && h.journal_committed(ids[2]));
    assert!(!h.journal_committed(ids[1]), "the bad game stays in the journal");
    assert_eq!(h.shard.pending_commits(), 1);
}

#[tokio::test]
async fn the_rematch_agreement_goes_to_the_lobby_with_colours_swapped_and_its_refusal_to_both_players() {
    let events = Arc::new(RecordingEvents::with_rematch(RematchPolicy::Refuse(EC::UserUnavailable)));
    let mut h = Rig::with(Opts { events: Some(events), ..Opts::default() }).await;
    let id = h.new_game(1, 2);
    let (ew, eb) = (Ep::new(1, 1), Ep::new(2, 2));
    h.shard.attach(id, 1, ew.endpoint());
    h.shard.attach(id, 2, eb.endpoint());
    h.play(id, 1000);
    h.play(id, 1000);
    h.send(1, resign(id, 3), Some(&ew));
    h.send(2, rematch(id, 4, true), Some(&eb));
    assert_eq!(game_event(&ew.last()).0, EV::RematchOffered);
    h.send(1, rematch(id, 5, true), Some(&ew));
    let req = h.events.rematches().pop().expect("a rematch request").game;
    assert_eq!(
        (
            req.rematch_of,
            req.white.user_id,
            req.black.user_id,
            req.base_ms,
            req.inc_ms,
            req.rated,
            req.category.as_str()
        ),
        (Some(id), 2, 1, 180000, 2000, true, "3+2")
    );
    h.shard.drain_tasks().await;
    // The lobby's code goes to both players.
    assert_eq!(error(&ew.last()), (EC::UserUnavailable, false, 0));
    assert_eq!(error(&eb.last()), (EC::UserUnavailable, false, 0));
}

#[tokio::test]
async fn an_accepted_rematch_sends_nothing_more_a_dropped_answer_is_rematch_unavailable() {
    for (policy, refused) in [(RematchPolicy::Accept(77), false), (RematchPolicy::Drop, true)] {
        let events = Arc::new(RecordingEvents::with_rematch(policy));
        let mut h = Rig::with(Opts { events: Some(events), ..Opts::default() }).await;
        let id = h.new_game(1, 2);
        let ew = Ep::new(1, 1);
        h.shard.attach(id, 1, ew.endpoint());
        h.play(id, 1000);
        h.play(id, 1000);
        h.send(1, resign(id, 3), Some(&ew));
        h.send(2, rematch(id, 4, true), None);
        h.send(1, rematch(id, 5, true), Some(&ew));
        h.shard.drain_tasks().await;
        let last = ew.last();
        assert_eq!(matches!(last, ServerMsg::Error(_)), refused, "{policy:?}");
        if refused {
            assert_eq!(error(&last), (EC::RematchUnavailable, false, 0));
        }
    }
}

#[tokio::test]
async fn decline_rematch_closes_the_window_and_counts_no_refusal_whatever_the_state() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let (ew, eb) = (Ep::new(1, 1), Ep::new(2, 2));
    h.shard.attach(id, 1, ew.endpoint());
    h.shard.attach(id, 2, eb.endpoint());
    h.play(id, 1000);
    h.play(id, 1000);
    h.shard.decline_rematch(id, 1); // still running
    assert!(!h.room(id).is_over());
    h.send(1, resign(id, 3), Some(&ew));
    ew.clear();
    eb.clear();
    h.shard.decline_rematch(id, 1);
    assert_eq!(eb.msgs().iter().map(|m| game_event(m).0).collect::<Vec<_>>(), [EV::RematchDeclined]);
    assert_eq!(ew.sent().len(), 1, "only the broadcast");
    assert!(!h.room(id).rematch_open());
    h.shard.decline_rematch(id, 2); // window closed
    let t = h.t();
    assert_eq!(h.poll(t + 1000).await, Some(true));
    h.run_timers(t + REMATCH_WINDOW_MS + 30);
    assert!(h.shard.room(id).is_none());
    h.shard.decline_rematch(id, 1); // game gone
    assert!(h.shard.counters().rejects.is_empty());
    h.send(1, rematch(id, 4, false), Some(&ew));
    assert_eq!(h.shard.counters().rejects.get("NotInGame"), Some(&1), "a client's request still counts");
}

#[tokio::test]
async fn abort_is_a_conduct_incident_forfeit_user_and_shutdown_commits_and_closes_the_journal() {
    let mut h = Rig::new().await;
    let a = h.new_game(1, 2);
    h.send(2, abort(a, 2), None);
    assert_eq!(h.events.conduct(), [(2, IncidentKind::Abort)]);
    let b = h.new_game(5, 6);
    h.play(b, 1000);
    h.play(b, 1000);
    assert!(h.shard.forfeit_user(6));
    assert_eq!(result(h.room(b)), (GS::WhiteWins, ER::Forfeit));
    assert!(!h.shard.forfeit_user(6));
    assert_eq!(h.shard.pending_commits(), 2);
    h.shard.shutdown().await;
    assert_eq!(h.shard.pending_commits(), 0);
    let mut ids = h.store.committed_ids();
    ids.sort_unstable();
    assert_eq!(ids, [a, b]);
    assert_eq!(h.shard.create(new_game(7, 8)), Err(EC::ShuttingDown));
    // The journal was written and closed: a restart takes nothing back.
    let dir = h.dir.clone().expect("a journal directory");
    let j = Journal::open(journal_options(&dir, SHARD, &h.clock)).await.expect("journal");
    assert!(j.recover().is_empty());
    j.close().await.expect("closed");
}

#[tokio::test]
async fn cancel_ends_a_game_the_lobby_gave_up_creating_server_aborted_without_a_no_show() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    assert!(h.shard.cancel(id));
    assert_eq!(result(h.room(id)), (GS::Aborted, ER::ServerAborted));
    assert_eq!(h.shard.active_game_of(1), None);
    assert!(!h.shard.cancel(id));
    h.run_timers(T0 + 30000 + 150);
    h.shard.drain_tasks().await;
    assert!(h.events.conduct().is_empty());
}

#[tokio::test]
async fn resync_binds_a_connection_not_attached_yet_and_answers_with_a_snapshot() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let ep = Ep::new(3, 2);
    h.send(2, resync(id, 4), Some(&ep));
    assert_eq!(ep.last().msg_type(), MsgType::GameSnapshot);
    assert!(h.shard.endpoint(id, B).is_some_and(|e| e.same(&ep.endpoint())));
}

#[tokio::test]
async fn a_room_replays_from_the_records_the_host_journaled() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    for _ in 0..6 {
        h.play(id, 900);
    }
    let t = h.t();
    let live = h.room(id).snapshot(W, t);
    let settings = h.settings();
    let (dir, _temp) = h.crash().await;
    let clock = ManualClock::new(t as f64, t);
    let mut j = Journal::open(journal_options(&dir, SHARD, &clock)).await.expect("journal");
    let records = j.take_recovered().shift_remove(&id).expect("the game's records");
    let copy = GameRoom::from_journal(&records, settings, FakeRules::boxed(Script::default()), true)
        .expect("replayed");
    assert_eq!(copy.snapshot(W, t), live);
    j.close().await.expect("closed");
}

#[tokio::test]
async fn gestures_go_to_the_opponent_only_as_droppable_frames_nothing_journaled_or_timed() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let (ew, eb) = (Ep::new(10, 1), Ep::new(20, 2));
    h.shard.attach(id, 1, ew.endpoint());
    h.shard.attach(id, 2, eb.endpoint());
    h.play(id, 1000);
    h.play(id, 1000);
    let t = h.t();
    let state = |h: &Rig| {
        (h.journal_kinds(id).len(), h.room(id).gseq(), h.shard.deadline_of(id), h.room(id).snapshot(W, t))
    };
    let before = state(&h);
    ew.clear();
    eb.clear();
    assert!(h.shard.gesture(id, 1, &gesture(41, id, -1234)));
    assert!(ew.sent().is_empty(), "never back to the sender");
    match eb.msgs().as_slice() {
        [ServerMsg::Gesture(g)] => assert_eq!(
            (g.game, g.ply, g.touch, g.aim, g.placed, g.flags, g.yaw, g.pitch, g.lean),
            (id, 2, 12, 28, 0, 5, -1234, 321, 40)
        ),
        other => panic!("not one gesture: {other:?}"),
    }
    assert!(h.shard.gesture(id, 2, &gesture(42, id, 7)));
    assert!(matches!(ew.last(), ServerMsg::Gesture(g) if g.yaw == 7));
    assert_eq!(state(&h), before);
    assert!(h.events.anomalies().is_empty());
    assert_eq!(h.shard.counters().gestures, 2);
}

#[tokio::test]
async fn gestures_are_dropped_without_an_anomaly_when_they_cannot_be_relayed_and_relayed_in_the_rematch_window()
 {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let (ew, eb) = (Ep::new(10, 1), Ep::with_limit(20, 2, 1000));
    h.shard.attach(id, 1, ew.endpoint());
    h.shard.attach(id, 2, eb.endpoint());
    let dropped = |h: &Rig, why: &str| h.shard.counters().gesture_drops.get(why).copied().unwrap_or(0);
    assert!(!h.shard.gesture(id, 99, &gesture(5, id, 0)));
    assert_eq!(dropped(&h, "not_player"), 1);
    eb.block(300);
    assert!(!h.shard.gesture(id, 1, &gesture(5, id, 0)));
    assert_eq!(dropped(&h, "backlog"), 1);
    eb.unblock();
    assert!(!h.shard.gesture(id, 1, &gesture(5, id, 0)[..20]));
    assert_eq!(dropped(&h, "malformed"), 1);
    assert!(!h.shard.gesture(id + 1, 1, &gesture(5, id + 1, 0)));
    assert_eq!(dropped(&h, "no_game"), 1);
    h.advance(1000);
    h.send(1, resign(id, 3), Some(&ew));
    assert!(h.room(id).is_over());
    assert!(h.shard.gesture(id, 1, &gesture(5, id, 0)), "the room still exists: the rematch window");
    h.shard.detach(id, 2, Some(20));
    assert!(!h.shard.gesture(id, 1, &gesture(5, id, 0)));
    assert_eq!(dropped(&h, "no_opponent"), 1);
    assert!(h.events.anomalies().is_empty());
}

#[tokio::test]
async fn auto_press_in_the_snapshot_the_created_record_the_rematch_and_the_record_flags() {
    let events = Arc::new(RecordingEvents::with_rematch(RematchPolicy::Accept(1)));
    let mut h = Rig::with(Opts { events: Some(events), ..Opts::default() }).await;
    let auto_in_created = |h: &Rig, id| {
        let (_, rec) = h
            .shard
            .journal_log
            .iter()
            .find(|(g, r)| *g == id && r.kind == RecordKind::Created)
            .expect("created");
        rec.payload[1] & 2 != 0
    };
    let id = h.new_game(1, 2);
    let ep = Ep::new(1, 1);
    h.shard.attach(id, 1, ep.endpoint());
    assert!(snapshot(ep.last()).auto_press);
    assert!(auto_in_created(&h, id));
    let manual = h.create(NewGame { auto_press: false, ..new_game(3, 4) });
    assert!(!h.room(manual).auto_press());
    assert!(!auto_in_created(&h, manual));
    let (ew, eb) = (Ep::new(3, 3), Ep::new(4, 4));
    h.shard.attach(manual, 3, ew.endpoint());
    h.shard.attach(manual, 4, eb.endpoint());
    assert!(!snapshot(ew.last()).auto_press);
    h.play(manual, 1000);
    h.play(manual, 1000);
    h.send(3, resign(manual, 3), Some(&ew));
    h.send(4, rematch(manual, 4, true), Some(&eb));
    h.send(3, rematch(manual, 5, true), Some(&ew));
    assert!(!h.events.rematches()[0].game.auto_press);
    let t = h.t() + 100;
    assert_eq!(h.poll(t).await, Some(true));
    h.shard.drain_tasks().await;
    let rec = h.store.batches().concat().into_iter().find(|r| r.id == manual).expect("committed");
    assert_eq!(rec.flags & record_flag::MANUAL_CLOCK, record_flag::MANUAL_CLOCK);
}

#[tokio::test]
async fn draw_offers_reach_the_opponent_through_the_host() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let (ew, eb) = (Ep::new(1, 1), Ep::new(2, 2));
    h.shard.attach(id, 1, ew.endpoint());
    h.shard.attach(id, 2, eb.endpoint());
    h.play(id, 1000);
    h.play(id, 1000);
    h.send(1, ClientMsg::DrawOffer(DrawOffer { seq: 3, game: id }), Some(&ew));
    assert_eq!(game_event(&eb.last()), (EV::DrawOffered, Color::White, 0));
}

/// Rules whose moves panic (a bug in the rules).
struct Explosive(FakeRules);

impl Rules for Explosive {
    fn digest(&self) -> u32 {
        self.0.digest()
    }

    fn is_legal(&self, m: u16) -> bool {
        self.0.is_legal(m)
    }

    fn play(&mut self, _m: u16) -> Option<Played> {
        panic!("a bug in the rules")
    }

    fn status(&self) -> GS {
        self.0.status()
    }

    fn reason(&self) -> ER {
        self.0.reason()
    }

    fn can_claim_threefold(&self) -> bool {
        false
    }

    fn can_claim_fifty_move(&self) -> bool {
        false
    }

    fn can_color_mate(&self, side: Side) -> bool {
        self.0.can_color_mate(side)
    }

    fn end(&mut self, status: GS, reason: ER) {
        self.0.end(status, reason);
    }
}

#[tokio::test]
async fn a_panic_in_room_code_is_contained_the_request_gets_internal_and_the_host_goes_on() {
    let rules: RulesFactory = Arc::new(|| Box::new(Explosive(FakeRules::default())));
    let mut h = Rig::with(Opts { rules: Some(rules), ..Opts::default() }).await;
    let id = h.new_game(1, 2);
    let ew = Ep::new(1, 1);
    h.advance(1000);
    h.move_now(id, Some(&ew));
    assert_eq!(error(&ew.last()), (EC::Internal, false, 5));
    assert_eq!(h.shard.counters().rejects.get("Internal"), Some(&1));
    // The game keeps its timer, and the host its other games.
    assert!(h.shard.deadline_of(id).is_some());
    let other = h.new_game(3, 4);
    h.send(3, abort(other, 2), None);
    assert_eq!(result(h.room(other)), (GS::Aborted, ER::Aborted));
}
