//! Tests of the lobby actor (the former control-plane tests), on an in-memory store and fake
//! game hosts.

use std::sync::Arc;
use std::time::Duration;

use scacelith_protocol::{self as proto, ErrorCode, NoticeCode, ServerMsg};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{ClaimOutcome, Lobby, LobbyDeps, LobbyMsg, LobbyRequest, Timer};
use crate::clock::{Clock, ManualClock};
use crate::config::{Config, test_config};
use crate::events::{
    GameEnded, HostEvents, IncidentKind, NewGame, RematchRequest, SanctionApplied, SanctionEvents,
    SessionEvents,
};
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::matching::ColorPref;
use crate::matching::challenges::{ChallengeSettings, Challenges};
use crate::matching::elo::Categories;
use crate::matching::matchmaker::{MatchSettings, Matchmaker, PAIR_RETRY_DELAY_MS};
use crate::net::limits::SharedLimits;
use crate::realtime::endpoint::{Endpoint, Outbound};
use crate::realtime::link::{ConnCmd, ConnLink};
use crate::realtime::testing::{CreateMode, FakeHosts, HostCall, drain, name};
use crate::store::{NewSanction, NewUser, SanctionKind, Source, Store, StoreOptions};

const START: i64 = 1_780_315_200_000; // 2026-06-01T12:00:00Z
const NAMES: [&str; 6] = ["alice", "bob", "carol", "dave", "erin", "frank"];

struct Harness {
    lobby: Lobby,
    hosts: Arc<FakeHosts>,
    store: Store,
    clock: Arc<ManualClock>,
    config: Arc<Config>,
    _task: JoinHandle<()>,
}

struct Player {
    link: Arc<ConnLink>,
    out: Arc<Outbound>,
    cmds: mpsc::UnboundedReceiver<ConnCmd>,
    seq: u32,
}

impl Player {
    fn user(&self) -> UserId {
        self.link.user_id()
    }

    fn frames(&self) -> Vec<ServerMsg> {
        drain(&self.out)
    }

    fn attached(&mut self) -> Vec<GameId> {
        let mut games = Vec::new();
        while let Ok(ConnCmd::Attach(g)) = self.cmds.try_recv() {
            games.push(g);
        }
        games
    }
}

fn ack_or_error(frames: &[ServerMsg], seq: u32) -> Result<(), ErrorCode> {
    let answers: Vec<Result<(), ErrorCode>> = frames
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Ack(a) if a.r#ref == seq => Some(Ok(())),
            ServerMsg::Error(e) if e.r#ref == seq && !e.fatal => Some(Err(e.code)),
            _ => None,
        })
        .collect();
    assert_eq!(answers.len(), 1, "exactly one answer to request {seq}: {frames:?}");
    answers[0]
}

fn queue_states(frames: &[ServerMsg]) -> Vec<proto::QueueState> {
    frames
        .iter()
        .filter_map(|m| match m {
            ServerMsg::QueueStatus(q) => Some(q.state),
            _ => None,
        })
        .collect()
}

fn challenge_states(frames: &[ServerMsg]) -> Vec<proto::ChallengeState> {
    frames
        .iter()
        .filter_map(|m| match m {
            ServerMsg::ChallengeStatus(c) => Some(c.state),
            _ => None,
        })
        .collect()
}

fn notices(frames: &[ServerMsg]) -> Vec<(NoticeCode, f64)> {
    frames
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Notice(n) => Some((n.code, n.arg)),
            _ => None,
        })
        .collect()
}

fn queue_join(category: &str, rated: bool) -> LobbyRequest {
    LobbyRequest::QueueJoin {
        category: category.into(),
        rated,
        rating: 1500,
        provisional: false,
        ban: None,
        cooldown: None,
    }
}

fn challenge(target: &str, base_sec: u16, inc_sec: u8, rated: bool) -> LobbyRequest {
    LobbyRequest::ChallengeCreate {
        target: target.into(),
        base_sec,
        inc_sec,
        rated,
        color: ColorPref::Random,
        rating: 1500,
        provisional: false,
        ban: None,
    }
}

impl Harness {
    async fn new(overrides: &[(&str, &str)]) -> Harness {
        let config = Arc::new(test_config(overrides).expect("a valid configuration"));
        let clock = ManualClock::new(1_000_000.0, START);
        let options =
            StoreOptions { path: Some(":memory:".into()), clock: Some(clock.clone()), ..Default::default() };
        let store = Store::open(&config, options).await.expect("store");
        store.migrate().await.expect("migrations");
        for name in NAMES {
            let user = NewUser {
                username: name.to_string(),
                email: None,
                password_hash: None,
                email_verified: true,
                accept_challenges: name != "dave",
                created_at: START,
            };
            store.users().create(user).await.expect("user");
        }
        let hosts = FakeHosts::new(4);
        let mut n = 0u32;
        let deps = LobbyDeps {
            config: config.clone(),
            clock: clock.clone(),
            store: store.clone(),
            hosts: hosts.clone(),
            limits: Arc::new(SharedLimits::new(clock.clone())),
            matchmaker: Matchmaker::new(
                MatchSettings::from_config(&config),
                Categories::from_config(&config),
                Box::new(|| 0.1),
            ),
            challenges: Challenges::new(
                ChallengeSettings::from_config(&config),
                Categories::from_config(&config),
                Box::new(move |max| {
                    n = n.wrapping_add(7);
                    n % max.max(1)
                }),
            ),
            create_timeout: Duration::from_millis(100),
            log: Logger::root().child("lobby"),
        };
        let (lobby, inbox) = Lobby::channel();
        let task = inbox.start_manual(deps);
        Harness { lobby, hosts, store, clock, config, _task: task }
    }

    fn connect(&self, user: UserId, conn: u32) -> Player {
        let out = Outbound::new(1 << 20);
        let name = NAMES[(user as usize - 1) % NAMES.len()];
        let (link, cmds) = ConnLink::new(
            Endpoint::new(conn, user, out.clone()),
            name.to_string(),
            crate::util::sha256(name.as_bytes()),
        );
        Player { link, out, cmds, seq: 1 }
    }

    async fn claim(&self, p: &Player, ban: Option<i64>) -> ClaimOutcome {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.lobby.post(LobbyMsg::Claim { link: p.link.clone(), ban, reply });
        let outcome = answer.await.expect("an answer");
        if matches!(outcome, ClaimOutcome::Admitted { .. }) {
            p.link.set_welcomed();
        }
        outcome
    }

    /// A player online with connection `user * 10`.
    async fn online(&self, user: UserId) -> Player {
        let p = self.connect(user, user * 10);
        assert!(matches!(self.claim(&p, None).await, ClaimOutcome::Admitted { .. }));
        p
    }

    fn release(&self, p: &Player) {
        self.lobby.post(LobbyMsg::Release { user: p.user(), conn: p.link.conn_id() });
    }

    /// Posts a request and waits until the lobby is idle; returns its sequence number.
    async fn request(&self, p: &mut Player, req: LobbyRequest) -> u32 {
        p.seq += 1;
        assert!(p.link.begin_request());
        self.lobby.post(LobbyMsg::Request { link: p.link.clone(), seq: p.seq, req });
        self.settle().await;
        p.seq
    }

    /// Posts a request and returns its answer (the other frames are dropped).
    async fn ask(&self, p: &mut Player, req: LobbyRequest) -> Result<(), ErrorCode> {
        let seq = self.request(p, req).await;
        ack_or_error(&p.frames(), seq)
    }

    async fn timer(&self, t: Timer) {
        self.lobby.post(LobbyMsg::Timer(t));
        self.settle().await;
    }

    /// Waits until the lobby handled everything and its tasks are done.
    async fn settle(&self) {
        loop {
            if self.lobby.pending_tasks().await == Some(0) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    async fn ban_in_store(&self, user: UserId, until: i64) {
        let now = self.clock.wall_ms();
        let s = NewSanction {
            user_id: user,
            kind: SanctionKind::Ban,
            reason: Some("cheating".into()),
            source: Source::Moderator,
            game_id: None,
            starts_at: now,
            ends_at: Some(until),
            created_by: None,
            created_at: now,
        };
        self.store.sanctions().create(s).await.expect("sanction");
    }

    async fn unban_in_store(&self, user: UserId) {
        let now = self.clock.wall_ms();
        for s in self.store.sanctions().active(user, now).await.expect("sanctions") {
            self.store.sanctions().lift(s.id, None, now).await.expect("lift");
        }
    }

    fn game_ended(&self, game: GameId, white: UserId, black: UserId) {
        self.lobby.game_ended(GameEnded {
            game,
            white,
            black,
            status: proto::GameStatus::WhiteWins,
            reason: proto::EndReason::Resignation,
            rated: false,
            category: "5+0".into(),
        });
    }

    async fn rematch(
        &self,
        old: GameId,
        white: UserId,
        black: UserId,
        rated: bool,
    ) -> Result<GameId, ErrorCode> {
        let info = |u: UserId| proto::PlayerInfo {
            user_id: u,
            name: NAMES[u as usize - 1].into(),
            rating: 1500,
            provisional: false,
        };
        let game = NewGame {
            category: "5+0".into(),
            base_ms: 300_000,
            inc_ms: 0,
            rated,
            white: info(white),
            black: info(black),
            created_at: self.clock.wall_ms(),
            rematch_of: Some(old),
            auto_press: true,
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.lobby.rematch(RematchRequest { game }, reply);
        let r = answer.await.expect("an answer");
        self.settle().await;
        r
    }

    fn last_created(&self) -> NewGame {
        self.hosts.created().pop().expect("a game was created")
    }

    /// Pairs two players through the queue; returns the game.
    async fn pair(&self, a: &mut Player, b: &mut Player, category: &str, rated: bool) -> Option<GameId> {
        assert_eq!(self.ask(a, queue_join(category, rated)).await, Ok(()));
        assert_eq!(self.ask(b, queue_join(category, rated)).await, Ok(()));
        self.clock.advance(250.0);
        self.timer(Timer::MatchTick).await;
        let game = a.attached().pop();
        b.attached();
        game
    }
}

// ---- presence -----------------------------------------------------------------------------------

#[tokio::test]
async fn replaces_an_older_connection_and_releases_only_the_live_one() {
    let h = Harness::new(&[]).await;
    let mut old = h.online(1).await;
    assert_eq!(h.ask(&mut old, queue_join("5+0", true)).await, Ok(()));
    let new = h.connect(1, 20);
    assert_eq!(h.claim(&new, None).await, ClaimOutcome::Admitted { active_game: 0 });
    let kick = old.out.take();
    assert_eq!(kick.close.map(|c| c.code), Some(4007));
    let frames: Vec<ServerMsg> = kick.frames.iter().map(|f| crate::realtime::testing::decode(f)).collect();
    assert_eq!(frames.iter().map(name).collect::<Vec<_>>(), ["Notice", "Error"]);
    assert_eq!(notices(&frames), [(NoticeCode::ReplacedByNewConnection, 0.0)]);
    assert!(
        matches!(&frames[1], ServerMsg::Error(e) if e.code == ErrorCode::Replaced && e.fatal && e.r#ref == 0)
    );
    // The replaced connection's search ended without a word, and its release changes nothing.
    h.release(&old);
    let mut new = new;
    assert_eq!(h.ask(&mut new, LobbyRequest::QueueLeave).await, Ok(()));
    assert!(queue_states(&new.frames()).is_empty(), "nobody was searching");
    // The live connection's release logs the account out: another connection is a newcomer.
    h.release(&new);
    let third = h.connect(1, 30);
    assert_eq!(h.claim(&third, None).await, ClaimOutcome::Admitted { active_game: 0 });
    assert!(third.out.is_open() && new.out.is_open(), "nothing to replace");
}

#[tokio::test]
async fn refuses_banned_users_and_a_full_server() {
    let h = Harness::new(&[("MAX_CONNECTIONS", "1")]).await;
    let until = START + 60_000;
    let bad = h.connect(5, 1);
    assert_eq!(h.claim(&bad, Some(until)).await, ClaimOutcome::Banned { until });
    assert!(bad.out.is_open(), "the connection says it itself");
    let a = h.connect(1, 2);
    assert_eq!(h.claim(&a, None).await, ClaimOutcome::Admitted { active_game: 0 });
    let b = h.connect(2, 3);
    assert_eq!(h.claim(&b, None).await, ClaimOutcome::Full);
    let again = h.connect(1, 4);
    assert_eq!(
        h.claim(&again, None).await,
        ClaimOutcome::Admitted { active_game: 0 },
        "a reconnect is not a newcomer"
    );
}

#[tokio::test]
async fn admits_a_player_whose_game_is_in_progress_on_a_full_server() {
    let h = Harness::new(&[("MAX_CONNECTIONS", "3")]).await;
    let players: Vec<Player> = futures_join(&h, &[1, 2, 3]).await;
    let game = h.hosts.next_id(1);
    h.lobby.game_recovered(game, 3, 6);
    h.release(&players[2]);
    let newcomer = h.connect(4, 40);
    assert_eq!(h.claim(&newcomer, None).await, ClaimOutcome::Admitted { active_game: 0 });
    let back = h.connect(3, 31);
    assert_eq!(h.claim(&back, None).await, ClaimOutcome::Admitted { active_game: game });
    let opponent = h.connect(6, 60);
    assert_eq!(
        h.claim(&opponent, None).await,
        ClaimOutcome::Admitted { active_game: game },
        "never seen before"
    );
    let another = h.connect(5, 50);
    assert_eq!(h.claim(&another, None).await, ClaimOutcome::Full);
}

async fn futures_join(h: &Harness, users: &[UserId]) -> Vec<Player> {
    let mut out = Vec::new();
    for &u in users {
        out.push(h.online(u).await);
    }
    out
}

#[tokio::test]
async fn a_claim_whose_connection_went_away_is_released() {
    let h = Harness::new(&[]).await;
    let gone = h.connect(1, 10);
    let (reply, answer) = tokio::sync::oneshot::channel();
    drop(answer);
    h.lobby.post(LobbyMsg::Claim { link: gone.link.clone(), ban: None, reply });
    h.settle().await;
    // A challenge to alice finds nobody online.
    let mut b = h.online(2).await;
    assert_eq!(h.ask(&mut b, challenge("alice", 300, 0, false)).await, Err(ErrorCode::UserUnavailable));
}

// ---- matchmaking --------------------------------------------------------------------------------

#[tokio::test]
async fn joins_the_queue_and_refuses_stale_busy_unknown_and_cooling_down_players() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let seq = h.request(&mut a, queue_join("5+0", true)).await;
    let frames = a.frames();
    assert_eq!(frames.iter().map(name).collect::<Vec<_>>(), ["Ack", "QueueStatus"]);
    assert_eq!(ack_or_error(&frames, seq), Ok(()));
    let ServerMsg::QueueStatus(q) = &frames[1] else { unreachable!() };
    assert_eq!(
        (q.state, q.category.as_str(), q.rated, q.queued),
        (proto::QueueState::Searching, "5+0", true, 1)
    );

    // Another connection of the same account (not the live one).
    let mut stale = h.connect(1, 999);
    assert_eq!(h.ask(&mut stale, queue_join("5+0", true)).await, Err(ErrorCode::QueueNotAllowed));
    assert_eq!(h.ask(&mut a, queue_join("7+7", true)).await, Err(ErrorCode::InvalidCategory));

    let until = START + 3_600_000;
    let mut c = h.online(3).await;
    let cooling = LobbyRequest::QueueJoin {
        category: "5+0".into(),
        rated: true,
        rating: 1500,
        provisional: false,
        ban: None,
        cooldown: Some(crate::matching::conduct::Cooldown { until, level: 1 }),
    };
    let seq = h.request(&mut c, cooling).await;
    let frames = c.frames();
    assert_eq!(notices(&frames), [(NoticeCode::MatchmakingCooldown, until as f64)]);
    assert_eq!(ack_or_error(&frames, seq), Err(ErrorCode::MatchmakingCooldown));
    // The cooldown is cached: a later rated join is refused without the stored state.
    assert_eq!(h.ask(&mut c, queue_join("5+0", true)).await, Err(ErrorCode::MatchmakingCooldown));
    assert_eq!(h.ask(&mut c, queue_join("5+0", false)).await, Ok(()), "casual games stay possible");

    h.lobby.game_recovered(h.hosts.next_id(0), 1, 6);
    assert_eq!(h.ask(&mut a, queue_join("5+0", true)).await, Err(ErrorCode::AlreadyInGame));

    let seq = h.request(&mut c, LobbyRequest::QueueLeave).await;
    let frames = c.frames();
    assert_eq!(frames.iter().map(name).collect::<Vec<_>>(), ["QueueStatus", "Ack"]);
    assert_eq!(queue_states(&frames), [proto::QueueState::Left]);
    assert_eq!(ack_or_error(&frames, seq), Ok(()));
}

#[tokio::test]
async fn pairs_tells_both_creates_the_game_and_attaches_both_connections() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    assert_eq!(h.ask(&mut a, queue_join("5+0", true)).await, Ok(()));
    h.clock.advance(4000.0);
    assert_eq!(h.ask(&mut b, queue_join("5+0", true)).await, Ok(()));
    h.clock.advance(250.0);
    h.timer(Timer::MatchTick).await;
    let fa = a.frames();
    let matched: Vec<_> = fa
        .iter()
        .filter_map(|m| match m {
            ServerMsg::QueueStatus(q) => Some((q.state, q.wait_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(matched, [(proto::QueueState::Matched, 4250)]);
    assert_eq!(queue_states(&b.frames()), [proto::QueueState::Matched]);
    let game = h.last_created();
    assert_eq!((game.category.as_str(), game.base_ms, game.inc_ms, game.rated), ("5+0", 300_000, 0, true));
    assert_eq!(game.white.name.len() + game.black.name.len(), 8, "alice and bob");
    assert!(game.auto_press);
    let id = a.attached();
    assert_eq!(id.len(), 1);
    assert_eq!(b.attached(), id);
    // The game is the players' active game until it ends.
    let again = h.connect(1, 11);
    assert_eq!(h.claim(&again, None).await, ClaimOutcome::Admitted { active_game: id[0] });
    h.game_ended(id[0], 2, 1);
    let third = h.connect(1, 12);
    assert_eq!(h.claim(&third, None).await, ClaimOutcome::Admitted { active_game: 0 });
}

#[tokio::test]
async fn colours_alternate_and_a_failed_creation_gives_them_back() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    let mut whites = Vec::new();
    for _ in 0..4 {
        let game = h.pair(&mut a, &mut b, "5+0", false).await.expect("a game");
        let g = h.last_created();
        whites.push(g.white.user_id);
        h.game_ended(game, g.white.user_id, g.black.user_id);
    }
    assert_eq!(whites, [1, 2, 1, 2]);
    h.hosts.set_mode(CreateMode::Fail(ErrorCode::Internal));
    assert_eq!(h.pair(&mut a, &mut b, "5+0", false).await, None);
    h.hosts.set_mode(CreateMode::Ok);
    // Both are back in the queue with their colours given back: the next pairing (after the
    // hold) gives White to alice again.
    h.clock.advance(PAIR_RETRY_DELAY_MS as f64);
    h.timer(Timer::MatchTick).await;
    assert_eq!(h.last_created().white.user_id, 1);
}

#[tokio::test]
async fn a_pairing_whose_game_could_not_be_created_is_held_then_tried_again() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    h.hosts.set_mode(CreateMode::Fail(ErrorCode::Internal));
    assert_eq!(h.pair(&mut a, &mut b, "5+0", true).await, None);
    assert_eq!(h.hosts.created().len(), 1);
    // Both still search (a refresh tells them), and the pair is not made at the next tick.
    a.frames();
    h.timer(Timer::RefreshQueues).await;
    assert_eq!(queue_states(&a.frames()), [proto::QueueState::Searching]);
    h.clock.advance(250.0);
    h.timer(Timer::MatchTick).await;
    assert_eq!(h.hosts.created().len(), 1, "held");
    h.clock.advance(PAIR_RETRY_DELAY_MS as f64);
    h.hosts.set_mode(CreateMode::Ok);
    h.timer(Timer::MatchTick).await;
    assert_eq!(h.hosts.created().len(), 2, "tried again once the delay is over");
    assert_eq!(a.attached().len(), 1);
}

#[tokio::test]
async fn a_rated_game_counts_toward_the_repeat_limit_whatever_made_it() {
    let h = Harness::new(&[("MATCH_REPEAT_LIMIT", "2")]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    // A direct challenge, then a private game: two rated games.
    assert_eq!(h.ask(&mut a, challenge("bob", 300, 0, true)).await, Ok(()));
    let id = received_id(&b.frames());
    assert_eq!(h.ask(&mut b, LobbyRequest::ChallengeAccept { id }).await, Ok(()));
    let g1 = a.attached()[0];
    b.attached();
    h.game_ended(g1, 1, 2);
    let seq = h.request(&mut a, challenge("", 300, 0, true)).await;
    let code = pending_code(&a.frames(), seq);
    assert_eq!(h.ask(&mut b, LobbyRequest::ChallengeJoinCode { code }).await, Ok(()));
    let g2 = a.attached()[0];
    b.attached();
    h.game_ended(g2, 1, 2);
    a.frames();
    // The limit is reached: no rated challenge, private game or rematch between them.
    assert_eq!(h.ask(&mut a, challenge("bob", 300, 0, true)).await, Err(ErrorCode::RatedRepeatLimit));
    assert!(b.frames().is_empty(), "nothing reaches the target");
    let seq = h.request(&mut b, challenge("", 300, 0, true)).await;
    let code = pending_code(&b.frames(), seq);
    assert_eq!(
        h.ask(&mut a, LobbyRequest::ChallengeJoinCode { code: code.clone() }).await,
        Err(ErrorCode::RatedRepeatLimit)
    );
    assert!(b.frames().is_empty(), "the creator is told nothing");
    assert_eq!(h.rematch(g2, 2, 1, true).await, Err(ErrorCode::RematchUnavailable));
    // The code stays valid for anyone else; unrated games stay free.
    let mut c = h.online(3).await;
    assert_eq!(h.ask(&mut c, LobbyRequest::ChallengeJoinCode { code }).await, Ok(()));
    assert_eq!(h.ask(&mut a, challenge("bob", 300, 0, false)).await, Ok(()));
    let id = received_id(&b.frames());
    b.attached();
    let created = h.hosts.created().len();
    assert_eq!(h.ask(&mut b, LobbyRequest::ChallengeAccept { id }).await, Err(ErrorCode::AlreadyInGame));
    assert_eq!(h.hosts.created().len(), created);
}

fn received_id(frames: &[ServerMsg]) -> u32 {
    frames
        .iter()
        .find_map(|m| match m {
            ServerMsg::ChallengeReceived(c) => Some(c.id),
            _ => None,
        })
        .expect("a ChallengeReceived")
}

fn pending_code(frames: &[ServerMsg], seq: u32) -> String {
    assert_eq!(ack_or_error(frames, seq), Ok(()));
    frames
        .iter()
        .find_map(|m| match m {
            ServerMsg::ChallengeStatus(c) if c.state == proto::ChallengeState::Pending => {
                Some(c.code.clone())
            }
            _ => None,
        })
        .expect("a pending ChallengeStatus")
}

// ---- challenges ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_direct_challenge_is_announced_then_accepted() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    let create = LobbyRequest::ChallengeCreate {
        target: "Bob".into(),
        base_sec: 300,
        inc_sec: 0,
        rated: true,
        color: ColorPref::White,
        rating: 1620,
        provisional: false,
        ban: None,
    };
    let seq = h.request(&mut a, create).await;
    let fa = a.frames();
    assert_eq!(fa.iter().map(name).collect::<Vec<_>>(), ["ChallengeStatus", "Ack"]);
    assert_eq!(ack_or_error(&fa, seq), Ok(()));
    assert_eq!(challenge_states(&fa), [proto::ChallengeState::Pending]);
    let fb = b.frames();
    let ServerMsg::ChallengeReceived(r) = &fb[0] else { panic!("{fb:?}") };
    assert_eq!((r.from.name.as_str(), r.from.rating, r.your_color), ("alice", 1620, proto::ColorPref::Black));
    assert_eq!(i64::from(r.expires_ms), h.config.challenge_ttl_ms);
    let seq = h.request(&mut b, LobbyRequest::ChallengeAccept { id: r.id }).await;
    assert_eq!(ack_or_error(&b.frames(), seq), Ok(()));
    assert_eq!(challenge_states(&a.frames()), [proto::ChallengeState::Accepted]);
    let g = h.last_created();
    assert_eq!((g.white.user_id, g.black.user_id, g.category.as_str(), g.rated), (1, 2, "5+0", true));
    assert_eq!(g.white.rating, 1500, "ratings read again from the store");
    let game = b.attached()[0];
    assert_eq!(a.attached(), [game]);
    // Busy now: a new challenge cannot be accepted.
    assert_eq!(h.ask(&mut a, challenge("bob", 300, 0, false)).await, Ok(()));
    let id = received_id(&b.frames());
    assert_eq!(h.ask(&mut b, LobbyRequest::ChallengeAccept { id }).await, Err(ErrorCode::AlreadyInGame));
}

#[tokio::test]
async fn a_busy_creator_is_told_to_the_target_only() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    let mut c = h.online(3).await;
    assert_eq!(h.ask(&mut a, challenge("bob", 300, 0, false)).await, Ok(()));
    let direct = received_id(&b.frames());
    h.lobby.game_recovered(h.hosts.next_id(0), 1, 6);
    assert_eq!(
        h.ask(&mut c, LobbyRequest::ChallengeAccept { id: direct }).await,
        Err(ErrorCode::ChallengeNotFound)
    );
    assert_eq!(
        h.ask(&mut b, LobbyRequest::ChallengeAccept { id: direct }).await,
        Err(ErrorCode::AlreadyInGame)
    );
    assert!(h.hosts.created().is_empty());
}

#[tokio::test]
async fn decline_cancel_expiry_and_a_creator_going_offline_notify_the_other_side() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    let _d = h.online(4).await;
    assert_eq!(
        h.ask(&mut a, challenge("dave", 60, 0, false)).await,
        Err(ErrorCode::UserUnavailable),
        "refuses"
    );
    assert_eq!(h.ask(&mut a, challenge("nobody", 60, 0, false)).await, Err(ErrorCode::UserUnavailable));

    assert_eq!(h.ask(&mut a, challenge("bob", 60, 0, false)).await, Ok(()));
    let id = received_id(&b.frames());
    assert_eq!(h.ask(&mut b, LobbyRequest::ChallengeDecline { id }).await, Ok(()));
    assert_eq!(challenge_states(&a.frames()), [proto::ChallengeState::Declined]);

    let seq = h.request(&mut a, challenge("bob", 60, 0, false)).await;
    let id = match &a.frames()[0] {
        ServerMsg::ChallengeStatus(c) => c.id,
        other => panic!("{other:?} for {seq}"),
    };
    b.frames();
    assert_eq!(h.ask(&mut a, LobbyRequest::ChallengeCancel { id }).await, Ok(()));
    assert_eq!(challenge_states(&b.frames()), [proto::ChallengeState::Cancelled]);
    assert_eq!(h.ask(&mut a, LobbyRequest::ChallengeCancel { id }).await, Err(ErrorCode::ChallengeNotFound));

    assert_eq!(h.ask(&mut a, challenge("bob", 60, 0, false)).await, Ok(()));
    b.frames();
    h.clock.advance(h.config.challenge_ttl_ms as f64 + 1.0);
    h.timer(Timer::ExpireChallenges).await;
    assert_eq!(challenge_states(&a.frames()), [proto::ChallengeState::Expired]);
    assert_eq!(challenge_states(&b.frames()), [proto::ChallengeState::Expired]);

    assert_eq!(h.ask(&mut b, challenge("alice", 60, 0, false)).await, Ok(()));
    a.frames();
    h.release(&b);
    h.settle().await;
    assert_eq!(challenge_states(&a.frames()), [proto::ChallengeState::Cancelled]);
}

#[tokio::test]
async fn withdrawn_or_declined_challenges_are_limited_per_creator() {
    let h = Harness::new(&[("CHALLENGE_UNPLAYED_PER_MIN", "2")]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    // Accepted challenges are not counted.
    for _ in 0..3 {
        assert_eq!(h.ask(&mut a, challenge("bob", 60, 0, false)).await, Ok(()));
        let id = received_id(&b.frames());
        assert_eq!(h.ask(&mut b, LobbyRequest::ChallengeAccept { id }).await, Ok(()));
        let game = a.attached()[0];
        b.attached();
        h.game_ended(game, 1, 2);
    }
    for _ in 0..2 {
        assert_eq!(h.ask(&mut a, challenge("bob", 60, 0, false)).await, Ok(()));
        let id = received_id(&b.frames());
        assert_eq!(h.ask(&mut b, LobbyRequest::ChallengeDecline { id }).await, Ok(()));
    }
    a.frames();
    assert_eq!(h.ask(&mut a, challenge("bob", 60, 0, false)).await, Err(ErrorCode::ChallengeLimit));
    assert!(b.frames().is_empty(), "nothing reaches the target");
    assert_eq!(h.ask(&mut a, challenge("", 60, 0, false)).await, Ok(()), "private games are not counted");
    h.clock.advance(120_000.0);
    assert_eq!(h.ask(&mut a, challenge("bob", 60, 0, false)).await, Ok(()));
}

#[tokio::test]
async fn wrong_private_codes_are_limited_per_player() {
    let h = Harness::new(&[("PRIVATE_CODE_FAILURES_PER_MIN", "3")]).await;
    let mut a = h.online(1).await;
    let mut c = h.online(3).await;
    let seq = h.request(&mut a, challenge("", 180, 2, false)).await;
    let code = pending_code(&a.frames(), seq);
    for _ in 0..2 {
        assert_eq!(
            h.ask(&mut c, LobbyRequest::ChallengeJoinCode { code: "XXXXXX".into() }).await,
            Err(ErrorCode::CodeInvalid)
        );
    }
    assert_eq!(
        h.ask(&mut a, LobbyRequest::ChallengeJoinCode { code: code.clone() }).await,
        Err(ErrorCode::CannotChallengeSelf)
    );
    assert_eq!(
        h.ask(&mut c, LobbyRequest::ChallengeJoinCode { code: code.to_lowercase() }).await,
        Ok(()),
        "a code that works costs nothing"
    );
    let g = h.last_created();
    assert_eq!((g.base_ms, g.inc_ms, g.category.as_str()), (180_000, 2000, "3+2"));
    let mut e = h.online(5).await;
    let mut d = h.online(4).await;
    let seq = h.request(&mut d, challenge("", 180, 2, false)).await;
    let code = pending_code(&d.frames(), seq);
    for _ in 0..3 {
        assert_eq!(
            h.ask(&mut e, LobbyRequest::ChallengeJoinCode { code: "XXXXXX".into() }).await,
            Err(ErrorCode::CodeInvalid)
        );
    }
    assert_eq!(
        h.ask(&mut e, LobbyRequest::ChallengeJoinCode { code: code.clone() }).await,
        Err(ErrorCode::RateLimited)
    );
    h.clock.advance(120_000.0);
    assert_eq!(h.ask(&mut e, LobbyRequest::ChallengeJoinCode { code }).await, Ok(()));
}

// ---- clock press and rematches ------------------------------------------------------------------

#[tokio::test]
async fn new_games_take_auto_press_clock_and_a_rematch_keeps_the_finished_games() {
    for auto in [true, false] {
        let h = Harness::new(&[("AUTO_PRESS_CLOCK", if auto { "true" } else { "false" })]).await;
        let mut a = h.online(1).await;
        let mut b = h.online(2).await;
        let mut c = h.online(3).await;
        let mut d = h.online(4).await;
        let game = h.pair(&mut a, &mut b, "5+0", false).await.expect("a queue game");
        let seq = h.request(&mut c, challenge("", 60, 0, false)).await;
        let code = pending_code(&c.frames(), seq);
        assert_eq!(h.ask(&mut d, LobbyRequest::ChallengeJoinCode { code }).await, Ok(()));
        assert_eq!(h.hosts.created().iter().map(|g| g.auto_press).collect::<Vec<_>>(), [auto, auto]);
        h.game_ended(game, 1, 2);
        // The host fills the rematch with the finished game's setting (true here).
        assert!(h.rematch(game, 2, 1, false).await.is_ok());
        assert!(h.last_created().auto_press);
    }
}

/// The player was kicked for a ban: the fatal `Error` is their last message, the request that
/// found the ban is not answered.
fn assert_banned_kick(p: &Player, until: i64) {
    let kick = p.out.take();
    assert_eq!(kick.close.map(|c| c.code), Some(4004));
    let frames: Vec<ServerMsg> = kick.frames.iter().map(|f| crate::realtime::testing::decode(f)).collect();
    assert_eq!(notices(&frames), [(NoticeCode::Banned, until as f64)]);
    assert!(
        matches!(frames.last(), Some(ServerMsg::Error(e)) if e.code == ErrorCode::Banned && e.fatal && e.r#ref == 0)
    );
    assert!(
        !frames
            .iter()
            .any(|m| matches!(m, ServerMsg::Ack(_)) || matches!(m, ServerMsg::Error(e) if !e.fatal))
    );
}

#[tokio::test]
async fn a_rematch_goes_to_the_old_host_and_is_refused_when_a_player_left() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let b = h.online(2).await;
    let old = h.hosts.next_id(1);
    h.lobby.game_recovered(old, 1, 2);
    let new = h.rematch(old, 2, 1, true).await.expect("a rematch");
    let calls = h.hosts.calls();
    let Some(HostCall::Create { preferred, game }) = calls.last() else { panic!("{calls:?}") };
    assert_eq!(*preferred, Some(1));
    assert_eq!((game.white.user_id, game.rematch_of, game.category.as_str()), (2, Some(old), "5+0"));
    assert_eq!(a.attached(), [new]);
    // Busy with the rematch: no queue.
    assert_eq!(h.ask(&mut a, queue_join("5+0", false)).await, Err(ErrorCode::AlreadyInGame));
    h.game_ended(new, 2, 1);
    h.release(&b);
    assert_eq!(h.rematch(new, 1, 2, true).await, Err(ErrorCode::RematchUnavailable));
}

// ---- sanctions, conduct, sessions -----------------------------------------------------------------

#[tokio::test]
async fn a_sanction_kicks_forfeits_and_keeps_the_player_out_until_it_ends() {
    let h = Harness::new(&[]).await;
    let a = h.online(1).await;
    let _b = h.online(2).await;
    let game = h.hosts.next_id(1);
    h.lobby.game_recovered(game, 1, 2);
    let until = START + 3_600_000;
    h.lobby.sanction_applied(SanctionApplied { user: 1, until, reason: "engine".into(), refunds: 0 });
    h.settle().await;
    assert_banned_kick(&a, until);
    assert!(h.hosts.calls().contains(&HostCall::Forfeit { game, user: 1 }));
    let again = h.connect(1, 11);
    assert_eq!(h.claim(&again, None).await, ClaimOutcome::Banned { until });
    h.clock.advance(3_600_001.0);
    h.timer(Timer::Sweep).await;
    assert!(matches!(h.claim(&again, None).await, ClaimOutcome::Admitted { .. }));
}

#[tokio::test]
async fn a_ban_only_in_the_database_stops_the_next_game() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    let mut c = h.online(3).await;
    let until = START + 3_600_000;

    // Alice, banned while she searches: paired with Carol, no game; Alice is kicked, Carol
    // searches again.
    assert_eq!(h.ask(&mut a, challenge("bob", 300, 0, true)).await, Ok(()));
    let to_bob = received_id(&b.frames());
    assert_eq!(h.ask(&mut a, queue_join("5+0", true)).await, Ok(()));
    assert_eq!(h.ask(&mut c, queue_join("5+0", true)).await, Ok(()));
    h.ban_in_store(1, until).await;
    h.clock.advance(250.0);
    h.timer(Timer::MatchTick).await;
    assert!(h.hosts.created().is_empty());
    assert_eq!(a.out.close_request().map(|r| r.code), Some(4004));
    c.frames();
    h.timer(Timer::RefreshQueues).await;
    assert_eq!(queue_states(&c.frames()), [proto::QueueState::Searching], "carol searches again");
    assert_eq!(
        h.ask(&mut b, LobbyRequest::ChallengeAccept { id: to_bob }).await,
        Err(ErrorCode::ChallengeNotFound)
    );

    // Dave, banned while idle: his next queue join or challenge is refused.
    let mut d = h.online(4).await;
    let banned_join = LobbyRequest::QueueJoin {
        category: "5+0".into(),
        rated: true,
        rating: 1500,
        provisional: false,
        ban: Some(until),
        cooldown: None,
    };
    h.request(&mut d, banned_join).await;
    assert_banned_kick(&d, until);

    // Erin, banned before she accepts Bob's challenge: no game, Bob is told she is unavailable.
    let mut e = h.online(5).await;
    assert_eq!(h.ask(&mut b, challenge("erin", 300, 0, true)).await, Ok(()));
    let to_erin = received_id(&e.frames());
    b.frames();
    h.ban_in_store(5, until).await;
    h.request(&mut e, LobbyRequest::ChallengeAccept { id: to_erin }).await;
    assert!(h.hosts.created().is_empty());
    assert_banned_kick(&e, until);
    assert_eq!(challenge_states(&b.frames()), [proto::ChallengeState::Unavailable]);

    // Frank, banned after a game: no rematch, and not cached (an unban counts at once).
    let f = h.online(6).await;
    let game = h.hosts.next_id(1);
    h.lobby.game_recovered(game, 2, 6);
    h.game_ended(game, 2, 6);
    h.ban_in_store(6, until).await;
    assert_eq!(h.rematch(game, 6, 2, true).await, Err(ErrorCode::RematchUnavailable));
    assert_eq!(f.out.close_request().map(|r| r.code), Some(4004));
    h.unban_in_store(6).await;
    let again = h.connect(6, 61);
    assert!(matches!(h.claim(&again, None).await, ClaimOutcome::Admitted { .. }));
}

#[tokio::test]
async fn a_conduct_incident_starts_a_cooldown_and_tells_the_player() {
    let h = Harness::new(&[("CONDUCT_ABANDON_LIMIT", "2")]).await;
    let mut a = h.online(1).await;
    h.lobby.conduct(1, IncidentKind::Abandon);
    h.settle().await;
    assert!(notices(&a.frames()).is_empty(), "one incident: no cooldown yet");
    h.lobby.conduct(1, IncidentKind::NoShow);
    h.settle().await;
    let until = START + 15 * 60_000;
    assert_eq!(notices(&a.frames()), [(NoticeCode::MatchmakingCooldown, until as f64)]);
    // Cached: the rated queue is refused at once.
    assert_eq!(h.ask(&mut a, queue_join("5+0", true)).await, Err(ErrorCode::MatchmakingCooldown));
    let stored = h.store.conduct().cooldown(1).await.expect("cooldown");
    assert_eq!((stored.until, stored.level), (until, 1));
}

#[tokio::test]
async fn a_revoked_session_closes_the_connection_that_used_it() {
    let h = Harness::new(&[]).await;
    let a = h.online(1).await;
    h.lobby.sessions_revoked(1, Some(vec![[9; 32]]));
    h.settle().await;
    assert!(a.out.is_open(), "another session");
    h.lobby.sessions_revoked(1, Some(vec![crate::util::sha256(b"alice")]));
    h.settle().await;
    let kick = a.out.take();
    assert_eq!(kick.close.map(|c| c.code), Some(4003));
    let frames: Vec<ServerMsg> = kick.frames.iter().map(|f| crate::realtime::testing::decode(f)).collect();
    assert_eq!(notices(&frames), [(NoticeCode::SessionRevoked, 0.0)]);
    assert!(matches!(&frames[1], ServerMsg::Error(e) if e.code == ErrorCode::Unauthorized && e.fatal));
    let b = h.online(2).await;
    h.lobby.sessions_revoked(2, None);
    h.settle().await;
    assert_eq!(b.out.close_request().map(|r| r.code), Some(4003), "every session");
}

// ---- game creation ------------------------------------------------------------------------------

#[tokio::test]
async fn a_game_created_after_the_timeout_is_cancelled() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    h.hosts.set_mode(CreateMode::Late(gate.clone()));
    assert_eq!(h.ask(&mut a, challenge("bob", 300, 0, false)).await, Ok(()));
    let id = received_id(&b.frames());
    assert_eq!(h.ask(&mut b, LobbyRequest::ChallengeAccept { id }).await, Err(ErrorCode::Internal));
    assert_eq!(challenge_states(&a.frames()), [proto::ChallengeState::Unavailable]);
    gate.notify_one();
    let cancelled = async {
        loop {
            if h.hosts.calls().iter().any(|c| matches!(c, HostCall::Cancel { .. })) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), cancelled).await.expect("the late game is cancelled");
    assert!(b.attached().is_empty());
    assert_eq!(h.ask(&mut a, queue_join("5+0", false)).await, Ok(()), "nobody is busy");
}

#[tokio::test]
async fn a_failed_host_answers_with_its_error_and_frees_the_players() {
    let h = Harness::new(&[]).await;
    let mut a = h.online(1).await;
    let mut b = h.online(2).await;
    h.hosts.set_mode(CreateMode::Fail(ErrorCode::ShuttingDown));
    let seq = h.request(&mut a, challenge("", 300, 0, false)).await;
    let code = pending_code(&a.frames(), seq);
    assert_eq!(h.ask(&mut b, LobbyRequest::ChallengeJoinCode { code }).await, Err(ErrorCode::ShuttingDown));
    assert_eq!(h.ask(&mut b, queue_join("5+0", false)).await, Ok(()));
}

// ---- refund notices -----------------------------------------------------------------------------

async fn insert_refund(store: &Store, game: GameId, victim: UserId, cheater: UserId, points: i64) {
    store
        .write(move |db| {
            db.exec(
                "INSERT OR IGNORE INTO games (id, category, rated, base_ms, inc_ms, white_id, black_id, white_name,
                 black_name, started_at, ended_at, status, reason, ply_count, moves)
                 VALUES (?1, '5+0', 1, 300000, 0, ?2, ?3, 'w', 'b', 0, 0, 1, 1, 0, x'')",
                rusqlite::params![game as i64, victim, cheater],
            )?;
            db.exec(
                "INSERT INTO rating_refunds (game_id, victim_id, cheater_id, category, points, created_at, source)
                 VALUES (?1, ?2, ?3, '5+0', ?4, 0, 'auto')",
                rusqlite::params![game as i64, victim, cheater, points],
            )?;
            Ok::<_, crate::store::StoreError>(())
        })
        .await
        .expect("refund rows");
}

#[tokio::test]
async fn a_refund_is_announced_once_to_an_idle_victim() {
    let h = Harness::new(&[]).await;
    let a = h.online(1).await;
    let _b = h.online(2).await;
    let game = h.hosts.next_id(0);
    h.lobby.game_recovered(game, 2, 3);
    insert_refund(&h.store, 11, 1, 6, 12).await;
    insert_refund(&h.store, 12, 2, 6, 7).await;
    h.lobby.refunds_pending();
    h.settle().await;
    assert_eq!(notices(&a.frames()), [(NoticeCode::RatingRestored, 12.0)]);
    h.timer(Timer::RefundPoll).await;
    assert!(a.frames().is_empty(), "once");
    let pending = h.store.refunds().pending_for(1).await.expect("pending");
    assert!(pending.ids.is_empty(), "marked notified");
    // Bob plays: told when his game ends.
    let b = &_b;
    assert!(notices(&b.frames()).is_empty());
    h.game_ended(game, 2, 3);
    h.settle().await;
    assert_eq!(notices(&b.frames()), [(NoticeCode::RatingRestored, 7.0)]);
}

#[tokio::test]
async fn a_victim_who_connects_is_told_after_the_welcome() {
    let h = Harness::new(&[]).await;
    insert_refund(&h.store, 11, 3, 6, 20).await;
    h.timer(Timer::RefundPoll).await;
    let c = h.connect(3, 30);
    let (reply, answer) = tokio::sync::oneshot::channel();
    h.lobby.post(LobbyMsg::Claim { link: c.link.clone(), ban: None, reply });
    assert_eq!(answer.await.expect("answer"), ClaimOutcome::Admitted { active_game: 0 });
    // Not welcomed yet: the retry finds the Welcome missing and tries again later.
    h.clock.advance(250.0);
    h.timer(Timer::RefundRetries).await;
    assert!(c.frames().is_empty());
    c.link.set_welcomed();
    h.clock.advance(250.0);
    h.timer(Timer::RefundRetries).await;
    assert_eq!(notices(&c.frames()), [(NoticeCode::RatingRestored, 20.0)]);
}
